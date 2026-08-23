//! Morsel-driven parallelism (doc 05 §2): the driving scan's batches
//! are the morsels; a scoped worker pool runs the pure-ID bind-join
//! chain over them (`scan_rows` needs no evaluator state — workers
//! never touch the dictionary or the ext table). Output preserves
//! morsel order, so results are deterministic and identical to the
//! sequential pipeline. Small queries never reach the pool: the driving
//! scan's exact count gates engagement.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

use graphy_algebra::TriplePat;
use graphy_store::Snapshot;

use crate::eval::{scan_rows, DatasetView, Row, Scope};
use crate::exec::batch::Batch;
use crate::exec::graphy_thread;
use crate::exec::ops::{BoxOp, Cx, Op};
use crate::EngineError;

/// Scan + bind-join chain executed morsel-parallel. The driving scan
/// runs on the coordinator (it owns cross-member dedup state); workers
/// bind-join each morsel independently.
pub(crate) struct ParallelBindOp<'a> {
    pub driving: BoxOp<'a>,
    /// Bind-join patterns in pipeline order.
    pub pats: Vec<TriplePat>,
    pub scope: Scope,
    pub nvars: usize,
    pub threads: usize,
    pub out: Option<VecDeque<Row>>,
}

impl<'a> ParallelBindOp<'a> {
    fn run(&mut self, cx: &mut Cx<'a, '_>) -> Result<VecDeque<Row>, EngineError> {
        // Collect the morsels (driving-scan batches).
        let mut morsels: Vec<Batch> = Vec::new();
        while let Some(b) = self.driving.next(cx)? {
            cx.ctl.charge(b.len * (16 * self.nvars + 48))?;
            morsels.push(b);
        }
        let snap: &'a Snapshot = cx.ev.snap;
        let dataset: DatasetView = cx.ev.dataset.clone();
        let pats = &self.pats;
        let scope = self.scope;
        let ctl = cx.ctl.clone();
        let workers = self.threads.min(morsels.len()).max(1);

        let results: Mutex<Vec<(usize, Vec<Row>)>> = Mutex::new(Vec::new());
        let next_morsel = AtomicUsize::new(0);
        let failure: Mutex<Option<EngineError>> = Mutex::new(None);

        let worker_loop = || {
            let mut local: Vec<(usize, Vec<Row>)> = Vec::new();
            loop {
                let idx = next_morsel.fetch_add(1, Ordering::Relaxed);
                let Some(morsel) = morsels.get(idx) else {
                    break;
                };
                if ctl.check().is_err() {
                    break;
                }
                let run = || -> Result<Vec<Row>, EngineError> {
                    let mut rows: Vec<Row> = (0..morsel.len).map(|i| morsel.row_at(i)).collect();
                    for pat in pats {
                        let mut next: Vec<Row> = Vec::new();
                        for row in &rows {
                            scan_rows(snap, &dataset, pat, &scope, row, &mut next)?;
                        }
                        ctl.charge(next.len() * (16 * rows.first().map_or(0, Vec::len) + 48))?;
                        rows = next;
                        if rows.is_empty() {
                            break;
                        }
                    }
                    Ok(rows)
                };
                match run() {
                    Ok(rows) => local.push((idx, rows)),
                    Err(e) => {
                        *failure.lock().unwrap() = Some(e);
                        break;
                    }
                }
            }
            results.lock().unwrap().extend(local);
        };
        if workers == 1 {
            // Inline on the calling thread: byte-identical semantics with no
            // spawn — also the wasm32 path, where std threads are
            // unavailable (docs/11 §3/§6).
            worker_loop();
        } else {
            graphy_thread::scope(|sc| {
                for _ in 0..workers {
                    sc.spawn(worker_loop);
                }
            });
        }

        if let Some(e) = failure.into_inner().unwrap() {
            return Err(e);
        }
        ctl.check()?;
        // Reassemble in morsel order: deterministic, sequential-identical.
        let mut all = results.into_inner().unwrap();
        all.sort_by_key(|(i, _)| *i);
        Ok(all.into_iter().flat_map(|(_, rows)| rows).collect())
    }
}

impl<'a> Op<'a> for ParallelBindOp<'a> {
    fn next(&mut self, cx: &mut Cx<'a, '_>) -> Result<Option<Batch>, EngineError> {
        if self.out.is_none() {
            let rows = self.run(cx)?;
            self.out = Some(rows);
        }
        let q = self.out.as_mut().unwrap();
        if q.is_empty() {
            return Ok(None);
        }
        let mut out = Batch::new(self.nvars);
        while let Some(r) = q.pop_front() {
            out.push_row(&r);
            if out.is_full() {
                break;
            }
        }
        Ok(Some(out))
    }
}
