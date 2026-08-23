//! Build dictionary (docs 01 §5, 02 §4, 07 §7): hash-cons concise terms with
//! role bits during parsing, then finalize into sorted PFC sections with
//! dense ordinals and remap tables.
//!
//! ## ID spaces (format v1)
//!
//! Index columns store **dense position-local ids**, not `TermId`s:
//!
//! - subject column: `shared` ordinals then `subjects` ordinals (`0..n_sh+n_s`)
//! - predicate column: `predicates` ordinals
//! - graph column: `0` = default graph, then `graphs` ordinals + 1
//! - object column: `shared` then `objects` ordinals (`< 2⁶⁰`), or a raw
//!   inline `TermId` (tags 1–6), or a triple-term reference (tag 7 |
//!   ordinal) — the tag bits make the three ranges disjoint and the u64
//!   order deterministic.
//!
//! `TermId`s (with 1-based section ordinals, since `TermId::NULL` aliases
//! `(Shared, 0)`) appear only at the public API boundary.
//!
//! During the parse pass, terms get **provisional** references (internal
//! tags 0x8 = dictionary entry, 0x9 = triple term) that `FinalDict::map`
//! rewrites to final column values after sections sort. A provisional
//! dictionary payload is `(shard << 54) | local`: the serial [`BuildDict`]
//! is shard 0 with flat ordinals, the parallel [`ShardedDict`] (doc 07 §7)
//! spreads terms over [`SHARDS`] mutex-guarded maps by content hash. Local
//! ordinals are arrival-ordered (nondeterministic under parallel intern),
//! but every persisted artifact derives from the byte-sorted sections, so
//! segment output stays deterministic.

use std::collections::{BinaryHeap, HashMap, HashSet};
use std::io::{BufRead, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use graphy_core::{concise, TermId, TermRef};
use graphy_succinct::{Pfc, PfcBuilder};

use crate::sidecar::HashSidecar;

pub(crate) const R_SUBJ: u8 = 1;
pub(crate) const R_PRED: u8 = 2;
pub(crate) const R_OBJ: u8 = 4;
pub(crate) const R_GRAPH: u8 = 8;

/// Internal provisional tags (never persisted; disjoint from TermId tags).
const PROV_DICT: u64 = 0x8 << 60;
const PROV_TT: u64 = 0x9 << 60;
const PAYLOAD: u64 = (1 << 60) - 1;

/// Intern shards for the parallel dictionary (doc 07 §7).
pub(crate) const SHARDS: usize = 64;
const SHARD_SHIFT: u32 = 54;
const LOCAL_MASK: u64 = (1 << SHARD_SHIFT) - 1;

/// Triple-term column tag (persisted, doc 01 §4 tag 0x7).
pub(crate) const TT_TAG: u64 = 0x7 << 60;

/// Where a provisional reference is being used (selects the remap space).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Pos {
    Subject,
    Predicate,
    Object,
    Graph,
}

/// One intern map (a shard of the parallel dictionary, or the whole
/// serial one): every distinct term stays resident with an
/// arrival-ordered local id. Bounded-memory loads use the two-pass
/// [`SpillerSet`] path instead — id-minting and spilling don't mix well
/// (remap tables grow with re-interned occurrences, measured
/// counterproductive at scale; see BENCHMARKS.md).
#[derive(Debug, Default)]
struct Interner {
    map: HashMap<Box<[u8]>, Entry>,
    /// Locals handed out so far.
    assigned: u64,
}

/// Per-entry bookkeeping overhead estimate for the intern budget.
const ENTRY_OVERHEAD: usize = 64;

impl Interner {
    fn intern(&mut self, bytes: &[u8], role: u8) -> u64 {
        if let Some(e) = self.map.get_mut(bytes) {
            e.roles |= role;
            return e.local;
        }
        let local = self.assigned;
        self.assigned += 1;
        self.map.insert(bytes.into(), Entry { local, roles: role });
        local
    }

    /// Drain into finalize sources as one in-memory sorted source.
    fn into_sources(mut self, out: &mut Vec<Source>, base: u64) -> Result<(), String> {
        let mut live: Vec<(Box<[u8]>, u64, u8)> = self
            .map
            .drain()
            .map(|(b, e)| (b, e.local, e.roles))
            .collect();
        live.sort_unstable_by(|a, b| a.0.cmp(&b.0));
        if !live.is_empty() {
            out.push(Source::from_vec(live, base));
        }
        Ok(())
    }
}

/// Budgeted dedup window over term bytes for the TWO-PASS load (doc 07 §7
/// memory ceiling): terms stream in with role bits, duplicates within the
/// window collapse, and crossing the budget flushes the window as a
/// byte-sorted `(bytes, roles)` run (the shared run-record format with a
/// zero local). No ids are minted — pass 2 resolves quads against the
/// finished sections — so adversarial recurrence costs run bytes on disk,
/// never memory.
#[derive(Debug)]
pub(crate) struct TermSpiller {
    map: HashMap<Box<[u8]>, u8>,
    resident: usize,
    runs: Vec<PathBuf>,
    scratch: PathBuf,
    budget: usize,
    shard: usize,
    io_error: Option<String>,
}

impl TermSpiller {
    pub fn new(scratch: PathBuf, budget: usize, shard: usize) -> TermSpiller {
        TermSpiller {
            map: HashMap::new(),
            resident: 0,
            runs: Vec::new(),
            scratch,
            budget: budget.max(1 << 16),
            shard,
            io_error: None,
        }
    }

    pub fn add(&mut self, bytes: &[u8], roles: u8) {
        if let Some(r) = self.map.get_mut(bytes) {
            *r |= roles;
            return;
        }
        self.resident += bytes.len() + ENTRY_OVERHEAD;
        self.map.insert(bytes.into(), roles);
        if self.resident > self.budget && self.io_error.is_none() {
            if let Err(e) = self.flush() {
                self.io_error = Some(e);
            }
        }
    }

    fn flush(&mut self) -> Result<(), String> {
        if self.map.is_empty() {
            return Ok(());
        }
        let path = self
            .scratch
            .join(format!("terms-{}-{}.run", self.shard, self.runs.len()));
        let err = |e: std::io::Error| format!("{}: {e}", path.display());
        let mut entries: Vec<(Box<[u8]>, u8)> = self.map.drain().collect();
        entries.sort_unstable_by(|a, b| a.0.cmp(&b.0));
        let mut w = std::io::BufWriter::new(std::fs::File::create(&path).map_err(err)?);
        for (bytes, roles) in &entries {
            w.write_all(&(bytes.len() as u32).to_le_bytes())
                .map_err(err)?;
            w.write_all(bytes).map_err(err)?;
            w.write_all(&0u64.to_le_bytes()).map_err(err)?;
            w.write_all(&[*roles]).map_err(err)?;
        }
        w.flush().map_err(err)?;
        self.runs.push(path);
        self.resident = 0;
        Ok(())
    }

    /// Drain into merge sources (run files + the live window).
    fn into_sources(mut self, out: &mut Vec<Source>) -> Result<(), String> {
        if let Some(e) = self.io_error {
            return Err(e);
        }
        for path in self.runs {
            out.push(Source::open_run(&path, 0)?);
        }
        let mut live: Vec<(Box<[u8]>, u64, u8)> =
            self.map.drain().map(|(b, r)| (b, 0u64, r)).collect();
        live.sort_unstable_by(|a, b| a.0.cmp(&b.0));
        if !live.is_empty() {
            out.push(Source::from_vec(live, 0));
        }
        Ok(())
    }
}

#[derive(Debug, Default)]
pub(crate) struct BuildDict {
    terms: Interner,
    tts: HashMap<[u64; 3], u32>,
    tt_list: Vec<[u64; 3]>,
}

#[derive(Debug)]
struct Entry {
    /// Shard-local ordinal (flat ordinal in the serial dictionary).
    local: u64,
    roles: u8,
}

impl BuildDict {
    pub fn new() -> BuildDict {
        BuildDict::default()
    }

    /// Intern an IRI / blank node term (subject, predicate, or graph
    /// position — also triple-term subjects/predicates).
    pub fn intern_node(&mut self, bytes: &[u8], role: u8) -> u64 {
        PROV_DICT | self.terms.intern(bytes, role)
    }

    /// Intern an object term: inline-encodable literals bypass the
    /// dictionary entirely; triple terms recurse into their components.
    pub fn intern_object(&mut self, bytes: &[u8]) -> Result<u64, String> {
        match concise::decode(bytes).map_err(|e| e.to_string())? {
            TermRef::Literal(l) => {
                if l.lang().is_none() {
                    if let Some(id) = TermId::try_inline(l.lexical(), l.datatype()) {
                        return Ok(id.raw());
                    }
                }
                Ok(self.intern_node(bytes, R_OBJ))
            }
            TermRef::Iri(_) | TermRef::BlankNode(_) => Ok(self.intern_node(bytes, R_OBJ)),
            TermRef::TripleTerm(_) => {
                let (s, p, o) = split_triple_term(bytes)?;
                let s_ref = self.intern_node(s, R_SUBJ);
                let p_ref = self.intern_node(p, R_PRED);
                let o_ref = self.intern_object(o)?;
                let key = [s_ref, p_ref, o_ref];
                let next = self.tt_list.len() as u32;
                let id = *self.tts.entry(key).or_insert_with(|| {
                    self.tt_list.push(key);
                    next
                });
                Ok(PROV_TT | u64::from(id))
            }
        }
    }

    pub fn finalize(mut self) -> Result<FinalDict, String> {
        let tt_list = std::mem::take(&mut self.tt_list);
        let n_flat = self.terms.assigned as usize;
        let mut sources = Vec::new();
        self.terms.into_sources(&mut sources, 0)?;
        finalize_merge(sources, n_flat, Box::new([0]), tt_list)
    }
}

/// Flat remap index of a provisional dictionary payload (`shard_bases[s] +
/// local`); `None` for a shard the dictionary never populated.
fn flat_of(bases: &[u64], payload: u64) -> Option<u64> {
    let shard = (payload >> SHARD_SHIFT) as usize;
    bases.get(shard).map(|b| b + (payload & LOCAL_MASK))
}

/// The shared finalize core: partition terms into byte-sorted sections,
/// build the per-position remap tables (flat-indexed), and resolve triple
/// terms bottom-up. Deterministic regardless of intern order — everything
/// persisted derives from the byte order.
/// One byte-sorted stream of (bytes, flat id, roles) for the finalize
/// merge: a spilled run file or the in-memory tail of an interner.
struct Source {
    kind: SourceKind,
    base: u64,
    head: Option<(Box<[u8]>, u64, u8)>,
}

enum SourceKind {
    Run(std::io::BufReader<std::fs::File>, PathBuf),
    Mem(std::vec::IntoIter<(Box<[u8]>, u64, u8)>),
}

impl Source {
    fn open_run(path: &Path, base: u64) -> Result<Source, String> {
        let f = std::fs::File::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
        let mut s = Source {
            kind: SourceKind::Run(std::io::BufReader::new(f), path.to_owned()),
            base,
            head: None,
        };
        s.advance()?;
        Ok(s)
    }

    fn from_vec(v: Vec<(Box<[u8]>, u64, u8)>, base: u64) -> Source {
        let mut s = Source {
            kind: SourceKind::Mem(v.into_iter()),
            base,
            head: None,
        };
        s.advance().expect("memory source");
        s
    }

    /// Load the next record into `head` (None at end).
    fn advance(&mut self) -> Result<(), String> {
        self.head = match &mut self.kind {
            SourceKind::Mem(it) => it.next(),
            SourceKind::Run(r, path) => {
                let err = |e: std::io::Error| format!("{}: {e}", path.display());
                if r.fill_buf().map_err(err)?.is_empty() {
                    None
                } else {
                    let mut len4 = [0u8; 4];
                    r.read_exact(&mut len4).map_err(err)?;
                    let mut bytes = vec![0u8; u32::from_le_bytes(len4) as usize];
                    r.read_exact(&mut bytes).map_err(err)?;
                    let mut l8 = [0u8; 8];
                    r.read_exact(&mut l8).map_err(err)?;
                    let mut roles = [0u8; 1];
                    r.read_exact(&mut roles).map_err(err)?;
                    Some((bytes.into(), u64::from_le_bytes(l8), roles[0]))
                }
            }
        };
        Ok(())
    }
}

/// Marker for "+ n_shared" section values whose final column value is only
/// known once the merge finishes (the shared count).
const FIX: u64 = 1 << 63;
const NONE_REMAP: u64 = u64::MAX;

/// The shared finalize core, as a k-way merge over byte-sorted sources
/// (doc 07 §7): equal bytes group at the heap head — duplicates from
/// post-flush re-interning and role bits from different occurrences meet
/// here — so each distinct term is classified once (same section rules as
/// ever), streamed into its PFC builder(s) in sorted order, and every
/// contributing flat id gets its remap entry. Deterministic regardless of
/// intern order, thread count, or spill cadence. Triple terms resolve
/// bottom-up afterwards, exactly as before.
fn finalize_merge(
    sources: Vec<Source>,
    n_flat: usize,
    shard_bases: Box<[u64]>,
    tt_list: Vec<[u64; 3]>,
) -> Result<FinalDict, String> {
    let mut sources = sources;
    let mut subj = vec![NONE_REMAP; n_flat];
    let mut obj = vec![NONE_REMAP; n_flat];
    let mut pred = vec![NONE_REMAP; n_flat];
    let mut graph = vec![NONE_REMAP; n_flat];
    let mut b_shared = PfcBuilder::new(32);
    let mut b_subjects = PfcBuilder::new(32);
    let mut b_predicates = PfcBuilder::new(32);
    let mut b_objects = PfcBuilder::new(32);
    let mut b_graphs = PfcBuilder::new(32);
    let (mut n_sh, mut n_su, mut n_pr, mut n_ob, mut n_gr) = (0u64, 0u64, 0u64, 0u64, 0u64);

    let mut heap: BinaryHeap<std::cmp::Reverse<(Box<[u8]>, usize)>> = BinaryHeap::new();
    for (i, s) in sources.iter().enumerate() {
        if let Some((b, _, _)) = &s.head {
            heap.push(std::cmp::Reverse((b.clone(), i)));
        }
    }
    let mut group: Vec<u64> = Vec::new(); // flat ids of the current term
    while let Some(std::cmp::Reverse((bytes, first))) = heap.pop() {
        group.clear();
        let mut roles = 0u8;
        let mut take = |sources: &mut Vec<Source>, i: usize| -> Result<(), String> {
            let s = &mut sources[i];
            let (_, local, r) = s.head.take().expect("head present");
            group.push(s.base + local);
            roles |= r;
            s.advance()
        };
        take(&mut sources, first)?;
        if let Some((b, _, _)) = &sources[first].head {
            heap.push(std::cmp::Reverse((b.clone(), first)));
        }
        while let Some(std::cmp::Reverse((b, i))) = heap.peek() {
            if **b != *bytes {
                break;
            }
            let i = *i;
            heap.pop();
            take(&mut sources, i)?;
            if let Some((b, _, _)) = &sources[i].head {
                heap.push(std::cmp::Reverse((b.clone(), i)));
            }
        }

        // Section membership (unchanged rules).
        let s_used = roles & R_SUBJ != 0;
        let o_used = roles & R_OBJ != 0;
        let (new_s, new_o) = match (s_used, o_used) {
            (true, true) => {
                b_shared.push(&bytes);
                let v = n_sh;
                n_sh += 1;
                (Some(v), Some(v))
            }
            (true, false) => {
                b_subjects.push(&bytes);
                let v = FIX | n_su;
                n_su += 1;
                (Some(v), None)
            }
            (false, true) => {
                b_objects.push(&bytes);
                let v = FIX | n_ob;
                n_ob += 1;
                (None, Some(v))
            }
            (false, false) => (None, None),
        };
        let new_p = (roles & R_PRED != 0).then(|| {
            b_predicates.push(&bytes);
            let v = n_pr;
            n_pr += 1;
            v
        });
        let new_g = (roles & R_GRAPH != 0).then(|| {
            b_graphs.push(&bytes);
            let v = n_gr;
            n_gr += 1;
            v
        });
        for &flat in &group {
            if let Some(v) = new_s {
                subj[flat as usize] = v;
            }
            if let Some(v) = new_o {
                obj[flat as usize] = v;
            }
            if let Some(v) = new_p {
                pred[flat as usize] = v;
            }
            if let Some(v) = new_g {
                graph[flat as usize] = v;
            }
        }
    }
    for t in [&mut subj, &mut obj] {
        for v in t.iter_mut() {
            if *v != NONE_REMAP && *v & FIX != 0 {
                *v = n_sh + (*v & !FIX);
            }
        }
    }
    // Spilled runs are consumed; remove them eagerly.
    for s in sources {
        if let SourceKind::Run(_, path) = s.kind {
            std::fs::remove_file(path).ok();
        }
    }

    let sections = Sections {
        shared: b_shared.build(),
        subjects: b_subjects.build(),
        predicates: b_predicates.build(),
        objects: b_objects.build(),
        graphs: b_graphs.build(),
    };

    // Triple terms: resolve components bottom-up (depth order), then sort
    // within each depth for deterministic ordinals (unchanged).
    let depths = tt_depths(&tt_list)?;
    let max_depth = depths.iter().copied().max().unwrap_or(0);
    let mut tt_final = vec![u64::MAX; tt_list.len()];
    let mut records: Vec<[u64; 3]> = Vec::with_capacity(tt_list.len());
    let resolve_dict = |r: u64, space: &[u64], what: &str| -> Result<u64, String> {
        let v = flat_of(&shard_bases, r & PAYLOAD)
            .and_then(|f| space.get(f as usize).copied())
            .unwrap_or(u64::MAX);
        if v == u64::MAX {
            return Err(format!("unresolved {what} in triple term"));
        }
        Ok(v)
    };
    for depth in 1..=max_depth {
        let mut level: Vec<([u64; 3], u32)> = Vec::new();
        for (i, key) in tt_list.iter().enumerate() {
            if depths[i] != depth {
                continue;
            }
            let s = resolve_dict(key[0], &subj, "subject")?;
            let p = resolve_dict(key[1], &pred, "predicate")?;
            let o = match key[2] >> 60 {
                0x8 => resolve_dict(key[2], &obj, "object")?,
                0x9 => {
                    let inner = tt_final[(key[2] & PAYLOAD) as usize];
                    debug_assert_ne!(inner, u64::MAX, "lower depth resolved first");
                    inner
                }
                _ => key[2], // inline TermId passes through
            };
            level.push(([s, p, o], i as u32));
        }
        level.sort_unstable();
        for (record, i) in level {
            tt_final[i as usize] = TT_TAG | records.len() as u64;
            records.push(record);
        }
    }

    Ok(FinalDict {
        sections,
        shard_bases,
        subj,
        obj,
        pred,
        graph,
        tt_final,
        tt_records: records,
    })
}

/// Parallel intern dictionary (doc 07 §7): terms spread over [`SHARDS`]
/// mutex-guarded maps by content hash, so independent ingest lanes intern
/// concurrently with low contention. `&self` methods — share via `Arc`.
#[derive(Debug)]
pub(crate) struct ShardedDict {
    shards: Vec<Mutex<Interner>>,
    /// Triple terms are rare; one mutex suffices.
    tts: Mutex<TtShared>,
}

#[derive(Debug, Default)]
struct TtShared {
    map: HashMap<[u64; 3], u32>,
    list: Vec<[u64; 3]>,
}

impl ShardedDict {
    pub fn new() -> ShardedDict {
        ShardedDict {
            shards: (0..SHARDS).map(|_| Mutex::default()).collect(),
            tts: Mutex::default(),
        }
    }

    fn shard_of(bytes: &[u8]) -> usize {
        xxhash_rust::xxh3::xxh3_64(bytes) as usize & (SHARDS - 1)
    }

    /// Thread-safe [`BuildDict::intern_node`].
    pub fn intern_node(&self, bytes: &[u8], role: u8) -> u64 {
        let s = Self::shard_of(bytes);
        let tag = PROV_DICT | ((s as u64) << SHARD_SHIFT);
        let mut m = self.shards[s].lock().expect("no poisoned intern shard");
        let local = m.intern(bytes, role);
        debug_assert!(local < LOCAL_MASK, "shard-local ordinal overflow");
        tag | local
    }

    /// Thread-safe [`BuildDict::intern_object`].
    pub fn intern_object(&self, bytes: &[u8]) -> Result<u64, String> {
        match concise::decode(bytes).map_err(|e| e.to_string())? {
            TermRef::Literal(l) => {
                if l.lang().is_none() {
                    if let Some(id) = TermId::try_inline(l.lexical(), l.datatype()) {
                        return Ok(id.raw());
                    }
                }
                Ok(self.intern_node(bytes, R_OBJ))
            }
            TermRef::Iri(_) | TermRef::BlankNode(_) => Ok(self.intern_node(bytes, R_OBJ)),
            TermRef::TripleTerm(_) => {
                let (s, p, o) = split_triple_term(bytes)?;
                let s_ref = self.intern_node(s, R_SUBJ);
                let p_ref = self.intern_node(p, R_PRED);
                let o_ref = self.intern_object(o)?;
                let key = [s_ref, p_ref, o_ref];
                let mut guard = self.tts.lock().expect("no poisoned tt intern");
                let tts = &mut *guard; // split the guard into disjoint fields
                let next = tts.list.len() as u32;
                let list = &mut tts.list;
                let id = *tts.map.entry(key).or_insert_with(|| {
                    list.push(key);
                    next
                });
                Ok(PROV_TT | u64::from(id))
            }
        }
    }

    /// Flatten the shards ((shard, local) → cumulative-base + local) and run
    /// the shared finalize. Output is deterministic: sections byte-sort and
    /// every persisted artifact derives from them.
    pub fn finalize(self) -> Result<FinalDict, String> {
        let mut bases = Vec::with_capacity(SHARDS);
        let mut sources = Vec::new();
        let mut running = 0u64;
        for shard in self.shards {
            let m = shard.into_inner().expect("no poisoned intern shard");
            bases.push(running);
            running += m.assigned;
            m.into_sources(&mut sources, *bases.last().expect("just pushed"))?;
        }
        let tts = self.tts.into_inner().expect("no poisoned tt intern");
        finalize_merge(sources, running as usize, bases.into(), tts.list)
    }
}

/// Cyclic references are impossible by construction (triple terms intern
/// bottom-up), so depth is a simple recursion over stored keys.
fn tt_depths(list: &[[u64; 3]]) -> Result<Vec<u32>, String> {
    let mut depths = vec![0u32; list.len()];
    fn depth_of(list: &[[u64; 3]], depths: &mut [u32], i: usize) -> u32 {
        if depths[i] != 0 {
            return depths[i];
        }
        let inner = match list[i][2] >> 60 {
            0x9 => depth_of(list, depths, (list[i][2] & PAYLOAD) as usize),
            _ => 0,
        };
        depths[i] = inner + 1;
        depths[i]
    }
    for i in 0..list.len() {
        depth_of(list, &mut depths, i);
    }
    Ok(depths)
}

/// Component byte slices of a concise triple term.
pub(crate) type TtParts<'a> = (&'a [u8], &'a [u8], &'a [u8]);

/// Split a concise triple term into its component byte slices.
pub(crate) fn split_triple_term(bytes: &[u8]) -> Result<TtParts<'_>, String> {
    let payload = bytes
        .strip_prefix(&[0x09])
        .ok_or("not a concise triple term")?;
    let mut at = 0;
    let mut parts = [&payload[0..0]; 3];
    for part in &mut parts {
        let (len, n) = read_varint(&payload[at..]).ok_or("truncated triple term")?;
        at += n;
        let end = at
            .checked_add(len as usize)
            .filter(|&e| e <= payload.len())
            .ok_or("truncated triple term")?;
        *part = &payload[at..end];
        at = end;
    }
    if at != payload.len() {
        return Err("trailing bytes in triple term".to_owned());
    }
    Ok((parts[0], parts[1], parts[2]))
}

fn read_varint(b: &[u8]) -> Option<(u64, usize)> {
    let mut v = 0u64;
    for (i, &x) in b.iter().enumerate().take(10) {
        v |= u64::from(x & 0x7F) << (7 * i);
        if x & 0x80 == 0 {
            return Some((v, i + 1));
        }
    }
    None
}

/// The five PFC sections in section order.
#[derive(Debug)]
pub(crate) struct Sections {
    pub shared: Pfc,
    pub subjects: Pfc,
    pub predicates: Pfc,
    pub objects: Pfc,
    pub graphs: Pfc,
}

/// Finalized dictionary: sections plus provisional→final remap tables
/// (flat-indexed; `shard_bases` maps (shard, local) payloads onto them —
/// the serial dictionary is the single-shard case).
#[derive(Debug)]
pub(crate) struct FinalDict {
    pub sections: Sections,
    shard_bases: Box<[u64]>,
    subj: Vec<u64>,
    obj: Vec<u64>,
    pred: Vec<u64>,
    graph: Vec<u64>,
    tt_final: Vec<u64>,
    pub tt_records: Vec<[u64; 3]>,
}

impl FinalDict {
    /// Rewrite a provisional reference to its final column value.
    pub fn map(&self, prov: u64, pos: Pos) -> Result<u64, String> {
        match prov >> 60 {
            0x8 => {
                let space = match pos {
                    Pos::Subject => &self.subj,
                    Pos::Predicate => &self.pred,
                    Pos::Object => &self.obj,
                    Pos::Graph => &self.graph,
                };
                let v = flat_of(&self.shard_bases, prov & PAYLOAD)
                    .and_then(|f| space.get(f as usize).copied())
                    .unwrap_or(u64::MAX);
                if v == u64::MAX {
                    return Err(format!("provisional id {prov:#x} absent in {pos:?} space"));
                }
                Ok(v)
            }
            0x9 => {
                if pos != Pos::Object {
                    return Err("triple term outside object position".to_owned());
                }
                Ok(self.tt_final[(prov & PAYLOAD) as usize])
            }
            _ if pos == Pos::Object => Ok(prov), // inline TermId
            _ => Err(format!("inline value in {pos:?} position")),
        }
    }
}

// ---------------------------------------------------------------------------
// Two-pass bulk load (doc 07 §7 memory ceiling): pass 1 spills raw byte
// quads and streams every term through budgeted [`TermSpiller`]s (no ids
// minted, so memory is bounded by the dedup window regardless of term
// recurrence); the spilled runs k-way merge straight into the PFC sections
// ([`merge_sections`]); pass 2 re-reads the byte quads and resolves each
// term against the finished sections via hash sidecars ([`Resolver`]).
// Output is byte-identical to the one-pass build: both derive every
// artifact from the same byte-sorted sections and the same
// depth-then-record-sorted triple-term table.
// ---------------------------------------------------------------------------

/// The deduped set of triple-term byte strings collected during pass 1.
pub(crate) type TtSet = HashSet<Box<[u8]>>;

/// Pass-1 term collector: [`SHARDS`] mutex-guarded [`TermSpiller`]s (by
/// content hash, like [`ShardedDict`]) plus a deduped set of triple-term
/// byte strings (triple terms are rare; their table stays in memory in
/// both build modes).
#[derive(Debug)]
pub(crate) struct SpillerSet {
    shards: Vec<Mutex<TermSpiller>>,
    tts: Mutex<TtSet>,
}

impl SpillerSet {
    /// `budget` divides across shards (floor 64 KiB each, matching the
    /// one-shard granularity of [`TermSpiller`]).
    pub fn new(scratch: PathBuf, budget: usize) -> SpillerSet {
        SpillerSet {
            shards: (0..SHARDS)
                .map(|s| Mutex::new(TermSpiller::new(scratch.clone(), budget / SHARDS, s)))
                .collect(),
            tts: Mutex::default(),
        }
    }

    /// Record an IRI / blank-node occurrence (also triple-term
    /// subjects/predicates). Mirrors [`ShardedDict::intern_node`] minus
    /// the id.
    pub fn add_node(&self, bytes: &[u8], role: u8) {
        let s = xxhash_rust::xxh3::xxh3_64(bytes) as usize & (SHARDS - 1);
        self.shards[s]
            .lock()
            .expect("no poisoned spill shard")
            .add(bytes, role);
    }

    /// Record an object occurrence: inline-able literals are skipped (they
    /// never enter the dictionary), triple terms recurse into components
    /// and join the tt set. Mirrors [`ShardedDict::intern_object`].
    pub fn add_object(&self, bytes: &[u8]) -> Result<(), String> {
        match concise::decode(bytes).map_err(|e| e.to_string())? {
            TermRef::Literal(l) => {
                if l.lang().is_none() && TermId::try_inline(l.lexical(), l.datatype()).is_some() {
                    return Ok(());
                }
                self.add_node(bytes, R_OBJ);
                Ok(())
            }
            TermRef::Iri(_) | TermRef::BlankNode(_) => {
                self.add_node(bytes, R_OBJ);
                Ok(())
            }
            TermRef::TripleTerm(_) => {
                let (s, p, o) = split_triple_term(bytes)?;
                self.add_node(s, R_SUBJ);
                self.add_node(p, R_PRED);
                self.add_object(o)?;
                let mut tts = self.tts.lock().expect("no poisoned tt set");
                if !tts.contains(bytes) {
                    tts.insert(bytes.into());
                }
                Ok(())
            }
        }
    }

    /// Merge the spilled runs + live windows into PFC sections and hand
    /// back the triple-term set.
    pub fn into_sections(self) -> Result<(Sections, TtSet), String> {
        let mut sources = Vec::new();
        for shard in self.shards {
            shard
                .into_inner()
                .expect("no poisoned spill shard")
                .into_sources(&mut sources)?;
        }
        let tts = self.tts.into_inner().expect("no poisoned tt set");
        Ok((merge_sections(sources)?, tts))
    }
}

/// K-way merge byte-sorted term sources straight into PFC sections: the
/// classification half of [`finalize_merge`], with no remap tables (the
/// two-pass build resolves occurrences by lookup instead of by id).
fn merge_sections(mut sources: Vec<Source>) -> Result<Sections, String> {
    let mut b_shared = PfcBuilder::new(32);
    let mut b_subjects = PfcBuilder::new(32);
    let mut b_predicates = PfcBuilder::new(32);
    let mut b_objects = PfcBuilder::new(32);
    let mut b_graphs = PfcBuilder::new(32);

    let mut heap: BinaryHeap<std::cmp::Reverse<(Box<[u8]>, usize)>> = BinaryHeap::new();
    for (i, s) in sources.iter().enumerate() {
        if let Some((b, _, _)) = &s.head {
            heap.push(std::cmp::Reverse((b.clone(), i)));
        }
    }
    while let Some(std::cmp::Reverse((bytes, first))) = heap.pop() {
        let mut roles = 0u8;
        let mut take = |sources: &mut Vec<Source>, i: usize| -> Result<(), String> {
            let s = &mut sources[i];
            let (_, _, r) = s.head.take().expect("head present");
            roles |= r;
            s.advance()
        };
        take(&mut sources, first)?;
        if let Some((b, _, _)) = &sources[first].head {
            heap.push(std::cmp::Reverse((b.clone(), first)));
        }
        while let Some(std::cmp::Reverse((b, i))) = heap.peek() {
            if **b != *bytes {
                break;
            }
            let i = *i;
            heap.pop();
            take(&mut sources, i)?;
            if let Some((b, _, _)) = &sources[i].head {
                heap.push(std::cmp::Reverse((b.clone(), i)));
            }
        }

        // Section membership (same rules as [`finalize_merge`]).
        match (roles & R_SUBJ != 0, roles & R_OBJ != 0) {
            (true, true) => b_shared.push(&bytes),
            (true, false) => b_subjects.push(&bytes),
            (false, true) => b_objects.push(&bytes),
            (false, false) => {}
        }
        if roles & R_PRED != 0 {
            b_predicates.push(&bytes);
        }
        if roles & R_GRAPH != 0 {
            b_graphs.push(&bytes);
        }
    }
    for s in sources {
        if let SourceKind::Run(_, path) = s.kind {
            std::fs::remove_file(path).ok();
        }
    }
    Ok(Sections {
        shared: b_shared.build(),
        subjects: b_subjects.build(),
        predicates: b_predicates.build(),
        objects: b_objects.build(),
        graphs: b_graphs.build(),
    })
}

/// Pass-2 term→final-id resolution over finished sections: O(1) hash
/// sidecars (docs/08 §4 — the same structure persisted as `dict/*.hash`)
/// in front of each PFC, plus the resolved triple-term table.
pub(crate) struct Resolver<'a> {
    sections: &'a Sections,
    n_shared: u64,
    side_shared: HashSidecar,
    side_subjects: HashSidecar,
    side_predicates: HashSidecar,
    side_objects: HashSidecar,
    side_graphs: HashSidecar,
    /// Full tt byte string → final `TT_TAG | ordinal`.
    tt_ids: HashMap<Box<[u8]>, u64>,
}

impl<'a> Resolver<'a> {
    pub fn new(sections: &'a Sections) -> Resolver<'a> {
        Resolver {
            n_shared: sections.shared.len() as u64,
            side_shared: HashSidecar::build(&sections.shared),
            side_subjects: HashSidecar::build(&sections.subjects),
            side_predicates: HashSidecar::build(&sections.predicates),
            side_objects: HashSidecar::build(&sections.objects),
            side_graphs: HashSidecar::build(&sections.graphs),
            tt_ids: HashMap::new(),
            sections,
        }
    }

    /// Resolve the deduped triple-term set bottom-up (depth order, records
    /// sorted within each depth — the same deterministic ordinal rule as
    /// [`finalize_merge`], so both build modes emit identical tables).
    /// Returns the final records; inner references stay resolvable via
    /// [`Resolver::object`] afterwards.
    pub fn resolve_tts(&mut self, tts: TtSet) -> Result<Vec<[u64; 3]>, String> {
        fn depth_of(bytes: &[u8]) -> Result<u32, String> {
            let (_, _, o) = split_triple_term(bytes)?;
            if o.first() == Some(&0x09) {
                Ok(1 + depth_of(o)?)
            } else {
                Ok(1)
            }
        }
        let mut by_depth: Vec<(u32, Box<[u8]>)> = tts
            .into_iter()
            .map(|b| depth_of(&b).map(|d| (d, b)))
            .collect::<Result<_, _>>()?;
        by_depth.sort_unstable_by_key(|(d, _)| *d);
        let mut records: Vec<[u64; 3]> = Vec::with_capacity(by_depth.len());
        let mut at = 0;
        while at < by_depth.len() {
            let depth = by_depth[at].0;
            let mut level: Vec<([u64; 3], Box<[u8]>)> = Vec::new();
            while at < by_depth.len() && by_depth[at].0 == depth {
                let bytes = std::mem::take(&mut by_depth[at].1);
                let (s, p, o) = split_triple_term(&bytes)?;
                let record = [self.subject(s)?, self.predicate(p)?, self.object(o)?];
                level.push((record, bytes));
                at += 1;
            }
            level.sort_unstable();
            for (record, bytes) in level {
                self.tt_ids.insert(bytes, TT_TAG | records.len() as u64);
                records.push(record);
            }
        }
        Ok(records)
    }

    fn locate(side: &HashSidecar, pfc: &Pfc, bytes: &[u8]) -> Option<u64> {
        side.locate(bytes, pfc).map(|i| i as u64)
    }

    /// Final subject-column id (shared then subject-only ordinals).
    pub fn subject(&self, bytes: &[u8]) -> Result<u64, String> {
        Self::locate(&self.side_shared, &self.sections.shared, bytes)
            .or_else(|| {
                Self::locate(&self.side_subjects, &self.sections.subjects, bytes)
                    .map(|i| self.n_shared + i)
            })
            .ok_or_else(|| "spilled subject absent from sections".to_owned())
    }

    /// Final predicate-column id.
    pub fn predicate(&self, bytes: &[u8]) -> Result<u64, String> {
        Self::locate(&self.side_predicates, &self.sections.predicates, bytes)
            .ok_or_else(|| "spilled predicate absent from sections".to_owned())
    }

    /// Final graph-column id for a **named** graph (0-based section
    /// ordinal; the caller adds 1 past the default graph).
    pub fn graph(&self, bytes: &[u8]) -> Result<u64, String> {
        Self::locate(&self.side_graphs, &self.sections.graphs, bytes)
            .ok_or_else(|| "spilled graph absent from sections".to_owned())
    }

    /// Final object-column id: inline `TermId`, `TT_TAG | ordinal`, or a
    /// shared/object section ordinal.
    pub fn object(&self, bytes: &[u8]) -> Result<u64, String> {
        match concise::decode(bytes).map_err(|e| e.to_string())? {
            TermRef::Literal(l) => {
                if l.lang().is_none() {
                    if let Some(id) = TermId::try_inline(l.lexical(), l.datatype()) {
                        return Ok(id.raw());
                    }
                }
            }
            TermRef::TripleTerm(_) => {
                return self
                    .tt_ids
                    .get(bytes)
                    .copied()
                    .ok_or_else(|| "unresolved triple term in object position".to_owned());
            }
            TermRef::Iri(_) | TermRef::BlankNode(_) => {}
        }
        Self::locate(&self.side_shared, &self.sections.shared, bytes)
            .or_else(|| {
                Self::locate(&self.side_objects, &self.sections.objects, bytes)
                    .map(|i| self.n_shared + i)
            })
            .ok_or_else(|| "spilled object absent from sections".to_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use graphy_core::Term;

    fn iri(s: &str) -> Vec<u8> {
        Term::iri(s).unwrap().as_concise().to_vec()
    }

    fn lit(s: &str) -> Vec<u8> {
        Term::literal_simple(s).as_concise().to_vec()
    }

    fn typed(lex: &str, dt: &str) -> Vec<u8> {
        Term::literal_typed(lex, dt).unwrap().as_concise().to_vec()
    }

    #[test]
    fn classification_and_remap() {
        let mut d = BuildDict::new();
        // a: subject only; b: subject+object (shared); p: predicate;
        // p also used as subject (both spaces); g: graph.
        let a = d.intern_node(&iri("http://x/a"), R_SUBJ);
        let b1 = d.intern_node(&iri("http://x/b"), R_SUBJ);
        let b2 = d.intern_object(&iri("http://x/b")).unwrap();
        let p = d.intern_node(&iri("http://x/p"), R_PRED);
        let p_subj = d.intern_node(&iri("http://x/p"), R_SUBJ);
        let g = d.intern_node(&iri("http://x/g"), R_GRAPH);
        // Longer than 7 bytes so it lands in the dictionary even with the
        // `inline-short-strings` feature enabled.
        let lit_o = d.intern_object(&lit("hello dictionary")).unwrap();
        assert_eq!(b1, b2);
        assert_eq!(p, p_subj); // same provisional entry, both roles

        let f = d.finalize().unwrap();
        // shared = {b}; subjects = {a, p}; objects = {hello}; predicates = {p}.
        assert_eq!(f.sections.shared.len(), 1);
        assert_eq!(f.sections.subjects.len(), 2);
        assert_eq!(f.sections.objects.len(), 1);
        assert_eq!(f.sections.predicates.len(), 1);
        assert_eq!(f.sections.graphs.len(), 1);

        // b is shared → same dense id in both spaces.
        let b_s = f.map(b1, Pos::Subject).unwrap();
        let b_o = f.map(b1, Pos::Object).unwrap();
        assert_eq!(b_s, b_o);
        assert!(b_s < 1); // only one shared term
                          // a and p follow in the subject space.
        assert!(f.map(a, Pos::Subject).unwrap() >= 1);
        assert_eq!(f.map(p, Pos::Predicate).unwrap(), 0);
        assert_eq!(f.map(g, Pos::Graph).unwrap(), 0);
        // hello sits in the object space above shared.
        assert_eq!(f.map(lit_o, Pos::Object).unwrap(), 1);
        // Roles don't leak across spaces.
        assert!(f.map(a, Pos::Object).is_err());
        assert!(f.map(lit_o, Pos::Subject).is_err());
    }

    #[test]
    fn inline_bypasses_dictionary() {
        let mut d = BuildDict::new();
        let n = d
            .intern_object(&typed("42", graphy_core::vocab::XSD_INTEGER))
            .unwrap();
        // Tag bits of an inline integer TermId.
        assert_eq!(n >> 60, 0x1);
        // Non-canonical numeral goes to the dictionary instead.
        let dict = d
            .intern_object(&typed("042", graphy_core::vocab::XSD_INTEGER))
            .unwrap();
        assert_eq!(dict >> 60, 0x8);
        let f = d.finalize().unwrap();
        assert_eq!(f.sections.objects.len(), 1);
        assert_eq!(f.map(n, Pos::Object).unwrap(), n); // passes through
    }

    #[test]
    fn sharded_matches_serial() {
        // Same term set interned serially and via concurrent sharded lanes
        // must finalize to identical sections and identical final column
        // values for every term/position.
        let terms: Vec<(Vec<u8>, u8)> = (0..500)
            .map(|i| {
                let bytes = iri(&format!("http://x/t{}", i % 200));
                let role = match i % 4 {
                    0 => R_SUBJ,
                    1 => R_PRED,
                    2 => R_OBJ,
                    _ => R_GRAPH,
                };
                (bytes, role)
            })
            .collect();
        let lits: Vec<Vec<u8>> = (0..100)
            .map(|i| lit(&format!("value {}", i % 40)))
            .collect();

        let mut serial = BuildDict::new();
        let sharded = ShardedDict::new();
        let mut serial_refs = Vec::new();
        for (bytes, role) in &terms {
            serial_refs.push((serial.intern_node(bytes, *role), *role));
        }
        for l in &lits {
            serial_refs.push((serial.intern_object(l).unwrap(), R_OBJ));
        }
        // Sharded interning from 4 threads over interleaved slices.
        std::thread::scope(|scope| {
            for t in 0..4 {
                let (terms, lits, sharded) = (&terms, &lits, &sharded);
                scope.spawn(move || {
                    for (bytes, role) in terms.iter().skip(t).step_by(4) {
                        sharded.intern_node(bytes, *role);
                    }
                    for l in lits.iter().skip(t).step_by(4) {
                        sharded.intern_object(l).unwrap();
                    }
                });
            }
        });
        // Re-intern single-threaded to capture refs (idempotent).
        let mut sharded_refs = Vec::new();
        for (bytes, role) in &terms {
            sharded_refs.push((sharded.intern_node(bytes, *role), *role));
        }
        for l in &lits {
            sharded_refs.push((sharded.intern_object(l).unwrap(), R_OBJ));
        }

        let fs = serial.finalize().unwrap();
        let fp = sharded.finalize().unwrap();
        for (name, a, b) in [
            ("shared", &fs.sections.shared, &fp.sections.shared),
            ("subjects", &fs.sections.subjects, &fp.sections.subjects),
            (
                "predicates",
                &fs.sections.predicates,
                &fp.sections.predicates,
            ),
            ("objects", &fs.sections.objects, &fp.sections.objects),
            ("graphs", &fs.sections.graphs, &fp.sections.graphs),
        ] {
            assert_eq!(a.len(), b.len(), "{name} size");
            for i in 0..a.len() {
                assert_eq!(a.get(i), b.get(i), "{name}[{i}]");
            }
        }
        // Final column values agree term-for-term.
        for ((ra, role), (rb, _)) in serial_refs.iter().zip(&sharded_refs) {
            let pos = match *role {
                R_SUBJ => Pos::Subject,
                R_PRED => Pos::Predicate,
                R_GRAPH => Pos::Graph,
                _ => Pos::Object,
            };
            assert_eq!(
                fs.map(*ra, pos).unwrap(),
                fp.map(*rb, pos).unwrap(),
                "column value for {pos:?}"
            );
        }
    }

    #[test]
    fn triple_terms_topological_ordinals() {
        let mut d = BuildDict::new();
        let s = Term::iri("http://x/s").unwrap();
        let p = Term::iri("http://x/p").unwrap();
        let o = Term::literal_simple("v");
        let inner = Term::triple_term(&s, &p, &o).unwrap();
        let outer = Term::triple_term(&s, &p, &inner).unwrap();
        let r_outer = d.intern_object(outer.as_concise()).unwrap();
        let r_inner = d.intern_object(inner.as_concise()).unwrap();
        assert_eq!(r_outer >> 60, 0x9);
        let f = d.finalize().unwrap();
        assert_eq!(f.tt_records.len(), 2);
        let inner_final = f.map(r_inner, Pos::Object).unwrap();
        let outer_final = f.map(r_outer, Pos::Object).unwrap();
        // Depth-1 first.
        assert_eq!(inner_final, TT_TAG);
        assert_eq!(outer_final, TT_TAG | 1);
        // Outer record's object references the inner ordinal.
        assert_eq!(f.tt_records[1][2], inner_final);
    }
}
