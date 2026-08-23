//! EXPLAIN / ANALYZE (doc 05 §5.6): render the physical plan with
//! estimated cardinalities; in ANALYZE mode also execute and report
//! per-operator actual rows and wall time — the optimizer's feedback
//! loop.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use graphy_algebra::{TranslatedQuery, TriplePat, VarTable, P};
use graphy_store::Snapshot;

use crate::eval::{Evaluator, Scope};
use crate::exec::batch::Batch;
use crate::exec::ops::{BoxOp, Cx, Op};
use crate::exec::plan::{PScope, Phys, PlanError};
use crate::exec::{ExecOptions, Output};
use crate::EngineError;

/// One metered operator's counters (ANALYZE).
pub(crate) struct Meter {
    pub label: String,
    pub est: Option<u64>,
    pub depth: usize,
    pub rows: AtomicU64,
    pub nanos: AtomicU64,
}

/// Meter registry carried in `Ctl` when ANALYZE is active.
pub(crate) type Meters = Mutex<Vec<Arc<Meter>>>;

/// Wraps an operator, counting emitted rows and wall time.
pub(crate) struct MeterOp<'a> {
    pub inner: BoxOp<'a>,
    pub meter: Arc<Meter>,
}

impl<'a> Op<'a> for MeterOp<'a> {
    fn next(&mut self, cx: &mut Cx<'a, '_>) -> Result<Option<Batch>, EngineError> {
        let t0 = Instant::now();
        let out = self.inner.next(cx);
        self.meter
            .nanos
            .fetch_add(t0.elapsed().as_nanos() as u64, Ordering::Relaxed);
        if let Ok(Some(b)) = &out {
            self.meter.rows.fetch_add(b.len as u64, Ordering::Relaxed);
        }
        out
    }
}

fn term_str(bytes: &[u8]) -> String {
    let s = match graphy_core::concise::decode(bytes) {
        Ok(graphy_core::TermRef::Iri(i)) => format!("<{i}>"),
        Ok(graphy_core::TermRef::BlankNode(l)) => format!("_:{l}"),
        Ok(graphy_core::TermRef::Literal(l)) => format!("{:?}", l.lexical()),
        Ok(graphy_core::TermRef::TripleTerm(_)) => "<<…>>".into(),
        Err(_) => String::from_utf8_lossy(bytes).into_owned(),
    };
    let mut s = s;
    if s.chars().count() > 42 {
        s = format!("{}…", s.chars().take(40).collect::<String>());
    }
    s
}

fn p_str(p: &P, vars: &VarTable) -> String {
    match p {
        P::Var(v) => format!("?{}", vars.name(*v)),
        P::Term(bytes) => term_str(bytes),
        P::Triple(_) => "<<…>>".into(),
    }
}

fn pat_str(t: &TriplePat, vars: &VarTable) -> String {
    format!(
        "{} {} {}",
        p_str(&t.s, vars),
        p_str(&t.p, vars),
        p_str(&t.o, vars)
    )
}

/// Node label + estimate for one plan node.
pub(crate) fn describe(phys: &Phys, vars: &VarTable) -> (String, Option<u64>) {
    match phys {
        Phys::Unit => ("Unit".into(), None),
        Phys::Empty => ("Empty".into(), Some(0)),
        Phys::Scan { pat, est } => (format!("Scan {}", pat_str(pat, vars)), Some(*est)),
        Phys::BindJoin { pat, est, .. } => (format!("BindJoin {}", pat_str(pat, vars)), Some(*est)),
        Phys::BindPath { s, o, .. } => (
            format!("PathScan {} …path… {}", p_str(s, vars), p_str(o, vars)),
            None,
        ),
        Phys::HashJoin { keys, .. } => (
            format!(
                "HashJoin [{}]",
                keys.iter()
                    .map(|v| format!("?{}", vars.name(*v)))
                    .collect::<Vec<_>>()
                    .join(" ")
            ),
            None,
        ),
        Phys::LeftJoin { keys, .. } => (
            format!(
                "LeftJoin [{}]",
                keys.iter()
                    .map(|v| format!("?{}", vars.name(*v)))
                    .collect::<Vec<_>>()
                    .join(" ")
            ),
            None,
        ),
        Phys::Minus { .. } => ("Minus".into(), None),
        Phys::Filter { .. } => ("Filter".into(), None),
        Phys::Extend { var, .. } => (format!("Extend ?{}", vars.name(*var)), None),
        Phys::Union(..) => ("Union".into(), None),
        Phys::GraphConst { col, .. } => (format!("Graph #{col}"), None),
        Phys::GraphVar { var, .. } => (format!("GraphEnum ?{}", vars.name(*var)), None),
        Phys::Table { rows, .. } => (
            format!("Values ({} rows)", rows.len()),
            Some(rows.len() as u64),
        ),
        Phys::Group {
            keys, aggregates, ..
        } => (
            format!("Group ({} keys, {} aggs)", keys.len(), aggregates.len()),
            None,
        ),
        Phys::Sort { conditions, .. } => (format!("Sort ({} keys)", conditions.len()), None),
        Phys::Project { vars: pv, .. } => (
            format!(
                "Project {}",
                pv.iter()
                    .map(|v| format!("?{}", vars.name(*v)))
                    .collect::<Vec<_>>()
                    .join(" ")
            ),
            None,
        ),
        Phys::Distinct { .. } => ("Distinct".into(), None),
        Phys::Slice { offset, limit, .. } => (
            format!(
                "Slice offset={offset}{}",
                limit.map(|l| format!(" limit={l}")).unwrap_or_default()
            ),
            None,
        ),
    }
}

fn children(phys: &Phys) -> Vec<&Phys> {
    match phys {
        Phys::Unit | Phys::Empty | Phys::Scan { .. } | Phys::Table { .. } => vec![],
        Phys::BindJoin { input, .. }
        | Phys::BindPath { input, .. }
        | Phys::Filter { input, .. }
        | Phys::Extend { input, .. }
        | Phys::GraphConst { input, .. }
        | Phys::GraphVar { input, .. }
        | Phys::Group { input, .. }
        | Phys::Sort { input, .. }
        | Phys::Project { input, .. }
        | Phys::Distinct { input }
        | Phys::Slice { input, .. } => vec![input],
        Phys::HashJoin { left, right, .. }
        | Phys::LeftJoin { left, right, .. }
        | Phys::Minus { left, right } => vec![left, right],
        Phys::Union(l, r) => vec![l, r],
    }
}

fn render(phys: &Phys, vars: &VarTable, depth: usize, out: &mut String) {
    let (label, est) = describe(phys, vars);
    out.push_str(&"  ".repeat(depth));
    out.push_str(&label);
    if let Some(e) = est {
        out.push_str(&format!(" (est {e})"));
    }
    out.push('\n');
    for c in children(phys) {
        render(c, vars, depth + 1, out);
    }
}

/// Render the physical plan (no execution).
pub fn explain(snap: &Snapshot, q: &TranslatedQuery) -> Result<String, EngineError> {
    let dataset = crate::eval::dataset_view(snap, &q.dataset)?;
    let ev = Evaluator::new(snap, &q.vars, q.base.clone(), dataset);
    let phys = match crate::exec::plan::plan(&ev, &q.root, PScope::Fixed(Scope::Default)) {
        Ok(p) => p,
        Err(PlanError::Unsupported(what)) => {
            return Err(EngineError(format!("cannot plan: unsupported {what}")))
        }
        Err(PlanError::Engine(e)) => return Err(e),
    };
    let mut out = String::new();
    render(&phys, &q.vars, 0, &mut out);
    Ok(out)
}

/// Execute with per-operator metering; returns the output and the
/// annotated plan (est vs actual rows, per-operator wall time).
pub fn explain_analyze(
    snap: &Snapshot,
    q: &TranslatedQuery,
    opts: &ExecOptions,
) -> Result<(Output, String), EngineError> {
    crate::exec::run_analyzed(snap, q, opts)
}

/// Render the meter table after an analyzed run.
pub(crate) fn render_meters(meters: &Meters) -> String {
    let mut out = String::new();
    for m in meters.lock().unwrap().iter() {
        out.push_str(&"  ".repeat(m.depth));
        out.push_str(&m.label);
        if let Some(e) = m.est {
            out.push_str(&format!(" (est {e})"));
        }
        let rows = m.rows.load(Ordering::Relaxed);
        let ms = m.nanos.load(Ordering::Relaxed) as f64 / 1e6;
        out.push_str(&format!(" [rows {rows}, {ms:.2} ms]"));
        out.push('\n');
    }
    out
}
