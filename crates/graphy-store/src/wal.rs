//! Write-ahead log (doc 07 §4, M4): single per-store log of length-prefixed,
//! xxh3-checksummed records. Terms are **concise strings**, never ids — the
//! WAL must stay meaningful across generations (ids are generation-local).
//!
//! Record stream grammar: a transaction is `BeginTx (Quad)* CommitTx(epoch)`;
//! `Checkpoint(epoch)` marks every transaction with `tx_epoch ≤ epoch` as
//! folded into the base — replay skips those and still delivers later ones
//! (commits that landed *while* a merge ran carry epochs above the freeze
//! epoch and must survive it). `MergeStart(epoch)` / `MergeCommit(gen)` are
//! observability brackets the merger writes; replay ignores them.
//!
//! Frame: `[payload_len u32][xxh3_64(payload) u64][payload]`, payload =
//! `[tag u8][fields…]`. Recovery replays committed transactions in order and
//! **truncates** any torn tail (unterminated transaction, short frame, or
//! checksum mismatch) back to the last commit boundary — `CommitTx` is the
//! only record that makes a transaction real.

use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use xxhash_rust::xxh3::xxh3_64;

use crate::format::StoreError;

pub(crate) const WAL_NAME: &str = "wal.log";
/// Staging name for a rotated log (see [`stage_open`]); a leftover one is
/// crash debris and is removed at store open.
pub(crate) const WAL_TMP_NAME: &str = "wal.log.tmp";

const TAG_BEGIN: u8 = 1;
const TAG_QUAD: u8 = 2;
const TAG_COMMIT: u8 = 3;
const TAG_CHECKPOINT: u8 = 4;
const TAG_MERGE_START: u8 = 5;
const TAG_MERGE_COMMIT: u8 = 6;

/// One logged operation: kind + owned concise terms.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct WalOp {
    pub del: bool,
    pub s: Vec<u8>,
    pub p: Vec<u8>,
    pub o: Vec<u8>,
    pub g: Option<Vec<u8>>,
}

/// Commit durability (doc 07 §4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Durability {
    /// fsync before the commit is applied/published.
    Strict,
    /// Skip the fsync (bulk ingestion; documented crash-loss window).
    Relaxed,
}

/// Append handle for the store's log. Transactions buffer in `pending`
/// until [`Wal::commit_group`] writes them as one unit; a failed group
/// write truncates back to the last complete group so later commits never
/// land after a torn frame (which would orphan them at replay).
#[derive(Debug)]
pub(crate) struct Wal {
    /// `None` = no backing file (ephemeral stores, docs/11): committed
    /// groups are discarded — or moved to `captured` in capture mode.
    file: Option<File>,
    /// Capture mode (docs/11 OPFS persistence): committed frames accumulate
    /// here for the host to drain and persist.
    captured: Option<Vec<u8>>,
    path: PathBuf,
    /// Frames buffered since the last group commit.
    pending: Vec<u8>,
    /// File length after the last successfully written group.
    written_len: u64,
    /// Set when a failed write could not be rolled back — the log tail is
    /// unusable and further appends must fail.
    poisoned: bool,
}

fn io_err(path: &Path) -> impl Fn(io::Error) -> StoreError + '_ {
    move |e| StoreError::io(path, e)
}

/// Release outsized buffer capacity after a group lands: one bulk commit
/// (a whole-model INSERT) would otherwise pin its frame bytes as dead
/// `pending` capacity for the store's lifetime — real memory on wasm32,
/// where the linear heap never shrinks either way but the allocator can
/// at least reuse it.
fn trim_pending(pending: &mut Vec<u8>) {
    const KEEP: usize = 1 << 20;
    if pending.capacity() > KEEP {
        pending.shrink_to(KEEP);
    }
}

fn frame(payload: &[u8], out: &mut Vec<u8>) {
    out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    out.extend_from_slice(&xxh3_64(payload).to_le_bytes());
    out.extend_from_slice(payload);
}

fn put_bytes(payload: &mut Vec<u8>, b: &[u8]) {
    payload.extend_from_slice(&(b.len() as u32).to_le_bytes());
    payload.extend_from_slice(b);
}

impl Wal {
    /// Open (create) the log for appending at `valid_len`, truncating any
    /// torn tail found by [`replay`].
    pub fn open_append(dir: &Path, valid_len: u64) -> Result<Wal, StoreError> {
        let path = dir.join(WAL_NAME);
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&path)
            .map_err(io_err(&path))?;
        file.set_len(valid_len).map_err(io_err(&path))?;
        let mut file = file;
        use std::io::Seek as _;
        file.seek(io::SeekFrom::End(0)).map_err(io_err(&path))?;
        Ok(Wal {
            file: Some(file),
            captured: None,
            path,
            pending: Vec::new(),
            written_len: valid_len,
            poisoned: false,
        })
    }

    /// A null log: everything buffered is dropped at commit time. Durability
    /// is meaningless for ephemeral stores; recovery never runs on them.
    pub fn null() -> Wal {
        Wal {
            file: None,
            captured: None,
            path: PathBuf::from("<null wal>"),
            pending: Vec::new(),
            written_len: 0,
            poisoned: false,
        }
    }

    /// A capturing log: committed groups accumulate in memory for the host
    /// to [`take_captured`](Self::take_captured) and persist (OPFS etc.).
    pub fn capture() -> Wal {
        Wal {
            file: None,
            captured: Some(Vec::new()),
            path: PathBuf::from("<captured wal>"),
            pending: Vec::new(),
            written_len: 0,
            poisoned: false,
        }
    }

    /// Drain the captured committed frames (capture mode; empty otherwise).
    pub fn take_captured(&mut self) -> Vec<u8> {
        self.captured
            .as_mut()
            .map(std::mem::take)
            .unwrap_or_default()
    }

    /// Buffer one transaction (`BeginTx`, its ops, `CommitTx(epoch)`) — the
    /// group-commit leader batches several and pays one
    /// [`Wal::commit_group`] for all of them (doc 07 §4). Nothing is
    /// buffered for empty transactions (callers skip those entirely).
    pub fn append_tx(&mut self, epoch: u64, ops: &[WalOp]) {
        self.append_tx_terms(
            epoch,
            ops.iter().map(|op| {
                (
                    op.del,
                    op.s.as_slice(),
                    op.p.as_slice(),
                    op.o.as_slice(),
                    op.g.as_deref(),
                )
            }),
        );
    }

    /// [`Wal::append_tx`] over borrowed terms — the commit path encodes a
    /// large transaction's frames directly from the caller's slices, never
    /// materializing an owned op list.
    pub fn append_tx_terms<'a>(
        &mut self,
        epoch: u64,
        ops: impl IntoIterator<Item = (bool, &'a [u8], &'a [u8], &'a [u8], Option<&'a [u8]>)>,
    ) {
        frame(&[TAG_BEGIN], &mut self.pending);
        let mut payload = Vec::new();
        let mut any = false;
        for (del, s, p, o, g) in ops {
            any = true;
            payload.clear();
            payload.push(TAG_QUAD);
            payload.push(u8::from(del));
            put_bytes(&mut payload, s);
            put_bytes(&mut payload, p);
            put_bytes(&mut payload, o);
            match g {
                None => payload.push(0),
                Some(g) => {
                    payload.push(1);
                    put_bytes(&mut payload, g);
                }
            }
            frame(&payload, &mut self.pending);
        }
        debug_assert!(any, "empty commits are not logged");
        payload.clear();
        payload.push(TAG_COMMIT);
        payload.extend_from_slice(&epoch.to_le_bytes());
        frame(&payload, &mut self.pending);
    }

    /// Buffer a `MergeStart(freeze_epoch)` bracket (doc 07 §6.1; pure
    /// observability — replay skips it).
    pub fn append_merge_start(&mut self, freeze_epoch: u64) {
        let mut payload = vec![TAG_MERGE_START];
        payload.extend_from_slice(&freeze_epoch.to_le_bytes());
        frame(&payload, &mut self.pending);
    }

    /// Write and (under [`Durability::Strict`]) fsync everything buffered by
    /// [`Wal::append_tx`], as one unit. On failure the file is rolled back
    /// to the last complete group.
    pub fn commit_group(&mut self, durability: Durability) -> Result<(), StoreError> {
        if self.poisoned {
            return Err(StoreError::Corrupt(format!(
                "{}: log tail unusable after a failed write",
                self.path.display()
            )));
        }
        if self.pending.is_empty() {
            return Ok(());
        }
        let Some(file) = &mut self.file else {
            match &mut self.captured {
                Some(cap) => cap.append(&mut self.pending),
                None => self.pending.clear(),
            }
            trim_pending(&mut self.pending);
            return Ok(());
        };
        let err = io_err(&self.path);
        let result = (|| -> io::Result<()> {
            file.write_all(&self.pending)?;
            if durability == Durability::Strict {
                file.sync_data()?;
            }
            Ok(())
        })();
        match result {
            Ok(()) => {
                self.written_len += self.pending.len() as u64;
                self.pending.clear();
                trim_pending(&mut self.pending);
                Ok(())
            }
            Err(e) => {
                self.pending.clear();
                // Roll the file back so later commits never follow a torn
                // frame (replay would stop at the tear and orphan them).
                use std::io::Seek as _;
                if file.set_len(self.written_len).is_err()
                    || file.seek(io::SeekFrom::Start(self.written_len)).is_err()
                {
                    self.poisoned = true;
                }
                Err(err(e))
            }
        }
    }
}

/// An in-progress rotated-log staging at [`WAL_TMP_NAME`] (doc 07 §6.1,
/// §6.3(b)): opens with `MergeCommit(gen)` + `Checkpoint(freeze_epoch)`,
/// then accumulates surviving transactions — the commits whose epochs
/// exceed the freeze epoch, re-serialized from the delta's suffix — across
/// the merge's *shadow passes* (flushed, not fsynced, so staging IO happens
/// while commits still flow). [`RotationStage::finish`] appends nothing
/// more, fsyncs, and hands back the staged path; the caller renames it live
/// with [`activate_rotated`] only after the generation pointer is durable
/// (a crash in between replays the *old* full log against the new base,
/// which the commit core's set semantics make a no-op-safe re-application).
#[derive(Debug)]
pub(crate) struct RotationStage {
    wal: Wal,
    freeze_epoch: u64,
}

pub(crate) fn stage_open(
    dir: &Path,
    generation: u64,
    freeze_epoch: u64,
) -> Result<RotationStage, StoreError> {
    let path = dir.join(WAL_TMP_NAME);
    let err = io_err(&path);
    let mut buf = Vec::new();
    let mut payload = vec![TAG_MERGE_COMMIT];
    payload.extend_from_slice(&generation.to_le_bytes());
    frame(&payload, &mut buf);
    payload.clear();
    payload.push(TAG_CHECKPOINT);
    payload.extend_from_slice(&freeze_epoch.to_le_bytes());
    frame(&payload, &mut buf);
    Ok(RotationStage {
        wal: Wal {
            file: Some(File::create(&path).map_err(err)?),
            captured: None,
            path,
            pending: buf,
            written_len: 0,
            poisoned: false,
        },
        freeze_epoch,
    })
}

impl RotationStage {
    /// Append surviving transactions and flush them to the staging file
    /// (no fsync — durability is [`RotationStage::finish`]'s job).
    pub fn append_txs(&mut self, txs: &[(u64, Vec<WalOp>)]) -> Result<(), StoreError> {
        for (epoch, ops) in txs {
            debug_assert!(*epoch > self.freeze_epoch, "folded tx in rotated log");
            self.wal.append_tx(*epoch, ops);
        }
        self.wal.commit_group(Durability::Relaxed)
    }

    /// Make the staged log durable and return its path (still NOT live).
    pub fn finish(mut self) -> Result<PathBuf, StoreError> {
        self.wal.commit_group(Durability::Strict)?;
        Ok(self.wal.path)
    }
}

/// Rename a staged rotated log over `wal.log` and return a fresh append
/// handle at its end.
pub(crate) fn activate_rotated(dir: &Path, staged: &Path) -> Result<Wal, StoreError> {
    let path = dir.join(WAL_NAME);
    let run = || -> io::Result<u64> {
        let len = staged.metadata()?.len();
        std::fs::rename(staged, &path)?;
        File::open(dir)?.sync_all()?;
        Ok(len)
    };
    let len = run().map_err(|e| StoreError::io(&path, e))?;
    Wal::open_append(dir, len)
}

/// Replay outcome: the byte length of the valid prefix (append point after
/// truncating a torn tail) and the newest checkpoint's folded epoch (0 when
/// the log has no checkpoint — the store's epoch floor).
#[derive(Debug, Clone, Copy)]
pub(crate) struct Replayed {
    pub valid_len: u64,
    pub checkpoint_epoch: u64,
}

/// Committed transactions delivered to `on_commit` in order, minus those a
/// checkpoint marks as folded into the base.
pub(crate) fn replay(
    dir: &Path,
    on_commit: impl FnMut(u64, &[WalOp]) -> Result<(), StoreError>,
) -> Result<Replayed, StoreError> {
    let path = dir.join(WAL_NAME);
    let bytes = match std::fs::read(&path) {
        Ok(b) => b,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            return Ok(Replayed {
                valid_len: 0,
                checkpoint_epoch: 0,
            })
        }
        Err(e) => return Err(StoreError::io(&path, e)),
    };
    replay_bytes(&bytes, &path.display().to_string(), on_commit)
}

/// [`replay`] over an in-memory log image (docs/11 OPFS persistence: the
/// host hands back the bytes it captured). Torn tails — e.g. an interrupted
/// append — truncate exactly like the on-disk path.
pub(crate) fn replay_bytes(
    bytes: &[u8],
    ctx: &str,
    mut on_commit: impl FnMut(u64, &[WalOp]) -> Result<(), StoreError>,
) -> Result<Replayed, StoreError> {
    let bad = |m: &str, at: usize| StoreError::Corrupt(format!("{ctx}: {m} at byte {at}"));

    let mut at = 0usize;
    let mut valid = 0u64; // end of the last complete, in-grammar record run
    let mut pending: Option<Vec<WalOp>> = None;
    let mut checkpoint_epoch = 0u64;
    // Commits that survive the newest checkpoint (epochs above its fold).
    let mut committed: Vec<(u64, Vec<WalOp>)> = Vec::new();

    // Loop ends at clean EOF or a short header (torn tail).
    while let Some(header) = bytes.get(at..at + 12) {
        let len = u32::from_le_bytes(header[0..4].try_into().expect("4 bytes")) as usize;
        let digest = u64::from_le_bytes(header[4..12].try_into().expect("8 bytes"));
        let Some(payload) = bytes.get(at + 12..at + 12 + len) else {
            break; // short frame → torn tail
        };
        if xxh3_64(payload) != digest || payload.is_empty() {
            break; // corrupt frame → torn tail
        }
        let rec_end = at + 12 + len;
        match payload[0] {
            TAG_BEGIN => {
                if pending.is_some() {
                    break; // nested BeginTx: unterminated predecessor
                }
                pending = Some(Vec::new());
            }
            TAG_QUAD => {
                let Some(tx) = pending.as_mut() else {
                    break; // quad outside a transaction
                };
                let mut p = &payload[1..];
                let take = |p: &mut &[u8]| -> Option<Vec<u8>> {
                    let len =
                        u32::from_le_bytes(p.get(0..4)?.try_into().expect("4 bytes")) as usize;
                    let b = p.get(4..4 + len)?.to_vec();
                    *p = &p[4 + len..];
                    Some(b)
                };
                let parse = |p: &mut &[u8]| -> Option<WalOp> {
                    let del = match p.first()? {
                        0 => false,
                        1 => true,
                        _ => return None,
                    };
                    *p = &p[1..];
                    let (s, pp, o) = (take(p)?, take(p)?, take(p)?);
                    let g = match p.first()? {
                        0 => {
                            *p = &p[1..];
                            None
                        }
                        1 => {
                            *p = &p[1..];
                            Some(take(p)?)
                        }
                        _ => return None,
                    };
                    p.is_empty().then_some(WalOp {
                        del,
                        s,
                        p: pp,
                        o,
                        g,
                    })
                };
                match parse(&mut p) {
                    Some(op) => tx.push(op),
                    // A checksummed frame that does not parse is a writer
                    // bug, not bit rot — surface it.
                    None => return Err(bad("malformed quad record", at)),
                }
            }
            TAG_COMMIT => {
                let Some(ops) = pending.take() else {
                    break; // commit outside a transaction
                };
                let epoch = u64::from_le_bytes(
                    payload
                        .get(1..9)
                        .ok_or_else(|| bad("malformed commit record", at))?
                        .try_into()
                        .expect("8 bytes"),
                );
                committed.push((epoch, ops));
                valid = rec_end as u64;
            }
            TAG_CHECKPOINT => {
                if pending.is_some() {
                    break;
                }
                // Transactions at or below the folded epoch are in the base;
                // later ones (commits during the merge window) still replay.
                let f = u64::from_le_bytes(
                    payload
                        .get(1..9)
                        .ok_or_else(|| bad("malformed checkpoint record", at))?
                        .try_into()
                        .expect("8 bytes"),
                );
                committed.retain(|(e, _)| *e > f);
                checkpoint_epoch = checkpoint_epoch.max(f);
                valid = rec_end as u64;
            }
            TAG_MERGE_START | TAG_MERGE_COMMIT => {
                if pending.is_some() {
                    break; // merge bracket inside a transaction: tail damage
                }
                valid = rec_end as u64;
            }
            _ => break, // unknown tag → treat as tail damage
        }
        at = rec_end;
    }

    for (epoch, ops) in &committed {
        on_commit(*epoch, ops)?;
    }
    Ok(Replayed {
        valid_len: valid,
        checkpoint_epoch,
    })
}
