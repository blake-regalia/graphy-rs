//! `Store` / `Snapshot` (doc 02 §8, doc 07 §2–§5): the read-side public
//! surface plus the M4 write path. A `Store` holds the current [`Snapshot`]
//! behind an `ArcSwap`; the single-writer [`Store::apply`] resolves terms,
//! records epoch-stamped delta events, and publishes the next snapshot
//! atomically — in-flight readers keep the epoch they acquired.
//!
//! A snapshot is `(base segment, delta view, epoch)`; [`Snapshot::scan`]
//! returns the **snapshot-level `QuadScan`** — the storage↔engine seam —
//! which zips the base [`SegmentScan`] with the delta's matching range in
//! the same ordering, eliding tombstones.
//!
//! [`Store::merge_with`] (doc 07 §6, M5) folds the delta into generation
//! G+1: generations live in `gen-NNNNNN/` directories under the store root
//! with a `CURRENT` pointer (generation 0 sits at the root itself), the WAL
//! rotates down to a `Checkpoint`, and old generations retire when their
//! last snapshot drops.
//!
//! `Snapshot` is also where public [`TermId`]s live: dictionary references
//! with **1-based** section ordinals (docs/08 §2; `TermId::NULL` aliases
//! `(Shared, 0)`), inline values, and triple-term ordinals. Overlay terms
//! continue each section's ordinal range past the base counts.

use std::collections::{BTreeMap, HashMap};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, Weak};

use arc_swap::ArcSwap;
use graphy_core::{Section, TermId};

use crate::bt::Order;
use crate::builder::{validate_quad, BuilderConfig, Profile};
use crate::delta::{BaseRanges, Delta, Kind as DeltaKind, QuadKey};
use crate::format::StoreError;
use crate::manifest::MANIFEST_NAME;
use crate::scan::{QuadBatch, SegmentScan};
use crate::segment::{OpenMode, Pattern, Segment, TermPos};
use crate::wal::{self, replay, Wal, WalOp};

pub use crate::wal::Durability;

/// Pointer file naming the live generation's segment directory (doc 02 §6).
/// Absent until the first merge: bulk loads write generation 0's segment at
/// the store root.
pub const CURRENT_NAME: &str = "CURRENT";
const CURRENT_TMP_NAME: &str = "CURRENT.tmp";

/// Segment-directory names the store's own components never use, so
/// generation directories can be recognized as such.
const GEN_PREFIX: &str = "gen-";

/// Resolve the live segment directory of a store rooted at `dir`: the
/// `CURRENT` pointer's target after a merge, else `dir` itself. Tooling that
/// opens segments directly (`verify`, `export`) must resolve through this —
/// a retired root segment may linger (unswept) next to a `CURRENT` pointer.
pub fn resolve_segment_dir(dir: &Path) -> Result<PathBuf, StoreError> {
    let cur = dir.join(CURRENT_NAME);
    match std::fs::read_to_string(&cur) {
        Ok(name) => {
            let name = name.trim();
            if name.is_empty() || !name.starts_with(GEN_PREFIX) || name.contains(['/', '\\']) {
                return Err(StoreError::Corrupt(format!(
                    "{}: invalid segment pointer {name:?}",
                    cur.display()
                )));
            }
            Ok(dir.join(name))
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(dir.to_owned()),
        Err(e) => Err(StoreError::io(&cur, e)),
    }
}

/// Remove crash debris a previous process may have left: a staged WAL
/// rotation, a `CURRENT` staging file, generation directories other than the
/// live one (failed builds or retired-but-unswept generations), and — once
/// `CURRENT` points away from the root — the retired root segment's
/// components. Best-effort; called only after the live segment opened.
fn clean_debris(dir: &Path, seg_dir: &Path) {
    std::fs::remove_file(dir.join(wal::WAL_TMP_NAME)).ok();
    std::fs::remove_file(dir.join(CURRENT_TMP_NAME)).ok();
    if let Ok(rd) = std::fs::read_dir(dir) {
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() && e.file_name().to_string_lossy().starts_with(GEN_PREFIX) && p != seg_dir
            {
                std::fs::remove_dir_all(&p).ok();
            }
        }
    }
    if seg_dir != dir && dir.join(MANIFEST_NAME).exists() {
        for sub in ["dict", "idx", "graphs", "stats"] {
            std::fs::remove_dir_all(dir.join(sub)).ok();
        }
        std::fs::remove_file(dir.join(MANIFEST_NAME)).ok();
    }
}

/// Deterministic fault injection over the merge fsync schedule
/// (doc 07 §8.2): aborts the process — no unwinding, no buffer flushes,
/// like a kill — when `GRAPHY_FAILPOINT` names this point. Test-only
/// scaffolding driven by tests/merge_fault.rs; one lazily-cached env read,
/// free when unset.
fn failpoint(name: &str) {
    static POINT: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
    let armed = POINT.get_or_init(|| std::env::var("GRAPHY_FAILPOINT").ok());
    if armed.as_deref() == Some(name) {
        std::process::abort();
    }
}

/// Write the `CURRENT` pointer via the atomic-rename recipe.
fn write_current(dir: &Path, name: &str) -> Result<(), StoreError> {
    use std::io::Write as _;
    let tmp = dir.join(CURRENT_TMP_NAME);
    let path = dir.join(CURRENT_NAME);
    let run = || -> io::Result<()> {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(name.as_bytes())?;
        f.write_all(b"\n")?;
        f.sync_all()?;
        std::fs::rename(&tmp, &path)?;
        std::fs::File::open(dir)?.sync_all()?;
        Ok(())
    };
    run().map_err(|e| StoreError::io(&path, e))
}

/// Default delta budget (doc 07 §3; events ≈ quads, sized well under the
/// 64 GB–2 TB envelope — tune per deployment via `set_delta_budget`).
const DEFAULT_SOFT_BUDGET: u64 = 2_000_000;
const DEFAULT_HARD_BUDGET: u64 = 8_000_000;

/// Default minimum event growth between ephemeral compactions
/// (see [`Store::ephemeral_compaction_due`]).
const DEFAULT_COMPACT_MIN: u64 = 65_536;

/// One quad of concise-encoded terms (`None` graph = default graph).
pub type QuadTerms<'a> = (&'a [u8], &'a [u8], &'a [u8], Option<&'a [u8]>);

/// Knobs for [`Store::merge_with`] (doc 07 §6.2: merge memory is bounded by
/// configuration, not data size — pacing knobs arrive with the scheduler).
#[derive(Debug, Clone)]
pub struct MergeConfig {
    /// External-sort buffer budget for the rebuild, in bytes.
    pub sort_budget: usize,
    /// Rebuild into this profile instead of the base's (the doc 07 §6.4
    /// profile-change trigger, explicit only — `graphy compact
    /// --profile`). `None` keeps the base profile AND its current
    /// materialized-ordering set (lazily added orderings survive);
    /// `Some` switches to the new profile's default orderings.
    pub profile: Option<Profile>,
    /// Duty-cycle cap on the fold (doc 07 §6.4 pacing): the merge's scan/
    /// build loop sleeps so its share of wall time stays ≈ this fraction,
    /// bounding its disturbance of foreground reads. `None` = unpaced
    /// (fastest merge; measured +39% on concurrent bound-p scans —
    /// BENCHMARKS.md M5). Clamped to `(0, 1]`. The read-p99 feedback loop
    /// that *adjusts* the duty arrives with the SLO runs, when a query
    /// engine drives realistic reads.
    pub pace_duty: Option<f64>,
}

impl Default for MergeConfig {
    fn default() -> MergeConfig {
        MergeConfig {
            sort_budget: 256 << 20,
            profile: None,
            pace_duty: None,
        }
    }
}

/// Timings and sizes of the most recent merge (doc 07 §6 observability;
/// the swap duration is the commit-visible pause — writers queue behind it
/// via the leadership handoff, so `swap` is what the §6.3 <10 ms target
/// measures).
#[derive(Debug, Clone, Copy)]
pub struct MergeStats {
    /// Quads streamed into the new generation.
    pub folded_quads: u64,
    /// Active-suffix events remapped during the swap.
    pub suffix_events: u64,
    /// Fold duration (concurrent with commits; not a pause).
    pub build: std::time::Duration,
    /// Exclusive swap duration: leadership acquired → snapshot published.
    pub swap: std::time::Duration,
}

/// The store handle: current snapshot behind an atomic pointer, plus the
/// single-writer commit path.
#[derive(Debug)]
pub struct Store {
    dir: PathBuf,
    mode: OpenMode,
    current: ArcSwap<Snapshot>,
    /// Group-commit queue (doc 07 §5): one leader at a time drains waiting
    /// transactions and pays a single fsync for the whole group. The merge
    /// swap acquires the same leadership, so publishes and generation swaps
    /// are mutually exclusive.
    writer: Mutex<WriterQueue>,
    /// Signalled when leadership is released (a would-be leader or the
    /// merge swap may be waiting).
    leader_cv: Condvar,
    /// Write-ahead log (doc 07 §4); only the group leader touches it.
    wal: Mutex<Wal>,
    /// Delta budget (doc 07 §3, events ≈ quads): crossing `soft` sets the
    /// sticky merge signal; at `hard` commits fail until a merge relieves
    /// the pressure.
    budget_soft: AtomicU64,
    budget_hard: AtomicU64,
    merge_needed: AtomicBool,
    /// Live-snapshot epoch registry (doc 07 §2): every snapshot pins its
    /// epoch for its lifetime; the minimum pinned epoch is the GC floor.
    pins: Arc<EpochPins>,
    /// Held for the duration of a merge — one merge at a time.
    merge_lock: Mutex<()>,
    /// Live segment directory (changes at each generation swap).
    seg_dir: Mutex<PathBuf>,
    /// Generations awaiting retirement: unlinked once their last snapshot
    /// drops (doc 07 §6.1 step 4; swept by [`Store::gc`] and merges).
    retired: Mutex<Vec<Retired>>,
    /// Set when a merge swap failed after its point of no return (the
    /// durable state is consistent but this handle's view is not) — every
    /// further write fails; reopen the store.
    poisoned: AtomicBool,
    /// A background merger is attached (doc 07 §6.4): hard-budget pressure
    /// becomes wait-for-merge backpressure instead of a commit failure.
    merger: AtomicBool,
    /// Backpressure gate: writers over the hard budget wait here; signalled
    /// whenever delta pressure can have dropped (merge publish, GC, budget
    /// raise, merger detach).
    gate_lock: Mutex<()>,
    gate: Condvar,
    /// Backpressure wait ceiling, milliseconds (doc 07 §6.4 predictable
    /// degradation: block briefly, then fail loudly).
    backpressure_ms: AtomicU64,
    /// Timings/sizes of the most recent completed merge.
    merge_stats: Mutex<Option<MergeStats>>,
    /// Ephemeral compaction pacing (docs/11): delta event count right after
    /// the last [`Store::compact_ephemeral`] (0 = never compacted) and the
    /// minimum growth above it before another compaction is due. Unused on
    /// directory-backed stores (the merger folds their deltas).
    compact_floor: AtomicU64,
    compact_min: AtomicU64,
}

/// Default [`Store::set_backpressure_timeout`] (generous: a healthy merge
/// of a budget-sized delta completes well inside this).
const DEFAULT_BACKPRESSURE_MS: u64 = 60_000;

/// One retired generation: its files go once the last reader drops.
#[derive(Debug)]
struct Retired {
    seg: Weak<Segment>,
    path: PathBuf,
    /// Root-resident generation 0: unlink its components, not the store dir.
    at_root: bool,
}

/// Refcounted registry of epochs pinned by live snapshots. The RwLock-
/// baseline stand-in for doc 07 §2's crossbeam-epoch reclamation: readers
/// pin on snapshot acquisition (via the snapshot's own lifetime), and
/// [`Store::gc`] compacts delta events no pinned epoch can observe.
#[derive(Debug, Default)]
struct EpochPins {
    inner: Mutex<BTreeMap<u64, usize>>,
}

impl EpochPins {
    fn pin(self: &Arc<Self>, epoch: u64) -> EpochPin {
        *self
            .inner
            .lock()
            .expect("epoch pins")
            .entry(epoch)
            .or_insert(0) += 1;
        EpochPin {
            pins: Arc::clone(self),
            epoch,
        }
    }

    /// The oldest epoch any live snapshot observes.
    fn min(&self) -> Option<u64> {
        self.inner
            .lock()
            .expect("epoch pins")
            .first_key_value()
            .map(|(&e, _)| e)
    }
}

/// One snapshot's registration in [`EpochPins`]; dropping it (with the
/// snapshot) releases the epoch.
#[derive(Debug)]
struct EpochPin {
    pins: Arc<EpochPins>,
    epoch: u64,
}

impl Drop for EpochPin {
    fn drop(&mut self) {
        let mut g = self.pins.inner.lock().expect("epoch pins");
        let n = g.get_mut(&self.epoch).expect("pinned epoch registered");
        *n -= 1;
        if *n == 0 {
            g.remove(&self.epoch);
        }
    }
}

/// A follower's queued transaction (owned — the caller blocks on `slot`
/// while the leader processes it).
#[derive(Debug)]
struct Pending {
    dels: Vec<OwnedQuad>,
    adds: Vec<OwnedQuad>,
    durability: Durability,
    slot: Arc<Slot>,
}

type OwnedQuad = (Vec<u8>, Vec<u8>, Vec<u8>, Option<Vec<u8>>);

#[derive(Debug, Default)]
struct WriterQueue {
    pending: Vec<Pending>,
    leader: bool,
    /// A merge swap is waiting for leadership. Commits arriving meanwhile
    /// must queue as followers instead of barging into leadership (the
    /// condvar waiter always loses that race), or sustained write load
    /// starves the swap indefinitely; the swap drains them after.
    swap_waiting: bool,
}

/// Completion slot a follower waits on.
#[derive(Debug, Default)]
struct Slot {
    done: Mutex<Option<Result<Arc<Snapshot>, StoreError>>>,
    cv: Condvar,
}

impl Slot {
    fn fill(&self, r: Result<Arc<Snapshot>, StoreError>) {
        *self.done.lock().expect("slot lock") = Some(r);
        self.cv.notify_all();
    }

    fn wait(&self) -> Result<Arc<Snapshot>, StoreError> {
        let mut g = self.done.lock().expect("slot lock");
        loop {
            match g.take() {
                Some(r) => return r,
                None => g = self.cv.wait(g).expect("slot lock"),
            }
        }
    }
}

impl Store {
    /// Open in [`OpenMode::Heap`].
    pub fn open(dir: &Path) -> Result<Store, StoreError> {
        Store::open_with(dir, OpenMode::Heap)
    }

    /// An in-memory, non-durable store (docs/11 M12a): the base is the
    /// embedded empty segment, the WAL is a null sink, and every quad lives
    /// in the delta. `merge`/`compact`/`add_ordering` are unavailable
    /// ([`StoreError::Ephemeral`]); epoch GC is the relief valve for churn.
    /// Suited to browser/wasm and scratch use — not persistence, and not
    /// 10⁸-scale data (the delta holds ~6 index entries per quad).
    pub fn ephemeral() -> Result<Store, StoreError> {
        let base = Arc::new(Segment::open_embedded(crate::segment::EMPTY_SEGMENT)?);
        Store::from_ephemeral_base(base)
    }

    /// An ephemeral store over an arbitrary segment byte image (docs/11 §6
    /// "scale"): the base is a real segment — typically built natively with
    /// `graphy load` and fetched or OPFS-loaded in the browser — the delta
    /// overlays writes, and the WAL runs in capture mode with `log`
    /// replayed, so edits over a published dataset persist separately from
    /// the (immutable) image.
    pub fn open_image(files: &[(&str, &[u8])], log: Option<&[u8]>) -> Result<Store, StoreError> {
        let base = Arc::new(Segment::open_embedded(files)?);
        let store = Store::from_ephemeral_base(base)?;
        store.enable_capture(log)?;
        Ok(store)
    }

    fn from_ephemeral_base(base: Arc<Segment>) -> Result<Store, StoreError> {
        let c = &base.manifest.counts;
        let delta = Arc::new(Delta::new(
            &base.scan_orders(),
            BaseRanges {
                subjects: c.shared + c.subjects,
                predicates: c.predicates,
                objects: c.shared + c.objects,
                graphs: c.graphs + 1,
            },
        ));
        let generation = base.manifest.generation;
        let pins: Arc<EpochPins> = Arc::default();
        let snapshot = Snapshot {
            base,
            delta,
            generation,
            epoch: 0,
            _pin: pins.pin(0),
        };
        Ok(Store {
            dir: PathBuf::new(),
            mode: OpenMode::Heap,
            current: ArcSwap::from_pointee(snapshot),
            writer: Mutex::new(WriterQueue::default()),
            leader_cv: Condvar::new(),
            wal: Mutex::new(Wal::null()),
            budget_soft: AtomicU64::new(DEFAULT_SOFT_BUDGET),
            budget_hard: AtomicU64::new(DEFAULT_HARD_BUDGET),
            merge_needed: AtomicBool::new(false),
            pins,
            merge_lock: Mutex::new(()),
            seg_dir: Mutex::new(PathBuf::new()),
            retired: Mutex::new(Vec::new()),
            poisoned: AtomicBool::new(false),
            merger: AtomicBool::new(false),
            gate_lock: Mutex::new(()),
            gate: Condvar::new(),
            backpressure_ms: AtomicU64::new(DEFAULT_BACKPRESSURE_MS),
            merge_stats: Mutex::new(None),
            compact_floor: AtomicU64::new(0),
            compact_min: AtomicU64::new(DEFAULT_COMPACT_MIN),
        })
    }

    /// [`Store::ephemeral`] with persistence hooks (docs/11 OPFS): the WAL
    /// runs in capture mode — every committed group's frames accumulate for
    /// [`Store::drain_wal_capture`], which the host appends to durable
    /// storage; `log` replays a previously captured image through the same
    /// recovery path as a directory-backed open (torn tails truncate).
    pub fn ephemeral_persistent(log: Option<&[u8]>) -> Result<Store, StoreError> {
        Store::ephemeral_persistent_recovering(log).map(|(store, _)| store)
    }

    /// [`Store::ephemeral_persistent`] that also reports the byte length of
    /// the image's valid prefix. When it is shorter than the image, the
    /// tail was torn (or foreign) and did not replay — the host should
    /// truncate its durable log to this length before appending, or frames
    /// written after the tear are unreachable on the next restore.
    pub fn ephemeral_persistent_recovering(log: Option<&[u8]>) -> Result<(Store, u64), StoreError> {
        let store = Store::ephemeral()?;
        let consumed = store.enable_capture(log)?;
        Ok((store, consumed))
    }

    /// [`Store::ephemeral_persistent`], but the image must be one valid
    /// frame run end to end: a torn or foreign tail is an error instead of
    /// a truncation point. For restoring *foreign* images (user imports),
    /// where silently dropping bytes would masquerade as success — a
    /// store's own durable log keeps the lenient path, since a crash
    /// mid-append leaves a torn tail as a matter of course.
    pub fn ephemeral_persistent_strict(log: Option<&[u8]>) -> Result<Store, StoreError> {
        let store = Store::ephemeral()?;
        let consumed = store.enable_capture(log)?;
        let total = log.map_or(0, |b| b.len() as u64);
        if consumed != total {
            return Err(StoreError::Corrupt(format!(
                "captured WAL image: invalid frame run after byte {consumed} (of {total})"
            )));
        }
        Ok(store)
    }

    /// Switch the WAL to capture mode and replay a previously captured log.
    /// Returns the byte length of the image's valid prefix (0 without one).
    fn enable_capture(&self, log: Option<&[u8]>) -> Result<u64, StoreError> {
        let store = self;
        {
            let mut w = store.wal.lock().expect("wal lock");
            *w = Wal::capture();
        }
        let Some(bytes) = log else { return Ok(0) };
        {
            let snapshot = store.current.load_full();
            let base = Arc::clone(&snapshot.base);
            let delta = Arc::clone(&snapshot.delta);
            let generation = snapshot.generation;
            let pins = Arc::clone(&store.pins);
            let mut epoch = 0u64;
            let replayed = wal::replay_bytes(bytes, "<captured wal>", |tx_epoch, ops| {
                if tx_epoch <= epoch {
                    return Err(StoreError::Corrupt(format!(
                        "captured WAL epochs not increasing ({tx_epoch} after {epoch})"
                    )));
                }
                let snap = Snapshot {
                    base: Arc::clone(&base),
                    delta: Arc::clone(&delta),
                    generation,
                    epoch,
                    _pin: pins.pin(epoch),
                };
                let dels: Vec<QuadTerms<'_>> = ops
                    .iter()
                    .filter(|o| o.del)
                    .map(|o| {
                        (
                            o.s.as_slice(),
                            o.p.as_slice(),
                            o.o.as_slice(),
                            o.g.as_deref(),
                        )
                    })
                    .collect();
                let adds: Vec<QuadTerms<'_>> = ops
                    .iter()
                    .filter(|o| !o.del)
                    .map(|o| {
                        (
                            o.s.as_slice(),
                            o.p.as_slice(),
                            o.o.as_slice(),
                            o.g.as_deref(),
                        )
                    })
                    .collect();
                let mut carry = HashMap::new();
                let (entries, _) = commit_core(&snap, &dels, &adds, &mut carry)?;
                delta.record(&entries, tx_epoch);
                epoch = tx_epoch;
                Ok(())
            })?;
            let restored = Snapshot {
                base,
                delta,
                generation,
                epoch,
                _pin: pins.pin(epoch),
            };
            store.current.store(Arc::new(restored));
            Ok(replayed.valid_len)
        }
    }

    /// Committed WAL frames captured since the last drain (capture-mode
    /// stores; empty otherwise). Append these bytes — in order, verbatim —
    /// to the durable log; feed the whole log back through
    /// [`Store::ephemeral_persistent`] to restore.
    pub fn drain_wal_capture(&self) -> Vec<u8> {
        self.wal.lock().expect("wal lock").take_captured()
    }

    /// The whole current dataset as a single-transaction WAL image (log
    /// compaction: atomically replace the durable log with this). Emitted at
    /// the snapshot's epoch, so frames captured *afterwards* keep replaying
    /// in order; empty stores yield an empty image.
    pub fn pack_log(&self) -> Result<Vec<u8>, StoreError> {
        let snap = self.snapshot();
        let Some(pat) = snap.resolve_pattern(None, None, None, None) else {
            return Ok(Vec::new());
        };
        let mut ops = Vec::new();
        let mut scan = snap.scan(&pat, crate::segment::Order::Spo)?;
        let mut batch = crate::scan::QuadBatch::new();
        while scan.next_batch(&mut batch)? {
            for i in 0..batch.len() {
                ops.push(wal::WalOp {
                    del: false,
                    s: snap.decode_value(batch.s[i], crate::segment::TermPos::Subject)?,
                    p: snap.decode_value(batch.p[i], crate::segment::TermPos::Predicate)?,
                    o: snap.decode_value(batch.o[i], crate::segment::TermPos::Object)?,
                    g: (batch.g[i] > 0)
                        .then(|| snap.decode_value(batch.g[i], crate::segment::TermPos::Graph))
                        .transpose()?,
                });
            }
        }
        if ops.is_empty() {
            return Ok(Vec::new());
        }
        let mut w = Wal::capture();
        w.append_tx(snap.epoch().max(1), &ops);
        w.commit_group(Durability::Relaxed)?;
        Ok(w.take_captured())
    }

    /// True for [`Store::ephemeral`] stores (no backing directory).
    pub fn is_ephemeral(&self) -> bool {
        self.dir.as_os_str().is_empty()
    }

    fn check_ephemeral(&self, what: &'static str) -> Result<(), StoreError> {
        if self.is_ephemeral() {
            return Err(StoreError::Ephemeral(what));
        }
        Ok(())
    }

    pub fn open_with(dir: &Path, mode: OpenMode) -> Result<Store, StoreError> {
        let seg_dir = resolve_segment_dir(dir)?;
        let base = Arc::new(Segment::open_with(&seg_dir, mode)?);
        clean_debris(dir, &seg_dir);
        let c = &base.manifest.counts;
        let delta = Arc::new(Delta::new(
            &base.scan_orders(),
            BaseRanges {
                subjects: c.shared + c.subjects,
                predicates: c.predicates,
                objects: c.shared + c.objects,
                graphs: c.graphs + 1,
            },
        ));
        let generation = base.manifest.generation;
        let pins: Arc<EpochPins> = Arc::default();

        // Recovery (doc 07 §4): replay committed WAL transactions into the
        // fresh delta, then continue appending after the valid prefix (any
        // torn tail is truncated). Replay re-runs the same commit core as
        // `apply`, so it is deterministic and idempotent.
        let mut epoch = 0u64;
        let replayed = replay(dir, |tx_epoch, ops| {
            if tx_epoch <= epoch {
                return Err(StoreError::Corrupt(format!(
                    "WAL epochs not increasing ({tx_epoch} after {epoch})"
                )));
            }
            let snap = Snapshot {
                base: Arc::clone(&base),
                delta: Arc::clone(&delta),
                generation,
                epoch,
                _pin: pins.pin(epoch),
            };
            let dels: Vec<QuadTerms<'_>> = ops
                .iter()
                .filter(|o| o.del)
                .map(|o| {
                    (
                        o.s.as_slice(),
                        o.p.as_slice(),
                        o.o.as_slice(),
                        o.g.as_deref(),
                    )
                })
                .collect();
            let adds: Vec<QuadTerms<'_>> = ops
                .iter()
                .filter(|o| !o.del)
                .map(|o| {
                    (
                        o.s.as_slice(),
                        o.p.as_slice(),
                        o.o.as_slice(),
                        o.g.as_deref(),
                    )
                })
                .collect();
            let mut carry = HashMap::new();
            let (entries, _) = commit_core(&snap, &dels, &adds, &mut carry)?;
            delta.record(&entries, tx_epoch);
            epoch = tx_epoch;
            Ok(())
        })?;
        let wal = Wal::open_append(dir, replayed.valid_len)?;
        // A rotated log restores the epoch floor through its checkpoint even
        // when no transactions follow it — epochs never regress.
        epoch = epoch.max(replayed.checkpoint_epoch);

        let snapshot = Snapshot {
            base,
            delta,
            generation,
            epoch,
            _pin: pins.pin(epoch),
        };
        Ok(Store {
            dir: dir.to_owned(),
            mode,
            current: ArcSwap::from_pointee(snapshot),
            writer: Mutex::new(WriterQueue::default()),
            leader_cv: Condvar::new(),
            wal: Mutex::new(wal),
            budget_soft: AtomicU64::new(DEFAULT_SOFT_BUDGET),
            budget_hard: AtomicU64::new(DEFAULT_HARD_BUDGET),
            merge_needed: AtomicBool::new(false),
            pins,
            merge_lock: Mutex::new(()),
            seg_dir: Mutex::new(seg_dir),
            retired: Mutex::new(Vec::new()),
            poisoned: AtomicBool::new(false),
            merger: AtomicBool::new(false),
            gate_lock: Mutex::new(()),
            gate: Condvar::new(),
            backpressure_ms: AtomicU64::new(DEFAULT_BACKPRESSURE_MS),
            merge_stats: Mutex::new(None),
            compact_floor: AtomicU64::new(0),
            compact_min: AtomicU64::new(DEFAULT_COMPACT_MIN),
        })
    }

    /// The current snapshot; callers keep it valid for as long as they hold
    /// the `Arc`, across any concurrent publishes.
    pub fn snapshot(&self) -> Arc<Snapshot> {
        self.current.load_full()
    }

    /// Apply one commit with [`Durability::Strict`]: `dels` then `adds`
    /// (SPARQL Update operation order), WAL-logged and fsynced before the
    /// delta applies, atomically visible at the returned snapshot's epoch.
    /// Terms are concise-encoded and validated (public trust boundary).
    /// Deletes of absent quads and adds of present ones are set-semantics
    /// no-ops; a commit with no effect writes nothing and keeps the epoch.
    /// Single-writer: concurrent calls serialize.
    pub fn apply(
        &self,
        dels: &[QuadTerms<'_>],
        adds: &[QuadTerms<'_>],
    ) -> Result<Arc<Snapshot>, StoreError> {
        self.apply_with(dels, adds, Durability::Strict)
    }

    /// [`Store::apply`] with an explicit durability mode (doc 07 §4:
    /// `Relaxed` skips the per-commit fsync for bulk ingestion — a crash
    /// may lose the OS-buffered tail, never corrupt it).
    ///
    /// Group commit (doc 07 §5): concurrent callers form a group — one
    /// leader plans every queued transaction (each seeing the effects of
    /// the ones before it), writes them to the WAL, and pays **one fsync**
    /// for the group (the strictest durability requested wins); followers
    /// block until their transaction's snapshot is ready. An uncontended
    /// caller takes the same path solo.
    pub fn apply_with(
        &self,
        dels: &[QuadTerms<'_>],
        adds: &[QuadTerms<'_>],
        durability: Durability,
    ) -> Result<Arc<Snapshot>, StoreError> {
        self.check_poisoned()?;
        self.wait_for_capacity()?;
        {
            let mut q = self.writer.lock().expect("writer queue");
            if q.leader || q.swap_waiting {
                // A leader is active (or a merge swap has priority):
                // enqueue (owned) and wait.
                let own = |v: &[QuadTerms<'_>]| -> Vec<OwnedQuad> {
                    v.iter()
                        .map(|&(s, p, o, g)| {
                            (s.to_vec(), p.to_vec(), o.to_vec(), g.map(<[u8]>::to_vec))
                        })
                        .collect()
                };
                let slot = Arc::new(Slot::default());
                q.pending.push(Pending {
                    dels: own(dels),
                    adds: own(adds),
                    durability,
                    slot: Arc::clone(&slot),
                });
                drop(q);
                return slot.wait();
            }
            q.leader = true;
        }

        // Leader: first group includes this call's (borrowed) transaction
        // plus anything queued while it waited; stragglers drain after.
        let group = {
            let mut q = self.writer.lock().expect("writer queue");
            std::mem::take(&mut q.pending)
        };
        let own = self.run_group(Some((dels, adds, durability)), group);
        self.drain_leadership();
        own.expect("leader processed its own transaction")
    }

    /// Drain queued transactions group by group, then release leadership
    /// (waking any waiting would-be leader or merge swap).
    fn drain_leadership(&self) {
        loop {
            let group = {
                let mut q = self.writer.lock().expect("writer queue");
                // Yield to a waiting merge swap even with commits queued —
                // under sustained load the queue refills during every group
                // fsync, so an unconditional drain would hold leadership
                // (and defer the swap) indefinitely. The swap drains the
                // queue itself right after (its own drain runs with
                // swap_waiting already cleared).
                if q.pending.is_empty() || q.swap_waiting {
                    q.leader = false;
                    self.leader_cv.notify_all();
                    return;
                }
                std::mem::take(&mut q.pending)
            };
            self.run_group(None, group);
        }
    }

    /// Block until group-commit leadership is free and take it — the merge
    /// swap's mutual exclusion: while held, no commit plans against the old
    /// generation and no publish races the generation swap. Followers that
    /// enqueue meanwhile are drained by [`Store::drain_leadership`] after.
    fn acquire_leadership(&self) {
        let mut q = self.writer.lock().expect("writer queue");
        // Priority flag: set under the same lock as every leadership check,
        // so no commit can barge in after this point (they enqueue as
        // followers; the swap's drain processes them). One merge at a time
        // (merge_lock), so a plain bool suffices.
        q.swap_waiting = true;
        while q.leader {
            q = self.leader_cv.wait(q).expect("writer queue");
        }
        q.swap_waiting = false;
        q.leader = true;
    }

    fn check_poisoned(&self) -> Result<(), StoreError> {
        if self.poisoned.load(Ordering::Relaxed) {
            return Err(StoreError::Corrupt(
                "store handle poisoned by a failed merge swap — reopen the store \
                 (durable state is consistent)"
                    .into(),
            ));
        }
        Ok(())
    }

    /// Plan, log, fsync (once), record, and publish one commit group.
    /// Returns the leader's own result when its transaction is in the group.
    fn run_group(
        &self,
        mine: Option<(&[QuadTerms<'_>], &[QuadTerms<'_>], Durability)>,
        group: Vec<Pending>,
    ) -> Option<Result<Arc<Snapshot>, StoreError>> {
        let start = self.snapshot();
        let hard = self.budget_hard.load(Ordering::Relaxed);
        let mut events = start.delta.events();

        // Plan every transaction in order; `carry` makes each one see the
        // effects of the ones before it in this group (their delta records
        // land only after the group fsync).
        let mut carry: HashMap<QuadKey, DeltaKind> = HashMap::new();
        let mut epoch = start.epoch;
        let mut planned: Vec<(PlannedTx, Option<Arc<Slot>>)> = Vec::new();
        let mut own_result: Option<Result<Arc<Snapshot>, StoreError>> = None;
        let mut strictest = Durability::Relaxed;
        {
            let mut wal = self.wal.lock().expect("wal lock");
            let mut plan_one = |dels: &[QuadTerms<'_>],
                                adds: &[QuadTerms<'_>],
                                durability: Durability,
                                wal: &mut Wal|
             -> Result<Option<PlannedTx>, StoreError> {
                if events >= hard && !self.merger.load(Ordering::Relaxed) {
                    // With a merger attached the entry gate enforces the
                    // budget; a group already past the gate may overshoot
                    // by at most its own size, never fail here.
                    return Err(StoreError::Corrupt(format!(
                        "delta budget exhausted ({events} events ≥ hard limit {hard}); \
                         merge required"
                    )));
                }
                let (entries, wal_ops) = commit_core(&start, dels, adds, &mut carry)?;
                if entries.is_empty() {
                    return Ok(None);
                }
                epoch += 1;
                if durability == Durability::Strict {
                    strictest = Durability::Strict;
                }
                events += entries.len() as u64;
                // Encode straight from the caller's borrowed terms.
                wal.append_tx_terms(
                    epoch,
                    wal_ops.iter().map(|&(del, i)| {
                        let (s, p, o, g) = if del { dels[i] } else { adds[i] };
                        (del, s, p, o, g)
                    }),
                );
                Ok(Some((entries, epoch)))
            };

            if let Some((dels, adds, durability)) = mine {
                match plan_one(dels, adds, durability, &mut wal) {
                    Ok(Some(tx)) => planned.push((tx, None)),
                    Ok(None) => own_result = Some(Ok(Arc::clone(&start))),
                    Err(e) => own_result = Some(Err(e)),
                }
            }
            for tx in &group {
                let dels: Vec<QuadTerms<'_>> = tx
                    .dels
                    .iter()
                    .map(|q| {
                        (
                            q.0.as_slice(),
                            q.1.as_slice(),
                            q.2.as_slice(),
                            q.3.as_deref(),
                        )
                    })
                    .collect();
                let adds: Vec<QuadTerms<'_>> = tx
                    .adds
                    .iter()
                    .map(|q| {
                        (
                            q.0.as_slice(),
                            q.1.as_slice(),
                            q.2.as_slice(),
                            q.3.as_deref(),
                        )
                    })
                    .collect();
                match plan_one(&dels, &adds, tx.durability, &mut wal) {
                    Ok(Some(p)) => planned.push((p, Some(Arc::clone(&tx.slot)))),
                    Ok(None) => tx.slot.fill(Ok(self.snapshot())),
                    Err(e) => tx.slot.fill(Err(e)),
                }
            }

            // One fsync for the whole group (doc 07 §4).
            if let Err(e) = wal.commit_group(strictest) {
                let msg = e.to_string();
                for (_, slot) in planned.drain(..) {
                    let err = StoreError::Corrupt(msg.clone());
                    match slot {
                        Some(slot) => slot.fill(Err(err)),
                        None => own_result = Some(Err(StoreError::Corrupt(msg.clone()))),
                    }
                }
                return own_result;
            }
        }

        // Durable: record and publish in epoch order.
        let mut last: Option<Arc<Snapshot>> = None;
        for ((entries, epoch), slot) in planned {
            start.delta.record(&entries, epoch);
            let snap = Arc::new(Snapshot {
                base: Arc::clone(&start.base),
                delta: Arc::clone(&start.delta),
                generation: start.generation,
                epoch,
                _pin: self.pins.pin(epoch),
            });
            match slot {
                Some(slot) => slot.fill(Ok(Arc::clone(&snap))),
                None => own_result = Some(Ok(Arc::clone(&snap))),
            }
            last = Some(snap);
        }
        if let Some(snap) = last {
            if snap.delta.events() > self.budget_soft.load(Ordering::Relaxed) {
                self.merge_needed.store(true, Ordering::Relaxed);
            }
            self.current.store(snap);
        }
        own_result
    }

    /// Backpressure gate (doc 07 §6.4): with a merger attached, a writer
    /// over the hard budget waits for a merge to fold the delta away
    /// instead of failing — predictable degradation to slower writes. The
    /// caller holds no locks or leadership here, so the merge swap it is
    /// waiting on can always proceed. Without a merger the old fail-fast
    /// behavior stands. Waits are bounded by
    /// [`Store::set_backpressure_timeout`].
    fn wait_for_capacity(&self) -> Result<(), StoreError> {
        let over = |hard: u64| self.current.load().delta.events() >= hard;
        if !over(self.budget_hard.load(Ordering::Relaxed)) {
            return Ok(());
        }
        let fail_fast = |events: u64, hard: u64| {
            Err(StoreError::Corrupt(format!(
                "delta budget exhausted ({events} events ≥ hard limit {hard}); \
                 merge required"
            )))
        };
        let deadline = std::time::Instant::now()
            + std::time::Duration::from_millis(self.backpressure_ms.load(Ordering::Relaxed));
        let mut g = self.gate_lock.lock().expect("gate lock");
        loop {
            let hard = self.budget_hard.load(Ordering::Relaxed);
            let events = self.current.load().delta.events();
            if events < hard {
                return Ok(());
            }
            if !self.merger.load(Ordering::Relaxed) {
                return fail_fast(events, hard);
            }
            let now = std::time::Instant::now();
            if now >= deadline {
                return Err(StoreError::Corrupt(format!(
                    "write backpressure timed out waiting for a merge \
                     ({events} delta events ≥ hard limit {hard})"
                )));
            }
            g = self
                .gate
                .wait_timeout(g, deadline - now)
                .expect("gate lock")
                .0;
        }
    }

    /// Signal the backpressure gate that delta pressure may have dropped.
    /// Signalers take the gate lock briefly so a waiter's check-then-wait
    /// can't miss the wakeup.
    fn notify_gate(&self) {
        drop(self.gate_lock.lock().expect("gate lock"));
        self.gate.notify_all();
    }

    /// Register the background merger (called by
    /// [`MergeScheduler::spawn`](crate::MergeScheduler)). One at a time.
    pub(crate) fn attach_merger(&self) {
        assert!(
            !self.merger.swap(true, Ordering::Relaxed),
            "a merger is already attached to this store"
        );
    }

    /// Deregister the background merger; writers blocked on the gate fall
    /// back to fail-fast.
    pub(crate) fn detach_merger(&self) {
        self.merger.store(false, Ordering::Relaxed);
        self.notify_gate();
    }

    /// Timings/sizes of the most recent completed merge, if any.
    pub fn last_merge_stats(&self) -> Option<MergeStats> {
        *self.merge_stats.lock().expect("merge stats")
    }

    /// The configured delta budget as `(soft, hard)`.
    pub fn delta_budget(&self) -> (u64, u64) {
        (
            self.budget_soft.load(Ordering::Relaxed),
            self.budget_hard.load(Ordering::Relaxed),
        )
    }

    /// How long a writer may block on hard-budget backpressure before its
    /// commit fails (only reachable with a merger attached).
    pub fn set_backpressure_timeout(&self, timeout: std::time::Duration) {
        self.backpressure_ms
            .store(timeout.as_millis() as u64, Ordering::Relaxed);
    }

    /// Configure the delta budget (doc 07 §3, in delta events ≈ quads).
    /// Crossing `soft` sets the sticky [`Store::needs_merge`] signal; at
    /// `hard` commits fail until a merge (M5) folds the delta away.
    pub fn set_delta_budget(&self, soft: u64, hard: u64) {
        self.budget_soft.store(soft, Ordering::Relaxed);
        self.budget_hard.store(hard, Ordering::Relaxed);
        self.notify_gate();
    }

    /// Whether the delta has crossed the soft budget (merge scheduling
    /// signal; sticky until the M5 merger clears it).
    pub fn needs_merge(&self) -> bool {
        self.merge_needed.load(Ordering::Relaxed)
    }

    /// Epoch GC (doc 07 §2): reclaim delta events no live snapshot can
    /// observe. The floor is the oldest epoch pinned by any outstanding
    /// [`Snapshot`] (the store's current snapshot always pins its own, so
    /// the floor exists and never exceeds the current epoch — and can only
    /// rise while this runs, keeping the floor sound). Superseded events
    /// drop; so do events that merely restate base membership (collapsed
    /// delete/re-add chains, tombstones of never-present quads). Reclaimed
    /// events lower [`Snapshot::delta_events`] and therefore the budget
    /// pressure. Returns the number of events reclaimed. Housekeeping —
    /// call between commits or from the merge scheduler; safe (but
    /// pointless) to call concurrently with writers.
    pub fn gc(&self) -> u64 {
        let snap = self.snapshot();
        let min = self.pins.min().expect("current snapshot pins its epoch");
        let reclaimed = snap.delta.gc(min, |key| snap.base.contains_quad(key));
        self.sweep_retired();
        if reclaimed > 0 {
            self.notify_gate();
        }
        reclaimed
    }

    /// In-memory compaction for ephemeral stores — the counterpart of
    /// [`Store::merge`], which needs a segment directory to fold into
    /// (docs/11). Scan cost over a long-lived embedded store grows with
    /// resident delta *history*: every scan collects its full matching
    /// event range and every tombstone is zipper work, so a churning store
    /// (repeated graph replaces, delete/re-add cycles) degrades
    /// monotonically even though its net size is stable. This rebuilds the
    /// delta as the net state at the current epoch — one `Add` per live
    /// overlay quad, one `Tombstone` per deleted base quad, a fresh
    /// overlay dictionary holding only referenced terms — and publishes it
    /// over the unchanged base, restoring the freshly-restored read
    /// profile without a worker reboot.
    ///
    /// Exclusive with commit groups for the whole rebuild (bounded by the
    /// delta budget); readers are untouched — live snapshots share the old
    /// delta and stay frozen. The WAL is untouched too: a capture-mode
    /// store's durable log still replays to the same dataset, and
    /// post-compaction commits append coherently (pair with
    /// [`Store::pack_log`] to compact the durable side). Public overlay
    /// [`TermId`]s are re-issued — they are snapshot-scoped, exactly as
    /// across a merge swap.
    pub fn compact_ephemeral(&self) -> Result<Arc<Snapshot>, StoreError> {
        if !self.is_ephemeral() {
            return Err(StoreError::Corrupt(
                "compact_ephemeral on a directory-backed store — use merge/compact".into(),
            ));
        }
        self.check_poisoned()?;
        let _running = self
            .merge_lock
            .try_lock()
            .map_err(|_| StoreError::Corrupt("a merge is already running on this store".into()))?;
        self.acquire_leadership();
        let result = self.fold_ephemeral_delta();
        self.drain_leadership();
        let snap = result?;
        self.compact_floor
            .store(snap.delta.events(), Ordering::Relaxed);
        self.merge_needed.store(
            snap.delta.events() > self.budget_soft.load(Ordering::Relaxed),
            Ordering::Relaxed,
        );
        // Folded-away history lowers budget pressure: release gated writers.
        self.notify_gate();
        Ok(snap)
    }

    /// The exclusive section of an ephemeral compaction (leadership held —
    /// no commit can plan or publish): fold the old delta's effective state
    /// into a fresh delta over the shared base. Base and inline column
    /// values carry over verbatim (the base is unchanged, so its id spaces
    /// are identical on both sides); overlay values re-intern their concise
    /// bytes through the same alias-preserving path commits use. Events
    /// land at the tip epoch — the only epoch new snapshots observe.
    fn fold_ephemeral_delta(&self) -> Result<Arc<Snapshot>, StoreError> {
        let tip = self.snapshot();
        let c = &tip.base.manifest.counts;
        let ranges = BaseRanges {
            subjects: c.shared + c.subjects,
            predicates: c.predicates,
            objects: c.shared + c.objects,
            graphs: c.graphs + 1,
        };
        let fresh = Arc::new(Delta::new(&tip.base.scan_orders(), ranges));

        // Overlay column values sit above the base watermarks; tagged
        // object values (inline literals, triple-term refs) are
        // generation-independent identities and copy through.
        let overlay = |v: u64, pos: TermPos| match pos {
            TermPos::Subject => v >= ranges.subjects,
            TermPos::Predicate => v >= ranges.predicates,
            TermPos::Object => v >> 60 == 0 && v >= ranges.objects,
            TermPos::Graph => v >= ranges.graphs,
        };
        const POS: [TermPos; 4] = [
            TermPos::Subject,
            TermPos::Predicate,
            TermPos::Object,
            TermPos::Graph,
        ];
        // Old overlay value → fresh overlay value, memoized per position.
        let mut memo: [HashMap<u64, u64>; 4] = Default::default();
        let mut entries: Vec<(QuadKey, DeltaKind)> = Vec::new();
        for (key, kind) in tip.delta.collect_effective(tip.epoch) {
            let overlay_at = |i: usize| overlay(key[i], POS[i]);
            match kind {
                // A net deletion only means anything for base-resident
                // quads; an overlay column marks a quad the base never had
                // (its add and its delete fold to nothing).
                DeltaKind::Tombstone => {
                    if !(0..4).any(overlay_at) && tip.base.contains_quad(key) {
                        entries.push((key, DeltaKind::Tombstone));
                    }
                }
                DeltaKind::Add => {
                    if !(0..4).any(overlay_at) {
                        // All-base columns: a delete/re-add chain over a
                        // base quad folds to plain base membership; only a
                        // net-new combination of base terms survives.
                        if !tip.base.contains_quad(key) {
                            entries.push((key, DeltaKind::Add));
                        }
                        continue;
                    }
                    let mut mapped = key;
                    for i in 0..4 {
                        if overlay_at(i) {
                            mapped[i] = *memo[i].entry(key[i]).or_insert_with(|| {
                                let bytes = tip
                                    .delta
                                    .decode(key[i], POS[i])
                                    .expect("overlay column decodes");
                                intern_overlay(&tip.base, &fresh, &bytes, POS[i])
                            });
                        }
                    }
                    entries.push((mapped, DeltaKind::Add));
                }
            }
        }
        if !entries.is_empty() {
            fresh.record(&entries, tip.epoch);
        }
        let snap = Arc::new(Snapshot {
            base: Arc::clone(&tip.base),
            delta: fresh,
            generation: tip.generation,
            epoch: tip.epoch,
            _pin: self.pins.pin(tip.epoch),
        });
        self.current.store(Arc::clone(&snap));
        Ok(snap)
    }

    /// Whether [`Store::compact_ephemeral_if_due`] would compact now. Due
    /// when the store is ephemeral, the delta holds at least one tombstone
    /// (a delta whose every event is a live add folds to itself), and either
    /// (a) it grew by the configured minimum and doubled since the last fold
    /// or (b) it grew at all and is within one minimum step of the hard
    /// budget, where another commit would fail. The doubling rule amortizes
    /// ordinary fold work to O(dataset); the hard-budget rider bypasses the
    /// minimum so a floor already near the limit cannot strand reclaimable
    /// churn behind an unreachable threshold.
    pub fn ephemeral_compaction_due(&self) -> bool {
        if !self.is_ephemeral() {
            return false;
        }
        let snap = self.snapshot();
        let events = snap.delta.events();
        let floor = self.compact_floor.load(Ordering::Relaxed);
        let min = self.compact_min.load(Ordering::Relaxed);
        let hard = self.budget_hard.load(Ordering::Relaxed);
        let grew_min = events >= floor.saturating_add(min);
        let doubled = events >= floor.saturating_mul(2);
        let grew_near_hard = events > floor && events.saturating_add(min) >= hard;
        snap.delta.tombstones() > 0 && ((grew_min && doubled) || grew_near_hard)
    }

    /// [`Store::compact_ephemeral`] when [`Store::ephemeral_compaction_due`]
    /// says so — the fabric-host hook: call after (batches of) writes; the
    /// check is a few atomic loads. `None` = not due (including on
    /// directory-backed stores, so hosts may call unconditionally).
    pub fn compact_ephemeral_if_due(&self) -> Result<Option<Arc<Snapshot>>, StoreError> {
        if !self.ephemeral_compaction_due() {
            return Ok(None);
        }
        self.compact_ephemeral().map(Some)
    }

    /// Set the minimum event growth between ephemeral compactions
    /// (default 65 536; see [`Store::ephemeral_compaction_due`]).
    pub fn set_ephemeral_compaction_min(&self, events: u64) {
        self.compact_min.store(events.max(1), Ordering::Relaxed);
    }

    /// Merge with default settings — see [`Store::merge_with`].
    pub fn merge(&self) -> Result<Arc<Snapshot>, StoreError> {
        self.merge_with(&MergeConfig::default())
    }

    /// Minor merge (doc 07 §6.4): lazily materialize one extra ordering on
    /// the CURRENT generation — Phase C alone, from a canonical run
    /// re-derived off the base's SPO walk. No fold, no remap, no WAL
    /// rotation: the id space is unchanged, so live snapshots, the delta
    /// (which gains a backfilled index for the new order), and every
    /// epoch's history stay valid. The new component is written first and
    /// the manifest update is atomic-last — a crash in between leaves an
    /// orphan `idx/*.bt` the manifest never references (harmless; the next
    /// successful attempt overwrites it). Publishes a same-generation
    /// snapshot whose base serves the new ordering. No-op if the ordering
    /// is already materialized. Serialized against major merges.
    pub fn add_ordering(&self, order: Order) -> Result<Arc<Snapshot>, StoreError> {
        self.add_ordering_with(order, &MergeConfig::default())
    }

    /// [`Store::add_ordering`] with explicit sort-budget/pacing settings
    /// (`cfg.profile` is ignored — minor merges never change profiles).
    pub fn add_ordering_with(
        &self,
        order: Order,
        cfg: &MergeConfig,
    ) -> Result<Arc<Snapshot>, StoreError> {
        self.check_ephemeral("add_ordering (no segment directory)")?;
        self.check_poisoned()?;
        let _running = self
            .merge_lock
            .try_lock()
            .map_err(|_| StoreError::Corrupt("a merge is already running on this store".into()))?;
        let snap = self.snapshot();
        let seg = &snap.base;
        if seg.manifest.orderings.iter().any(|n| n == order.name()) {
            return Ok(snap);
        }

        // Canonical run: walk the base SPO (quads, so the width maxima are
        // computed exactly like the original build's — including tagged
        // object values), dedup to distinct triples.
        let mut bcfg = BuilderConfig::new(seg.seg_dir());
        bcfg.sort_budget = cfg.sort_budget;
        bcfg.pace_duty = cfg.pace_duty;
        std::fs::create_dir_all(&bcfg.scratch).map_err(|e| StoreError::io(&bcfg.scratch, e))?;
        let triples_path = bcfg.scratch.join("triples.run");
        let mut n_triples = 0u64;
        let mut maxima = [0u64; 4];
        {
            use std::io::Write as _;
            let mut out = std::io::BufWriter::new(
                std::fs::File::create(&triples_path)
                    .map_err(|e| StoreError::io(&triples_path, e))?,
            );
            let mut gate = crate::builder::PaceGate::new(bcfg.pace_duty);
            let mut scan = seg.scan_order(&Pattern::default(), Order::Spo)?;
            let mut batch = QuadBatch::new();
            let mut last: Option<[u64; 3]> = None;
            while scan.next_batch(&mut batch)? {
                for i in 0..batch.len() {
                    crate::builder::pace(&mut gate);
                    let t = [batch.s[i], batch.p[i], batch.o[i]];
                    maxima = [
                        maxima[0].max(t[0]),
                        maxima[1].max(t[1]),
                        maxima[2].max(t[2]),
                        maxima[3].max(batch.g[i]),
                    ];
                    if last == Some(t) {
                        continue;
                    }
                    last = Some(t);
                    let mut buf = [0u8; 24];
                    for (j, v) in t.iter().enumerate() {
                        buf[j * 8..j * 8 + 8].copy_from_slice(&v.to_le_bytes());
                    }
                    out.write_all(&buf)
                        .map_err(|e| StoreError::io(&triples_path, e))?;
                    n_triples += 1;
                }
            }
            out.flush().map_err(|e| StoreError::io(&triples_path, e))?;
        }
        let widths = [
            graphy_succinct::intvec::bits_for(maxima[0]),
            graphy_succinct::intvec::bits_for(maxima[1]),
            graphy_succinct::intvec::bits_for(maxima[2]),
            graphy_succinct::intvec::bits_for(maxima[3]),
        ];
        let result = crate::builder::materialize_ordering(
            &bcfg,
            &triples_path,
            n_triples,
            order,
            widths,
            seg.manifest.has_graphs,
        );
        std::fs::remove_file(&triples_path).ok();
        std::fs::remove_dir(&bcfg.scratch).ok();
        let (rel, lg) = result?;

        // Atomic point: the manifest now references the new component.
        let mut manifest = seg.manifest.clone();
        manifest.components.insert(
            rel,
            crate::manifest::Component {
                bytes: lg.0,
                xxh3: format!("{:016x}", lg.1),
            },
        );
        manifest.orderings.push(order.name().to_owned());
        manifest.save(seg.seg_dir())?;

        // Publish: same generation/epoch/delta, base reopened with the new
        // ordering; the delta index backfills BEFORE the base is visible.
        let new_base = Arc::new(Segment::open_with(seg.seg_dir(), self.mode)?);
        self.acquire_leadership();
        let tip = self.snapshot();
        tip.delta.add_order(order);
        let new = Arc::new(Snapshot {
            base: new_base,
            delta: Arc::clone(&tip.delta),
            generation: tip.generation,
            epoch: tip.epoch,
            _pin: self.pins.pin(tip.epoch),
        });
        self.current.store(Arc::clone(&new));
        self.drain_leadership();
        Ok(new)
    }

    /// Fold the delta into a new base generation (doc 07 §6, M5). The
    /// dataset as of the current epoch `f` (the *freeze* epoch) streams
    /// through the deterministic Phase A–D builders into `gen-{G+1}/`;
    /// commits keep flowing meanwhile, accumulating the *active suffix*
    /// (events with epoch > f). The swap then, briefly exclusive with
    /// commit groups: remaps the suffix into the new id space (terms travel
    /// as generation-independent concise bytes), stages a rotated WAL
    /// (`MergeCommit`, `Checkpoint(f)`, the suffix transactions), flips the
    /// `CURRENT` pointer, activates the rotated log, and publishes the new
    /// snapshot. Readers throughout see either generation, both correct;
    /// old-generation snapshots stay frozen and their files retire once the
    /// last one drops. Clears (or re-arms) the merge-needed signal.
    ///
    /// Crash-safe at every step: before the `CURRENT` flip the old store is
    /// intact (a fresh build directory is debris, removed at open); after
    /// it, replaying the old log against the new base is a no-op-safe
    /// re-application (set semantics), and the rotated log takes over only
    /// when fully durable. One merge at a time; concurrent calls fail fast.
    pub fn merge_with(&self, cfg: &MergeConfig) -> Result<Arc<Snapshot>, StoreError> {
        self.check_ephemeral("merge/compact (no segment directory to fold into)")?;
        self.check_poisoned()?;
        let _running = self
            .merge_lock
            .try_lock()
            .map_err(|_| StoreError::Corrupt("a merge is already running on this store".into()))?;

        let snap_f = self.snapshot();
        let f = snap_f.epoch();
        let new_gen = snap_f.generation() + 1;

        // Observability bracket (replay skips it; a later successful merge
        // rotates it away).
        {
            let mut wal = self.wal.lock().expect("wal lock");
            wal.append_merge_start(f);
            wal.commit_group(Durability::Strict)?;
        }
        failpoint("merge:start-logged");

        // ---- build G+1 = base ⊎ delta@f (no locks held; commits proceed).
        let gen_dir = self.dir.join(format!("{GEN_PREFIX}{new_gen:06}"));
        if gen_dir.exists() {
            // Debris from a crashed attempt — CURRENT never points at an
            // uncommitted generation, and generation numbers only advance.
            std::fs::remove_dir_all(&gen_dir).map_err(|e| StoreError::io(&gen_dir, e))?;
        }
        let base_profile = Profile::from_name(&snap_f.base.manifest.profile).ok_or_else(|| {
            StoreError::Corrupt(format!(
                "unknown profile {:?} in base manifest",
                snap_f.base.manifest.profile
            ))
        })?;
        let mut bcfg = BuilderConfig::new(&gen_dir);
        bcfg.profile = cfg.profile.unwrap_or(base_profile);
        // Same profile: preserve the base's CURRENT ordering set, so
        // lazily added orderings (minor merges) survive the fold. An
        // explicit profile change resets to the new profile's defaults.
        bcfg.orderings = match cfg.profile {
            Some(_) => None,
            None => Some(
                snap_f
                    .base
                    .manifest
                    .orderings
                    .iter()
                    .map(|n| {
                        Order::from_name(n).ok_or_else(|| {
                            StoreError::Corrupt(format!("unknown ordering {n:?} in base manifest"))
                        })
                    })
                    .collect::<Result<Vec<_>, _>>()?,
            ),
        };
        bcfg.sort_budget = cfg.sort_budget;
        bcfg.generation = new_gen;
        bcfg.pace_duty = cfg.pace_duty;
        let build_start = std::time::Instant::now();
        let mut folded_quads = 0u64;
        // Streaming dictionary merge (doc 07 §6.2 Phase A): the new
        // dictionary comes from a k-way merge of the base's sorted PFC
        // sections with the (budget-bounded) overlay — no re-interning, no
        // hash map over every distinct term; then Phases B–D consume the
        // freeze snapshot rewritten through the remap tables, so no term
        // bytes flow at all. Usage bits computed by the pre-pass drop
        // garbage terms (all-tombstoned) and migrate roles.
        let usage = crate::dictmerge::usage_prepass(&snap_f)?;
        let md = crate::dictmerge::merge_dictionaries(&snap_f, &usage)?;
        crate::builder::build_from_ids(
            &bcfg,
            &md.sections,
            &md.tt_records,
            md.counts_dict,
            &mut |sink| {
                let mut gate = crate::builder::PaceGate::new(bcfg.pace_duty);
                let mut scan = snap_f.scan(&Pattern::default(), Order::Spo)?;
                let mut batch = QuadBatch::new();
                while scan.next_batch(&mut batch)? {
                    folded_quads += batch.len() as u64;
                    for i in 0..batch.len() {
                        crate::builder::pace(&mut gate);
                        sink([
                            md.map_subj(batch.s[i])?,
                            md.map_pred(batch.p[i])?,
                            md.map_obj(batch.o[i])?,
                            md.map_graph(batch.g[i])?,
                        ])?;
                    }
                }
                Ok(())
            },
        )?;
        let new_base = Arc::new(Segment::open_with(&gen_dir, self.mode)?);
        // Make the new generation's directory entry durable before anything
        // can point at it.
        std::fs::File::open(&self.dir)
            .and_then(|d| d.sync_all())
            .map_err(|e| StoreError::io(&self.dir, e))?;
        let build = build_start.elapsed();
        failpoint("merge:built");

        // ---- shadow remap (doc 07 §6.3(b)): remap and stage the bulk of
        // the active suffix while commits keep flowing. Each pass covers
        // the epochs that landed since the previous one, so the tail left
        // for the exclusive section shrinks geometrically as long as the
        // remap outruns the write rate; the pass cap bounds a writer that
        // doesn't let it converge (backpressure bounds the delta anyway).
        let mut remap = SuffixRemap::new(&self.dir, &new_base, f)?;
        const TAIL_TARGET: usize = 2_048;
        const MAX_SHADOW_PASSES: usize = 8;
        for _ in 0..MAX_SHADOW_PASSES {
            let upto = self.snapshot().epoch();
            if remap.pass(&snap_f, &new_base, upto)? <= TAIL_TARGET {
                break;
            }
        }

        // ---- the swap: exclusive with commit groups (doc 07 §6.1 step 3).
        self.acquire_leadership();
        let swap_start = std::time::Instant::now();
        let result = self.swap_generation(new_base, &gen_dir, remap);
        let swap = swap_start.elapsed();
        self.drain_leadership();
        let (snap, suffix_events) = result?;
        *self.merge_stats.lock().expect("merge stats") = Some(MergeStats {
            folded_quads,
            suffix_events,
            build,
            swap,
        });
        drop(snap_f);
        self.sweep_retired();
        Ok(snap)
    }

    /// The exclusive section of a merge: the suffix remap's late tail (the
    /// shadow passes handled the bulk — doc 07 §6.3(b)), WAL-rotation
    /// fsync, `CURRENT` flip, publish. Leadership is held by the caller.
    fn swap_generation(
        &self,
        new_base: Arc<Segment>,
        gen_dir: &Path,
        mut remap: SuffixRemap,
    ) -> Result<(Arc<Snapshot>, u64), StoreError> {
        // No commits can land past this snapshot: leadership is held.
        let tip = self.snapshot();
        remap.pass(&tip, &new_base, tip.epoch)?;
        let suffix_events = remap.suffix_events;
        let new_delta = remap.new_delta;
        let staged = remap.stage.finish()?;
        failpoint("merge:staged");

        // Point of no return: flip CURRENT (atomic rename, fsynced). Before
        // this, aborting leaves the old store untouched; after it, the new
        // generation is the durable truth.
        let gen_name = gen_dir
            .file_name()
            .expect("generation directory name")
            .to_string_lossy();
        write_current(&self.dir, &gen_name)?;
        failpoint("merge:flipped");

        // Activate the rotated log. Failure here (rename/reopen) leaves the
        // durable state consistent — old full log against the new base
        // replays as set-semantics no-ops — but this handle can no longer
        // guarantee its in-memory view: poison it.
        let new_wal = match wal::activate_rotated(&self.dir, &staged) {
            Ok(w) => w,
            Err(e) => {
                self.poisoned.store(true, Ordering::Relaxed);
                return Err(e);
            }
        };
        *self.wal.lock().expect("wal lock") = new_wal;
        failpoint("merge:activated");

        // Publish (the new delta was populated by the remap passes at the
        // suffix's original epochs), and queue the old generation for
        // retirement.
        let snap = Arc::new(Snapshot {
            base: Arc::clone(&new_base),
            delta: Arc::clone(&new_delta),
            generation: new_base.manifest.generation,
            epoch: tip.epoch,
            _pin: self.pins.pin(tip.epoch),
        });
        {
            let mut seg_dir = self.seg_dir.lock().expect("segment dir");
            let old = std::mem::replace(&mut *seg_dir, gen_dir.to_owned());
            self.retired.lock().expect("retired list").push(Retired {
                seg: Arc::downgrade(&tip.base),
                at_root: old == self.dir,
                path: old,
            });
        }
        self.current.store(Arc::clone(&snap));
        self.merge_needed.store(
            new_delta.events() > self.budget_soft.load(Ordering::Relaxed),
            Ordering::Relaxed,
        );
        // The fold moved the delta into the base: release gated writers.
        self.notify_gate();
        Ok((snap, suffix_events))
    }

    /// Unlink retired generations whose last snapshot has dropped
    /// (doc 07 §6.1 step 4). Best-effort file removal; anything missed is
    /// debris the next open cleans.
    fn sweep_retired(&self) {
        let mut retired = self.retired.lock().expect("retired list");
        retired.retain(|r| {
            if r.seg.upgrade().is_some() {
                return true;
            }
            if r.at_root {
                for sub in ["dict", "idx", "graphs", "stats"] {
                    std::fs::remove_dir_all(r.path.join(sub)).ok();
                }
                std::fs::remove_file(r.path.join(MANIFEST_NAME)).ok();
            } else {
                std::fs::remove_dir_all(&r.path).ok();
            }
            false
        });
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn open_mode(&self) -> OpenMode {
        self.mode
    }
}

/// The shared commit core (doc 07 §5, also re-run verbatim by WAL replay):
/// validate, resolve-or-intern, and filter to the *effective* set-semantics
/// events at the snapshot's state — returning the delta entries and the
/// loggable ops as `(del, index)` references into the caller's dels/adds
/// slices (the WAL encoder reads the borrowed terms directly; a large
/// commit never materializes an owned copy of its quad bytes here).
type CommitPlan = (Vec<(QuadKey, DeltaKind)>, Vec<(bool, usize)>);

/// One planned (non-empty) transaction: its delta entries and epoch.
type PlannedTx = (Vec<(QuadKey, DeltaKind)>, u64);

/// One remapped suffix epoch at a merge swap: re-keyed delta entries plus
/// the term-level ops for the rotated log.
type SuffixTx = (Vec<(QuadKey, DeltaKind)>, Vec<WalOp>);

/// Incremental suffix-remap state (doc 07 §6.3(b)): the cross-generation
/// term memos, the new generation's delta being populated at the suffix's
/// original epochs, and the incrementally staged rotated log. Shadow
/// passes run *before* the swap takes leadership (commits keep flowing);
/// the exclusive section runs one final tail pass — covering only the
/// epochs that landed since the last shadow pass — and fsyncs the stage.
/// Terms cross generations through their concise bytes: decode via the old
/// snapshot, resolve in the new base, intern into the new overlay on a
/// miss (a suffix term may not exist anywhere in G+1).
struct SuffixRemap {
    new_delta: Arc<Delta>,
    stage: wal::RotationStage,
    /// Per-position old-column → (new column, concise bytes) memo.
    maps: [HashMap<u64, (u64, Vec<u8>)>; 4],
    /// Epochs ≤ `mark` are remapped, recorded, and staged.
    mark: u64,
    /// Total suffix events across all passes.
    suffix_events: u64,
}

impl SuffixRemap {
    fn new(
        dir: &Path,
        new_base: &Arc<Segment>,
        freeze_epoch: u64,
    ) -> Result<SuffixRemap, StoreError> {
        let c = &new_base.manifest.counts;
        let new_delta = Arc::new(Delta::new(
            &new_base.scan_orders(),
            BaseRanges {
                subjects: c.shared + c.subjects,
                predicates: c.predicates,
                objects: c.shared + c.objects,
                graphs: c.graphs + 1,
            },
        ));
        let stage = wal::stage_open(dir, new_base.manifest.generation, freeze_epoch)?;
        Ok(SuffixRemap {
            new_delta,
            stage,
            maps: Default::default(),
            mark: freeze_epoch,
            suffix_events: 0,
        })
    }

    /// Remap the events with `mark < epoch ≤ upto`: record them into the
    /// new delta at their original epochs (in epoch order — passes cover
    /// disjoint ascending ranges, keeping every event list epoch-sorted)
    /// and append their re-serialized transactions to the staged log.
    /// Returns the number of events this pass processed.
    fn pass(&mut self, old: &Snapshot, new_base: &Segment, upto: u64) -> Result<usize, StoreError> {
        if upto <= self.mark {
            return Ok(0);
        }
        let suffix = old.delta.collect_suffix(self.mark, upto);
        self.mark = upto;
        self.suffix_events += suffix.len() as u64;
        let n = suffix.len();
        let mut by_epoch: BTreeMap<u64, SuffixTx> = BTreeMap::new();
        for (key, kind, epoch) in suffix {
            let positions = [
                TermPos::Subject,
                TermPos::Predicate,
                TermPos::Object,
                TermPos::Graph,
            ];
            for (i, pos) in positions.into_iter().enumerate() {
                if pos == TermPos::Graph && key[3] == 0 {
                    continue; // default graph: column 0 in every generation
                }
                if self.maps[i].contains_key(&key[i]) {
                    continue;
                }
                let concise = old.decode_value(key[i], pos)?;
                let nv = match pos {
                    TermPos::Graph => new_base
                        .resolve_term(&concise, pos)
                        .map(|v| v + 1)
                        .unwrap_or_else(|| self.new_delta.intern(&concise, pos, None)),
                    _ => new_base.resolve_term(&concise, pos).unwrap_or_else(|| {
                        intern_overlay(new_base, &self.new_delta, &concise, pos)
                    }),
                };
                self.maps[i].insert(key[i], (nv, concise));
            }
            let col = |i: usize| &self.maps[i][&key[i]];
            let new_key = [
                col(0).0,
                col(1).0,
                col(2).0,
                if key[3] == 0 { 0 } else { col(3).0 },
            ];
            let (entries, ops) = by_epoch.entry(epoch).or_default();
            entries.push((new_key, kind));
            ops.push(WalOp {
                del: kind == DeltaKind::Tombstone,
                s: col(0).1.clone(),
                p: col(1).1.clone(),
                o: col(2).1.clone(),
                g: (key[3] != 0).then(|| col(3).1.clone()),
            });
        }
        for (epoch, (entries, _)) in &by_epoch {
            self.new_delta.record(entries, *epoch);
        }
        let txs: Vec<(u64, Vec<WalOp>)> =
            by_epoch.into_iter().map(|(e, (_, ops))| (e, ops)).collect();
        self.stage.append_txs(&txs)?;
        Ok(n)
    }
}

fn commit_core(
    snap: &Snapshot,
    dels: &[QuadTerms<'_>],
    adds: &[QuadTerms<'_>],
    carry: &mut HashMap<QuadKey, DeltaKind>,
) -> Result<CommitPlan, StoreError> {
    // Validate everything up front: a failed transaction must not have
    // leaked partial effects into a shared group carry.
    for &(s, p, _, g) in dels.iter().chain(adds) {
        validate_quad(s, p, g)?;
    }
    let mut entries: Vec<(QuadKey, DeltaKind)> = Vec::new();
    let mut wal_ops: Vec<(bool, usize)> = Vec::new();
    // `carry` holds this commit's own effects AND (under group commit) the
    // effects of earlier transactions in the same group, whose delta
    // records land only after the group fsync.
    let pending = carry;
    let present = |key: QuadKey, pending: &HashMap<QuadKey, DeltaKind>| match pending.get(&key) {
        Some(k) => *k == DeltaKind::Add,
        None => snap.contains_key(key),
    };

    for (i, &(s, p, o, g)) in dels.iter().enumerate() {
        // Unresolvable term ⇒ the quad cannot exist.
        let Some(key) = snap.resolve_key(s, p, o, g) else {
            continue;
        };
        if present(key, pending) {
            entries.push((key, DeltaKind::Tombstone));
            wal_ops.push((true, i));
            pending.insert(key, DeltaKind::Tombstone);
        }
    }
    for (i, &(s, p, o, g)) in adds.iter().enumerate() {
        let key = snap.intern_key(s, p, o, g);
        if !present(key, pending) {
            entries.push((key, DeltaKind::Add));
            wal_ops.push((false, i));
            pending.insert(key, DeltaKind::Add);
        }
    }
    Ok((entries, wal_ops))
}

/// Public [`TermId`] of a column value from the positional section math
/// alone (no overlay alias lookup): the base mapping, where the shared
/// section already gives subject/object co-occurring terms one id.
fn raw_term_id(n_sh: u64, v: u64, pos: TermPos) -> TermId {
    match pos {
        TermPos::Subject => {
            if v < n_sh {
                TermId::dict(Section::Shared, v + 1)
            } else {
                TermId::dict(Section::Subjects, v - n_sh + 1)
            }
        }
        TermPos::Predicate => TermId::dict(Section::Predicates, v + 1),
        TermPos::Graph => {
            if v == 0 {
                TermId::DEFAULT_GRAPH
            } else {
                // Graph column values are already ordinal + 1.
                TermId::dict(Section::Graphs, v)
            }
        }
        TermPos::Object => match v >> 60 {
            0x0 => {
                if v < n_sh {
                    TermId::dict(Section::Shared, v + 1)
                } else {
                    TermId::dict(Section::Objects, v - n_sh + 1)
                }
            }
            // Inline values and triple-term references ARE TermIds.
            _ => TermId::from_raw(v),
        },
    }
}

/// Intern a base-miss term into the overlay, carrying its canonical
/// identity from the *other* subject/object space when it already has one
/// there (base or overlay). The base unifies co-occurring subject/object
/// terms through the shared section; without this, a term entering the
/// other position via an update would get a second, unrelated public id —
/// silently breaking every join through it.
fn intern_overlay(base: &Segment, delta: &Delta, bytes: &[u8], pos: TermPos) -> u64 {
    let opp = match pos {
        TermPos::Subject => Some(TermPos::Object),
        TermPos::Object => Some(TermPos::Subject),
        _ => None,
    };
    let alias = opp.and_then(|opp| {
        let n_sh = base.manifest.counts.shared;
        if let Some(v) = base.resolve_term(bytes, opp) {
            return Some(raw_term_id(n_sh, v, opp));
        }
        let v = delta.resolve(bytes, opp)?;
        Some(
            delta
                .canon(v, opp)
                .unwrap_or_else(|| raw_term_id(n_sh, v, opp)),
        )
    });
    delta.intern(bytes, pos, alias)
}

/// An immutable view of the dataset: base segment + the delta as of one
/// epoch (doc 07 §2).
#[derive(Debug)]
pub struct Snapshot {
    base: Arc<Segment>,
    delta: Arc<Delta>,
    generation: u64,
    epoch: u64,
    /// Keeps this snapshot's epoch visible to [`Store::gc`]'s floor.
    _pin: EpochPin,
}

impl Snapshot {
    /// Process-unique identity of this snapshot's overlay/id-space
    /// incarnation. Ordinary commits share it; compaction and generation
    /// swaps replace it. Consumers that cache resolved column values must
    /// include it in their cache key alongside generation and epoch.
    pub fn storage_identity(&self) -> u64 {
        self.delta.identity()
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// Write epoch this snapshot observes.
    pub fn epoch(&self) -> u64 {
        self.epoch
    }

    /// The base segment (escape hatch for tooling; the engine goes through
    /// the snapshot methods so the delta layer stays transparent).
    pub fn segment(&self) -> &Segment {
        &self.base
    }

    /// Delta events recorded so far (size signal for the merge scheduler).
    pub fn delta_events(&self) -> u64 {
        self.delta.events()
    }

    /// Delta tombstones recorded so far (read-amplification signal: every
    /// tombstone is zipper work on scans until a merge folds it away).
    pub fn delta_tombstones(&self) -> u64 {
        self.delta.tombstones()
    }

    /// Overlay term count for `pos` (streaming-merge input sizing).
    pub(crate) fn overlay_len(&self, pos: TermPos) -> u64 {
        self.delta.space_len(pos)
    }

    /// Every overlay term for `pos` as (bytes, old column value) — the
    /// streaming dictionary merge's overlay input.
    pub(crate) fn overlay_terms_at(&self, pos: TermPos) -> Vec<(Box<[u8]>, u64)> {
        self.delta.overlay_terms(pos)
    }

    // -------------------------------------------------------------- scans

    /// Scan `pat` in the requested ordering — the storage↔engine seam
    /// (doc 02 §8): base∪delta with tombstone elision, batched.
    pub fn scan(&self, pat: &Pattern, order: Order) -> Result<QuadScan<'_>, StoreError> {
        Ok(QuadScan {
            order,
            base: self.base.scan_order(pat, order)?,
            stage: QuadBatch::new(),
            stage_pos: 0,
            base_done: false,
            delta: self.delta.collect_range(order, pat, self.epoch),
            dpos: 0,
        })
    }

    /// Scan `pat` in the cheapest available ordering.
    pub fn scan_best(&self, pat: &Pattern) -> Result<QuadScan<'_>, StoreError> {
        self.scan(pat, self.base.best_scan_order(pat))
    }

    /// Orderings [`Snapshot::scan`] can emit in.
    pub fn scan_orders(&self) -> Vec<Order> {
        self.base.scan_orders()
    }

    /// Exact match count for `pat`: base count adjusted by the delta's
    /// effective events (each checked against base membership, so
    /// add-then-delete and delete-of-base compose correctly).
    pub fn count(&self, pat: &Pattern) -> Result<u64, StoreError> {
        let mut n = self.base.count(pat)?;
        let order = self.base.best_scan_order(pat);
        for (key, kind) in self.delta.collect_range(order, pat, self.epoch) {
            let in_base = self.base.contains_quad(key);
            match kind {
                DeltaKind::Add if !in_base => n += 1,
                DeltaKind::Tombstone if in_base => n -= 1,
                _ => {}
            }
        }
        Ok(n)
    }

    /// Cheap upper-bound match count for `pat` — the planner's and
    /// join-orderer's costing input. The base term is exact; the
    /// delta term is the matching index range's size when the bound
    /// components form a prefix, and the live event count otherwise
    /// (never the whole-map walk [`Snapshot::count`] needs for
    /// exactness). Tombstones are counted as matches, so this only
    /// ever over-estimates; never use it for query semantics.
    pub fn count_estimate(&self, pat: &Pattern) -> Result<u64, StoreError> {
        let n = self.base.count(pat)?;
        let order = self.base.best_scan_order(pat);
        Ok(n + self.delta.estimate_range(order, pat))
    }

    /// Whether the snapshot contains this exact column-value quad.
    pub(crate) fn contains_key(&self, key: QuadKey) -> bool {
        match self.delta.probe(key, self.epoch) {
            Some(DeltaKind::Add) => true,
            Some(DeltaKind::Tombstone) => false,
            None => self.base.contains_quad(key),
        }
    }

    // ------------------------------------------------- term resolution

    /// Column value of a concise term in `pos` (base, then overlay).
    fn resolve_col(&self, bytes: &[u8], pos: TermPos) -> Option<u64> {
        if let Some(v) = self.base.resolve_term(bytes, pos) {
            // The base graph space is section-ordinal-indexed; the column
            // convention reserves 0 for the default graph.
            return Some(if pos == TermPos::Graph { v + 1 } else { v });
        }
        self.delta.resolve(bytes, pos)
    }

    fn resolve_key(&self, s: &[u8], p: &[u8], o: &[u8], g: Option<&[u8]>) -> Option<QuadKey> {
        Some([
            self.resolve_col(s, TermPos::Subject)?,
            self.resolve_col(p, TermPos::Predicate)?,
            self.resolve_col(o, TermPos::Object)?,
            match g {
                None => 0,
                Some(g) => self.resolve_col(g, TermPos::Graph)?,
            },
        ])
    }

    /// Writer-side resolve-or-intern (overlay ids for base misses).
    fn intern_key(&self, s: &[u8], p: &[u8], o: &[u8], g: Option<&[u8]>) -> QuadKey {
        let col = |bytes: &[u8], pos: TermPos| {
            self.resolve_col(bytes, pos)
                .unwrap_or_else(|| intern_overlay(&self.base, &self.delta, bytes, pos))
        };
        [
            col(s, TermPos::Subject),
            col(p, TermPos::Predicate),
            col(o, TermPos::Object),
            g.map_or(0, |g| col(g, TermPos::Graph)),
        ]
    }

    /// Build a [`Pattern`] from concise term bytes; `None` when a bound term
    /// occurs nowhere in this snapshot (the pattern matches nothing).
    /// `g: Some(None)` = default graph; `None` = any graph.
    #[allow(clippy::type_complexity)]
    pub fn resolve_pattern(
        &self,
        s: Option<&[u8]>,
        p: Option<&[u8]>,
        o: Option<&[u8]>,
        g: Option<Option<&[u8]>>,
    ) -> Option<Pattern> {
        let mut pat = Pattern::default();
        if let Some(b) = s {
            pat.s = Some(self.resolve_col(b, TermPos::Subject)?);
        }
        if let Some(b) = p {
            pat.p = Some(self.resolve_col(b, TermPos::Predicate)?);
        }
        if let Some(b) = o {
            pat.o = Some(self.resolve_col(b, TermPos::Object)?);
        }
        match g {
            None => {}
            Some(None) => pat.g = Some(0),
            Some(Some(b)) => pat.g = Some(self.resolve_col(b, TermPos::Graph)?),
        }
        Some(pat)
    }

    /// Concise bytes of a column value in `pos`. Graph uses the **column**
    /// convention (`v ≥ 1` named; the default graph has no term).
    pub fn decode_value(&self, v: u64, pos: TermPos) -> Result<Vec<u8>, StoreError> {
        let c = &self.base.manifest.counts;
        let (in_base, base_v) = match pos {
            TermPos::Subject => (v < c.shared + c.subjects, v),
            TermPos::Predicate => (v < c.predicates, v),
            TermPos::Graph => {
                if v == 0 {
                    return Err(StoreError::Corrupt(
                        "the default graph has no concise term".into(),
                    ));
                }
                (v <= c.graphs, v - 1)
            }
            TermPos::Object => match v >> 60 {
                0x0 => (v < c.shared + c.objects, v),
                // Inline / triple-term values always decode via the base.
                _ => (true, v),
            },
        };
        if in_base {
            return self.base.decode_value(base_v, pos);
        }
        self.delta
            .decode(v, pos)
            .ok_or_else(|| StoreError::Corrupt(format!("column value {v:#x} out of range")))
    }

    // ---------------------------------------------- public TermId boundary

    /// Public [`TermId`] of a column value in the given position.
    ///
    /// Dictionary sections are a storage detail: the returned id is
    /// canonical across *all* RDF positions. Subject/object already share
    /// ids in the segment format. Predicate and graph terms prefer an
    /// existing object/subject id, then a predicate id, so a term bound in
    /// two different triple positions compares equal without decoding in
    /// the query engine.
    pub fn term_id(&self, v: u64, pos: TermPos) -> TermId {
        let c = &self.base.manifest.counts;
        if matches!(pos, TermPos::Subject | TermPos::Object) {
            let overlay = match pos {
                TermPos::Subject => v >= c.shared + c.subjects,
                TermPos::Object => v >> 60 == 0 && v >= c.shared + c.objects,
                _ => unreachable!(),
            };
            if overlay {
                if let Some(id) = self.delta.canon(v, pos) {
                    return id;
                }
            }
            return raw_term_id(c.shared, v, pos);
        }
        let local = raw_term_id(c.shared, v, pos);

        // Predicates and graph names have position-local dictionaries in
        // the persisted segment. Canonicalize them to the first earlier
        // identity space in the same order used by the query evaluator's
        // term interner. Failure to decode is an invariant violation, but
        // this infallible boundary retains the local id so corruption is
        // reported by the eventual decode rather than hidden here.
        let Ok(bytes) = self.decode_value(v, pos) else {
            return local;
        };
        for earlier in match pos {
            TermPos::Predicate => &[TermPos::Object, TermPos::Subject][..],
            TermPos::Graph => &[TermPos::Object, TermPos::Subject, TermPos::Predicate][..],
            _ => &[][..],
        } {
            if let Some(col) = self.resolve_col(&bytes, *earlier) {
                return self.term_id(col, *earlier);
            }
        }
        local
    }

    /// Column value of a public [`TermId`] in the given position; `None`
    /// when the id does not name a term usable in that position (wrong
    /// section, out of range, or a sentinel outside the graph column).
    pub fn column(&self, id: TermId, pos: TermPos) -> Option<u64> {
        if let Some(col) = self.column_direct(id, pos) {
            return Some(col);
        }
        // A canonical id whose section names the other subject/object space
        // may still hold a positional overlay value here.
        if matches!(pos, TermPos::Subject | TermPos::Object) {
            if let Some(col) = self.delta.alias_col(id, pos) {
                return Some(col);
            }
        }
        // Predicate and graph dictionaries remain position-local on disk.
        // Translate a canonical id through its concise spelling when a
        // query reuses the binding in another position.
        let bytes = self.decode(id).ok()?;
        self.resolve_col(&bytes, pos)
    }

    /// [`Snapshot::column`] without the overlay alias fallback.
    fn column_direct(&self, id: TermId, pos: TermPos) -> Option<u64> {
        let c = &self.base.manifest.counts;
        let n_sh = c.shared;
        let over = |pos| self.delta.space_len(pos);
        let dict = id.dict_ref();
        let in_range = |ordinal: u64, n: u64| (1..=n).contains(&ordinal);
        match pos {
            TermPos::Subject => match dict? {
                (Section::Shared, o) if in_range(o, n_sh) => Some(o - 1),
                (Section::Subjects, o) if in_range(o, c.subjects + over(TermPos::Subject)) => {
                    Some(n_sh + o - 1)
                }
                _ => None,
            },
            TermPos::Predicate => match dict? {
                (Section::Predicates, o)
                    if in_range(o, c.predicates + over(TermPos::Predicate)) =>
                {
                    Some(o - 1)
                }
                _ => None,
            },
            TermPos::Graph => {
                if id == TermId::DEFAULT_GRAPH {
                    return Some(0);
                }
                match dict? {
                    (Section::Graphs, o) if in_range(o, c.graphs + over(TermPos::Graph)) => Some(o),
                    _ => None,
                }
            }
            TermPos::Object => {
                if let Some(ord) = id.triple_term_ordinal() {
                    return (ord < c.triple_terms).then_some(id.raw());
                }
                match id.dict_ref() {
                    Some((Section::Shared, o)) if in_range(o, n_sh) => Some(o - 1),
                    Some((Section::Objects, o))
                        if in_range(o, c.objects + over(TermPos::Object)) =>
                    {
                        Some(n_sh + o - 1)
                    }
                    Some(_) => None,
                    // Inline values pass through; sentinels do not.
                    None => match id.tag()? {
                        graphy_core::Tag::Sentinel => None,
                        _ => Some(id.raw()),
                    },
                }
            }
        }
    }

    /// Public [`TermId`] of a concise term in the given position, if the
    /// term occurs there in this snapshot.
    pub fn resolve(&self, concise: &[u8], pos: TermPos) -> Option<TermId> {
        let v = self.resolve_col(concise, pos)?;
        Some(self.term_id(v, pos))
    }

    /// Concise bytes of a public [`TermId`].
    pub fn decode(&self, id: TermId) -> Result<Vec<u8>, StoreError> {
        let bad = |m: String| StoreError::Corrupt(m);
        let n_sh = self.base.manifest.counts.shared;
        if let Some((section, o)) = id.dict_ref() {
            if o == 0 {
                return Err(bad("null/zero dictionary ordinal".to_owned()));
            }
            let (v, pos) = match section {
                Section::Shared => (o - 1, TermPos::Subject),
                Section::Subjects => (n_sh + o - 1, TermPos::Subject),
                Section::Predicates => (o - 1, TermPos::Predicate),
                Section::Objects => (n_sh + o - 1, TermPos::Object),
                Section::Graphs => (o, TermPos::Graph),
                Section::TripleTerms => ((0x7 << 60) | (o - 1), TermPos::Object),
            };
            return self.decode_value(v, pos);
        }
        if id.triple_term_ordinal().is_some() || id.decode().is_some() {
            return self.base.decode_value(id.raw(), TermPos::Object);
        }
        Err(bad(format!("term id {:#x} has no concise form", id.raw())))
    }
}

/// The snapshot-level batched scan (doc 02 §8): a linear zipper of the base
/// [`SegmentScan`] and the delta's matching range in the same ordering, with
/// tombstone elision. Same batch contract as the segment scan.
#[derive(Debug)]
pub struct QuadScan<'a> {
    order: Order,
    base: SegmentScan<'a>,
    /// Staging buffer for base output (drained into the zipper).
    stage: QuadBatch,
    stage_pos: usize,
    base_done: bool,
    /// Matching delta entries at this snapshot's epoch, in permuted-key
    /// order (canonical keys).
    delta: Vec<(QuadKey, DeltaKind)>,
    dpos: usize,
}

impl QuadScan<'_> {
    /// The ordering this scan emits in.
    pub fn order(&self) -> Order {
        self.order
    }

    fn stage_key(&self) -> Option<QuadKey> {
        (self.stage_pos < self.stage.len()).then(|| {
            [
                self.stage.s[self.stage_pos],
                self.stage.p[self.stage_pos],
                self.stage.o[self.stage_pos],
                self.stage.g[self.stage_pos],
            ]
        })
    }

    /// Clear `out` and fill it with the next quads, up to its capacity.
    /// Returns `false` (with `out` empty) when the scan is exhausted.
    pub fn next_batch(&mut self, out: &mut QuadBatch) -> Result<bool, StoreError> {
        out.clear();
        let perm = |q: QuadKey| {
            let [x, y, z] = self.order.to_xyz(q[0], q[1], q[2]);
            [x, y, z, q[3]]
        };
        while out.len() < out.capacity() {
            // Keep the staging buffer primed.
            if !self.base_done && self.stage_pos >= self.stage.len() {
                self.stage_pos = 0;
                if !self.base.next_batch(&mut self.stage)? {
                    self.base_done = true;
                }
            }
            let b = self.stage_key();
            let d = self.delta.get(self.dpos).copied();
            match (b, d) {
                (None, None) => break,
                (Some(bk), None) => {
                    out.push_key(bk);
                    self.stage_pos += 1;
                }
                (None, Some((dk, kind))) => {
                    self.dpos += 1;
                    if kind == DeltaKind::Add {
                        out.push_key(dk);
                    }
                }
                (Some(bk), Some((dk, kind))) => {
                    let (bp, dp) = (perm(bk), perm(dk));
                    if bp < dp {
                        out.push_key(bk);
                        self.stage_pos += 1;
                    } else if dp < bp {
                        self.dpos += 1;
                        if kind == DeltaKind::Add {
                            out.push_key(dk);
                        }
                    } else {
                        // Same quad in both: the delta event is authoritative
                        // (a tombstone elides the base quad; an equal Add is
                        // prevented at apply but handled for robustness).
                        self.stage_pos += 1;
                        self.dpos += 1;
                        if kind == DeltaKind::Add {
                            out.push_key(dk);
                        }
                    }
                }
            }
        }
        Ok(!out.is_empty())
    }
}
