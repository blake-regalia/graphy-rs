//! The vectorized engine (doc 05 §2–5): physical planning over exact
//! leaf counts, pull-based columnar operators, compiled expression fast
//! paths. Semantics are shared with the reference evaluator (doc 05
//! §9's oracle) — the W3C harness dual-runs both engines.

pub(crate) mod batch;
pub(crate) mod cache;
pub mod explain;
pub(crate) mod expr;
pub(crate) mod ops;
pub(crate) mod plan;
pub(crate) mod scheduler;

pub use explain::{explain, explain_analyze};

use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering};
use std::sync::Arc;
use std::time::Instant;

/// Thread backend switch (docs/11 §6): std natively; the `wasm-threads`
/// build swaps in `wasm_thread` (web-worker backed, requires the atomics
/// target features). Without that feature on wasm, the scheduler's inline
/// single-worker path means `scope` is never reached.
pub(crate) mod graphy_thread {
    #[cfg(not(all(target_arch = "wasm32", feature = "wasm-threads")))]
    pub(crate) use std::thread::scope;
    #[cfg(all(target_arch = "wasm32", feature = "wasm-threads"))]
    pub(crate) use wasm_thread::scope;
}

use graphy_algebra::{Form, TranslatedQuery, VarTable};
use graphy_store::Snapshot;

use crate::eval::{self, DatasetView, Evaluator, Row, Scope};
use crate::exec::ops::{build, Cx};
use crate::exec::plan::{plan, PScope, PlanError};
use crate::{EngineError, Output};

/// Marker: fall back to the reference evaluator (carries the construct
/// name for strict-mode error messages).
struct Fallback(&'static str);

/// Per-query execution options (doc 05 §2): cancellation, deadline,
/// memory budget, and pool sizing. `Default` = unbounded sequential-
/// or-parallel-by-size execution.
#[derive(Debug, Clone, Default)]
pub struct ExecOptions {
    /// Cooperative cancellation, checked at every batch boundary.
    pub cancel: Option<Arc<AtomicBool>>,
    /// Absolute deadline, checked at every batch boundary.
    pub deadline: Option<Instant>,
    /// Materialized-row budget in bytes (blocking operators charge it;
    /// exceeding fails the query — spill lands with M10 hardening).
    pub mem_budget: Option<usize>,
    /// Worker threads (0 = all cores).
    pub threads: usize,
    /// Driving-scan exact count at which a bind-join chain engages the
    /// morsel pool (small queries run inline, doc 05 §2).
    pub parallel_threshold: Option<u64>,
}

/// Shared runtime control state (checked/charged by every operator).
pub(crate) struct Ctl {
    cancel: Option<Arc<AtomicBool>>,
    deadline: Option<Instant>,
    budget: Option<Arc<AtomicIsize>>,
    threads: usize,
    pub(crate) parallel_threshold: u64,
    /// ANALYZE meter registry (None outside explain_analyze).
    meters: Option<explain::Meters>,
}

impl Ctl {
    fn new(opts: &ExecOptions) -> Arc<Ctl> {
        Arc::new(Ctl {
            cancel: opts.cancel.clone(),
            deadline: opts.deadline,
            budget: opts
                .mem_budget
                .map(|b| Arc::new(AtomicIsize::new(b as isize))),
            threads: if opts.threads == 0 {
                std::thread::available_parallelism().map_or(1, |n| n.get())
            } else {
                opts.threads
            },
            parallel_threshold: opts.parallel_threshold.unwrap_or(65_536),
            meters: None,
        })
    }

    fn new_metered(opts: &ExecOptions) -> Arc<Ctl> {
        let mut ctl = Arc::try_unwrap(Ctl::new(opts)).ok().expect("fresh");
        ctl.meters = Some(std::sync::Mutex::new(Vec::new()));
        Arc::new(ctl)
    }

    pub(crate) fn meters(&self) -> Option<&explain::Meters> {
        self.meters.as_ref()
    }

    /// A clone for runtime-rebuilt subtrees (GRAPH ?g enumeration):
    /// same limits, no meter registration (the enumerating node's own
    /// meter carries the subtree's output).
    pub(crate) fn rebuild(self: &Arc<Ctl>) -> Arc<Ctl> {
        Arc::new(Ctl {
            cancel: self.cancel.clone(),
            deadline: self.deadline,
            budget: self.budget.clone(),
            threads: self.threads,
            parallel_threshold: self.parallel_threshold,
            meters: None,
        })
    }

    pub(crate) fn check(&self) -> Result<(), EngineError> {
        if let Some(c) = &self.cancel {
            if c.load(Ordering::Relaxed) {
                return Err(EngineError("query cancelled".into()));
            }
        }
        if let Some(d) = self.deadline {
            if Instant::now() >= d {
                return Err(EngineError("query deadline exceeded".into()));
            }
        }
        Ok(())
    }

    pub(crate) fn charge(&self, bytes: usize) -> Result<(), EngineError> {
        if let Some(b) = &self.budget {
            if b.fetch_sub(bytes as isize, Ordering::Relaxed) - (bytes as isize) < 0 {
                return Err(EngineError("query memory budget exceeded".into()));
            }
        }
        Ok(())
    }

    pub(crate) fn threads(&self) -> usize {
        self.threads
    }
}

/// Evaluate a translated query: vectorized engine first, reference
/// evaluator for the (deliberately few) constructs the planner rejects
/// (SERVICE and friends — both engines reject them with the same
/// message).
pub fn evaluate(snap: &Snapshot, q: &TranslatedQuery) -> Result<Output, EngineError> {
    evaluate_with(snap, q, &ExecOptions::default())
}

/// Evaluate with execution options (cancellation, deadline, memory
/// budget, pool sizing).
pub fn evaluate_with(
    snap: &Snapshot,
    q: &TranslatedQuery,
    opts: &ExecOptions,
) -> Result<Output, EngineError> {
    match evaluate_vectorized(snap, q, opts) {
        Err(Fallback(_)) => eval::evaluate_ref(snap, q),
        Ok(out) => out,
    }
}

/// Run strictly through the vectorized engine — no fallback; planner
/// gaps surface as errors. The dual-run conformance gate calls this.
pub fn evaluate_vec(snap: &Snapshot, q: &TranslatedQuery) -> Result<Output, EngineError> {
    match evaluate_vectorized(snap, q, &ExecOptions::default()) {
        Ok(out) => out,
        Err(Fallback(what)) => Err(EngineError(format!(
            "vectorized engine: unsupported construct: {what}"
        ))),
    }
}

fn evaluate_vectorized(
    snap: &Snapshot,
    q: &TranslatedQuery,
    opts: &ExecOptions,
) -> Result<Result<Output, EngineError>, Fallback> {
    run_vectorized(snap, q, opts, Ctl::new(opts)).map(|r| r.map(|(out, _)| out))
}

/// ANALYZE driver: metered Ctl, plan rendered from the meters.
pub(crate) fn run_analyzed(
    snap: &Snapshot,
    q: &TranslatedQuery,
    opts: &ExecOptions,
) -> Result<(Output, String), EngineError> {
    match run_vectorized(snap, q, opts, Ctl::new_metered(opts)) {
        Ok(Ok((out, plan_text))) => Ok((out, plan_text.expect("metered"))),
        Ok(Err(e)) => Err(e),
        Err(Fallback(what)) => Err(EngineError(format!("cannot analyze: unsupported {what}"))),
    }
}

fn run_vectorized(
    snap: &Snapshot,
    q: &TranslatedQuery,
    _opts: &ExecOptions,
    ctl: Arc<Ctl>,
) -> Result<Result<(Output, Option<String>), EngineError>, Fallback> {
    let dataset = match eval::dataset_view(snap, &q.dataset) {
        Ok(d) => d,
        Err(e) => return Ok(Err(e)),
    };
    let mut ev = Evaluator::new(snap, &q.vars, q.base.clone(), dataset);
    let phys = match cache::plan_cached(&ev, snap, q) {
        Ok(p) => p,
        Err(PlanError::Unsupported(what)) => return Err(Fallback(what)),
        Err(PlanError::Engine(e)) => return Ok(Err(e)),
    };
    let nvars = q.vars.len();
    let mut op = build(&phys, Scope::Default, nvars, &ev, &ctl, 0);
    let ask = matches!(q.form, Form::Ask);
    let mut rows: Vec<Row> = Vec::new();
    let result = (|| -> Result<(), EngineError> {
        let mut cx = Cx {
            ev: &mut ev,
            ctl: ctl.clone(),
        };
        while let Some(b) = op.next(&mut cx)? {
            cx.ctl.check()?;
            for i in 0..b.len {
                rows.push(b.row_at(i));
            }
            if ask && !rows.is_empty() {
                break;
            }
        }
        Ok(())
    })();
    drop(op);
    if let Err(e) = result {
        return Ok(Err(e));
    }
    let plan_text = ctl.meters().map(explain::render_meters);
    Ok(eval::finish(&mut ev, q, rows).map(|out| (out, plan_text)))
}

/// Vectorized twin of `eval::evaluate_rows` (update WHERE evaluation):
/// rows stay in binding space; the returned evaluator decodes them (the
/// update executor memoizes decodes across rows).
pub(crate) fn evaluate_rows_vec<'s>(
    snap: &'s Snapshot,
    vars: &'s VarTable,
    root: &graphy_algebra::Algebra,
    dataset: DatasetView,
    root_scope_named: Option<u64>,
) -> Result<(Vec<Row>, Evaluator<'s>), EngineError> {
    let scope = match root_scope_named {
        Some(col) => Scope::Named(col),
        None => Scope::Default,
    };
    let mut ev = Evaluator::new(snap, vars, None, dataset.clone());
    let phys = match plan(&ev, root, PScope::Fixed(scope)) {
        Ok(p) => p,
        Err(PlanError::Unsupported(_)) => {
            return eval::evaluate_rows(snap, vars, root, dataset, root_scope_named)
        }
        Err(PlanError::Engine(e)) => return Err(e),
    };
    let nvars = vars.len();
    let ctl = Ctl::new(&ExecOptions::default());
    let mut rows: Vec<Row> = Vec::new();
    {
        let mut op = build(&phys, scope, nvars, &ev, &ctl, 0);
        let mut cx = Cx {
            ev: &mut ev,
            ctl: ctl.clone(),
        };
        while let Some(b) = op.next(&mut cx)? {
            for i in 0..b.len {
                rows.push(b.row_at(i));
            }
        }
    }
    Ok((rows, ev))
}
