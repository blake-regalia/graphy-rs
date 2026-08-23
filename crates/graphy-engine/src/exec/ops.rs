//! Physical operators (doc 05 §3): pull-based, batch-at-a-time. Each
//! operator's `next` returns the next batch of solutions or `None` at
//! exhaustion. Semantics-heavy pieces (expression evaluation, paths,
//! grouping, sorting) call into the shared reference machinery so the
//! two engines cannot drift.

use std::collections::{HashMap, HashSet, VecDeque};

use graphy_algebra::{AggregateExpr, Expression, PathExpr, TriplePat, VarId, P};
use graphy_store::{QuadBatch, QuadScan, Snapshot, TermPos};

use std::sync::Arc;

use crate::eval::{consistent_in, match_pattern_col, repeated_vars, Evaluator, Row, Scope, B};
use crate::exec::batch::Batch;
use crate::exec::expr::Prog;
use crate::exec::plan::Phys;
use crate::exec::scheduler::ParallelBindOp;
use crate::exec::Ctl;
use crate::EngineError;

/// Execution context threaded through every pull.
pub(crate) struct Cx<'a, 'b> {
    pub ev: &'b mut Evaluator<'a>,
    pub ctl: Arc<Ctl>,
}

/// Materialized-row accounting estimate (columnar cells + overhead).
pub(crate) fn row_cost(nvars: usize) -> usize {
    16 * nvars + 48
}

pub(crate) trait Op<'a> {
    fn next(&mut self, cx: &mut Cx<'a, '_>) -> Result<Option<Batch>, EngineError>;
}

pub(crate) type BoxOp<'a> = Box<dyn Op<'a> + 'a>;

/// Build the operator tree for a plan under a concrete runtime scope.
/// With ANALYZE metering active, every node is wrapped in a `MeterOp`
/// registered pre-order (so the meter table reads like the plan tree).
pub(crate) fn build<'a>(
    phys: &Phys,
    scope: Scope,
    nvars: usize,
    ev: &Evaluator<'a>,
    ctl: &Arc<Ctl>,
    depth: usize,
) -> BoxOp<'a> {
    let meter = ctl.meters().map(|meters| {
        let (label, est) = crate::exec::explain::describe(phys, ev.vars);
        let m = Arc::new(crate::exec::explain::Meter {
            label,
            est,
            depth,
            rows: std::sync::atomic::AtomicU64::new(0),
            nanos: std::sync::atomic::AtomicU64::new(0),
        });
        meters.lock().unwrap().push(m.clone());
        m
    });
    let op = build_node(phys, scope, nvars, ev, ctl, depth);
    match meter {
        Some(meter) => Box::new(crate::exec::explain::MeterOp { inner: op, meter }),
        None => op,
    }
}

fn build_node<'a>(
    phys: &Phys,
    scope: Scope,
    nvars: usize,
    ev: &Evaluator<'a>,
    ctl: &Arc<Ctl>,
    depth: usize,
) -> BoxOp<'a> {
    // Morsel-parallel fast path (doc 05 §2): a Scan + BindJoin chain
    // whose driving scan is big enough runs on the worker pool. The
    // count gate keeps point lookups off the pool entirely.
    if let Phys::BindJoin { .. } = phys {
        let mut pats_rev: Vec<TriplePat> = Vec::new();
        let mut cur = phys;
        while let Phys::BindJoin { input, pat, .. } = cur {
            pats_rev.push(pat.clone());
            cur = input;
        }
        if let Phys::Scan { pat, est } = cur {
            if ctl.threads() > 1 && *est >= ctl.parallel_threshold {
                pats_rev.reverse();
                return Box::new(ParallelBindOp {
                    driving: Box::new(ScanOp::new(pat.clone(), scope, nvars, ev)),
                    pats: pats_rev,
                    scope,
                    nvars,
                    threads: ctl.threads(),
                    out: None,
                });
            }
        }
    }
    match phys {
        Phys::Unit => Box::new(UnitOp { nvars, done: false }),
        Phys::Empty => Box::new(EmptyOp),
        Phys::Scan { pat, .. } => Box::new(ScanOp::new(pat.clone(), scope, nvars, ev)),
        Phys::BindJoin { input, pat, .. } => Box::new(BindJoinOp {
            input: build(input, scope, nvars, ev, ctl, depth + 1),
            pat: pat.clone(),
            scope,
            nvars,
            pending: VecDeque::new(),
            done: false,
        }),
        Phys::BindPath { input, s, path, o } => Box::new(BindPathOp {
            input: build(input, scope, nvars, ev, ctl, depth + 1),
            s: s.clone(),
            path: path.clone(),
            o: o.clone(),
            scope,
            nvars,
            pending: VecDeque::new(),
            done: false,
        }),
        Phys::HashJoin { left, right, keys } => Box::new(HashJoinOp {
            left: build(left, scope, nvars, ev, ctl, depth + 1),
            right: build(right, scope, nvars, ev, ctl, depth + 1),
            keys: keys.clone(),
            nvars,
            table: HashMap::new(),
            loose: Vec::new(),
            built: false,
            pending: VecDeque::new(),
            done: false,
        }),
        Phys::LeftJoin {
            left,
            right,
            expr,
            keys,
        } => Box::new(LeftJoinOp {
            left: build(left, scope, nvars, ev, ctl, depth + 1),
            right: build(right, scope, nvars, ev, ctl, depth + 1),
            expr: expr.clone(),
            keys: keys.clone(),
            scope,
            nvars,
            table: HashMap::new(),
            loose: Vec::new(),
            built: false,
            pending: VecDeque::new(),
            done: false,
        }),
        Phys::Minus { left, right } => Box::new(MinusOp {
            left: build(left, scope, nvars, ev, ctl, depth + 1),
            right: build(right, scope, nvars, ev, ctl, depth + 1),
            nvars,
            rows: Vec::new(),
            built: false,
        }),
        Phys::Filter { input, prog } => Box::new(FilterOp {
            input: build(input, scope, nvars, ev, ctl, depth + 1),
            prog: prog.clone(),
            scope,
            nvars,
        }),
        Phys::Extend { input, var, prog } => Box::new(ExtendOp {
            input: build(input, scope, nvars, ev, ctl, depth + 1),
            var: *var,
            prog: prog.clone(),
            scope,
        }),
        Phys::Union(l, r) => Box::new(UnionOp {
            left: Some(build(l, scope, nvars, ev, ctl, depth + 1)),
            right: Some(build(r, scope, nvars, ev, ctl, depth + 1)),
        }),
        // The named column was resolved at plan time; the child was
        // planned under the fixed scope.
        Phys::GraphConst { col, input } => {
            build(input, Scope::Named(*col), nvars, ev, ctl, depth + 1)
        }
        Phys::GraphVar { var, input } => Box::new(GraphVarOp {
            var: *var,
            plan: (**input).clone(),
            nvars,
            cols: None,
            idx: 0,
            cur: None,
            cur_gid: None,
        }),
        Phys::Table { vars, rows } => Box::new(TableOp {
            vars: vars.clone(),
            rows: rows.clone(),
            nvars,
            done: false,
        }),
        Phys::Group {
            keys,
            aggregates,
            input,
        } => Box::new(GroupOp {
            input: build(input, scope, nvars, ev, ctl, depth + 1),
            keys: keys.clone(),
            aggregates: aggregates.clone(),
            scope,
            nvars,
            out: None,
        }),
        Phys::Sort { input, conditions } => Box::new(SortOp {
            input: build(input, scope, nvars, ev, ctl, depth + 1),
            conditions: conditions.clone(),
            scope,
            nvars,
            out: None,
        }),
        Phys::Project { input, vars } => Box::new(ProjectOp {
            input: build(input, scope, nvars, ev, ctl, depth + 1),
            keep: vars.iter().map(|v| v.0 as usize).collect(),
        }),
        Phys::Distinct { input } => Box::new(DistinctOp {
            input: build(input, scope, nvars, ev, ctl, depth + 1),
            nvars,
            seen: HashSet::new(),
        }),
        Phys::Slice {
            input,
            offset,
            limit,
        } => Box::new(SliceOp {
            input: build(input, scope, nvars, ev, ctl, depth + 1),
            nvars,
            skip: *offset as usize,
            remain: limit.map(|l| l as usize),
        }),
    }
}

/// Drain an operator into materialized rows.
pub(crate) fn drain<'a>(op: &mut BoxOp<'a>, cx: &mut Cx<'a, '_>) -> Result<Vec<Row>, EngineError> {
    let mut rows = Vec::new();
    while let Some(b) = op.next(cx)? {
        cx.ctl.charge(b.len * b.cols.len().max(1) * 16)?;
        for i in 0..b.len {
            rows.push(b.row_at(i));
        }
    }
    Ok(rows)
}

// ---------------------------------------------------------------- leaves

struct UnitOp {
    nvars: usize,
    done: bool,
}

impl<'a> Op<'a> for UnitOp {
    fn next(&mut self, _cx: &mut Cx<'a, '_>) -> Result<Option<Batch>, EngineError> {
        if self.done {
            return Ok(None);
        }
        self.done = true;
        let mut b = Batch::new(self.nvars);
        b.push_row(&vec![None; self.nvars]);
        Ok(Some(b))
    }
}

struct EmptyOp;

impl<'a> Op<'a> for EmptyOp {
    fn next(&mut self, _cx: &mut Cx<'a, '_>) -> Result<Option<Batch>, EngineError> {
        Ok(None)
    }
}

/// Streaming driving scan: one triple pattern, unbound seed. A USING /
/// FROM default-graph union scans each member graph with cross-member
/// triple dedup (mirroring `Evaluator::scan_pattern`).
struct ScanOp<'a> {
    pat: TriplePat,
    members: Vec<Scope>,
    dedup: bool,
    repeated: bool,
    structural: bool,
    nvars: usize,
    m: usize,
    scan: Option<QuadScan<'a>>,
    qb: QuadBatch,
    qpos: usize,
    seen: HashSet<(u64, u64, u64)>,
    done: bool,
}

impl<'a> ScanOp<'a> {
    fn new(pat: TriplePat, scope: Scope, nvars: usize, ev: &Evaluator<'a>) -> ScanOp<'a> {
        let (members, dedup) = match (&scope, &ev.dataset.default_union) {
            (Scope::Default, Some(cols)) => (cols.iter().map(|c| Scope::Named(*c)).collect(), true),
            _ => (vec![scope], false),
        };
        ScanOp {
            repeated: repeated_vars(&pat),
            structural: matches!(&pat.s, P::Triple(_))
                || matches!(&pat.p, P::Triple(_))
                || matches!(&pat.o, P::Triple(_)),
            pat,
            members,
            dedup,
            nvars,
            m: 0,
            scan: None,
            qb: QuadBatch::new(),
            qpos: 0,
            seen: HashSet::new(),
            done: false,
        }
    }

    fn advance_scan(
        &mut self,
        snap: &'a Snapshot,
        ev: &Evaluator<'a>,
    ) -> Result<bool, EngineError> {
        while self.m < self.members.len() {
            let scope = self.members[self.m];
            self.m += 1;
            if let Some(pat) = ev.pattern_of(&self.pat, &scope, None)? {
                self.scan = Some(snap.scan_best(&pat)?);
                return Ok(true);
            }
        }
        Ok(false)
    }
}

impl<'a> Op<'a> for ScanOp<'a> {
    fn next(&mut self, cx: &mut Cx<'a, '_>) -> Result<Option<Batch>, EngineError> {
        cx.ctl.check()?;
        if self.done {
            return Ok(None);
        }
        let snap: &'a Snapshot = cx.ev.snap;
        let mut out = Batch::new(self.nvars);
        let mut row: Row = vec![None; self.nvars];
        'fill: while !out.is_full() {
            // Prime the quad buffer.
            while self.qpos >= self.qb.len() {
                self.qpos = 0;
                let more = match self.scan.as_mut() {
                    Some(scan) => scan.next_batch(&mut self.qb)?,
                    None => false,
                };
                if !more {
                    self.scan = None;
                    if !self.advance_scan(snap, cx.ev)? {
                        self.done = true;
                        break 'fill;
                    }
                }
            }
            let i = self.qpos;
            self.qpos += 1;
            let (s, p, o) = (self.qb.s[i], self.qb.p[i], self.qb.o[i]);
            if self.dedup && !self.seen.insert((s, p, o)) {
                continue;
            }
            for cell in row.iter_mut() {
                *cell = None;
            }
            let ok = if self.structural {
                match_pattern_col(snap, &self.pat.s, s, TermPos::Subject, &mut row)?
                    && match_pattern_col(snap, &self.pat.p, p, TermPos::Predicate, &mut row)?
                    && match_pattern_col(snap, &self.pat.o, o, TermPos::Object, &mut row)?
            } else {
                let bind = |pv: &P, col: u64, pos: TermPos, row: &mut Row| {
                    if let P::Var(v) = pv {
                        if row[v.0 as usize].is_none() {
                            row[v.0 as usize] = Some(B::Id(snap.term_id(col, pos)));
                        }
                    }
                };
                bind(&self.pat.s, s, TermPos::Subject, &mut row);
                bind(&self.pat.p, p, TermPos::Predicate, &mut row);
                bind(&self.pat.o, o, TermPos::Object, &mut row);
                !self.repeated || consistent_in(snap, &self.pat, &row, s, p, o)
            };
            if !ok {
                continue;
            }
            out.push_row(&row);
        }
        Ok((!out.is_empty()).then_some(out))
    }
}

struct TableOp {
    vars: Vec<VarId>,
    rows: Vec<Vec<Option<Vec<u8>>>>,
    nvars: usize,
    done: bool,
}

impl<'a> Op<'a> for TableOp {
    fn next(&mut self, cx: &mut Cx<'a, '_>) -> Result<Option<Batch>, EngineError> {
        if self.done {
            return Ok(None);
        }
        self.done = true;
        let mut out = Batch::new(self.nvars);
        for row in &self.rows {
            let mut r: Row = vec![None; self.nvars];
            let mut ok = true;
            for (v, cell) in self.vars.iter().zip(row) {
                if let Some(bytes) = cell {
                    let b = cx.ev.intern(bytes.clone());
                    match r[v.0 as usize] {
                        Some(existing) if existing != b => {
                            ok = false;
                            break;
                        }
                        _ => r[v.0 as usize] = Some(b),
                    }
                }
            }
            if ok {
                out.push_row(&r);
            }
        }
        Ok((!out.is_empty()).then_some(out))
    }
}

// ---------------------------------------------------------------- joins

/// IndexNestedLoop (bind) join: probe the pattern per input row through
/// the shared scan machinery (bound variables substituted into the
/// storage pattern).
struct BindJoinOp<'a> {
    input: BoxOp<'a>,
    pat: TriplePat,
    scope: Scope,
    nvars: usize,
    pending: VecDeque<Row>,
    done: bool,
}

impl<'a> Op<'a> for BindJoinOp<'a> {
    fn next(&mut self, cx: &mut Cx<'a, '_>) -> Result<Option<Batch>, EngineError> {
        cx.ctl.check()?;
        let mut out = Batch::new(self.nvars);
        loop {
            while let Some(r) = self.pending.pop_front() {
                out.push_row(&r);
                if out.is_full() {
                    return Ok(Some(out));
                }
            }
            if self.done {
                break;
            }
            match self.input.next(cx)? {
                None => self.done = true,
                Some(b) => {
                    let mut row = Vec::new();
                    let mut matches = Vec::new();
                    for i in 0..b.len {
                        b.row_into(i, &mut row);
                        cx.ev
                            .scan_pattern(&self.pat, &self.scope, &row, &mut matches)?;
                        self.pending.extend(matches.drain(..));
                    }
                }
            }
        }
        Ok((!out.is_empty()).then_some(out))
    }
}

/// Property-path step per input row (shared closure machinery).
struct BindPathOp<'a> {
    input: BoxOp<'a>,
    s: P,
    path: PathExpr,
    o: P,
    scope: Scope,
    nvars: usize,
    pending: VecDeque<Row>,
    done: bool,
}

impl<'a> Op<'a> for BindPathOp<'a> {
    fn next(&mut self, cx: &mut Cx<'a, '_>) -> Result<Option<Batch>, EngineError> {
        cx.ctl.check()?;
        let mut out = Batch::new(self.nvars);
        loop {
            while let Some(r) = self.pending.pop_front() {
                out.push_row(&r);
                if out.is_full() {
                    return Ok(Some(out));
                }
            }
            if self.done {
                break;
            }
            match self.input.next(cx)? {
                None => self.done = true,
                Some(b) => {
                    let mut matches = Vec::new();
                    for i in 0..b.len {
                        cx.ctl.charge(row_cost(self.nvars))?;
                        cx.ev.eval_path(
                            &self.s,
                            &self.path,
                            &self.o,
                            &self.scope,
                            b.row_at(i),
                            &mut matches,
                        )?;
                        self.pending.extend(matches.drain(..));
                    }
                }
            }
        }
        Ok((!out.is_empty()).then_some(out))
    }
}

/// Key of certainly-bound join vars (`None` when any key is unbound —
/// the row joins by compatible-merge scanning instead).
fn key_of(row: &Row, keys: &[VarId]) -> Option<Vec<B>> {
    keys.iter()
        .map(|v| row[v.0 as usize])
        .collect::<Option<Vec<B>>>()
}

struct HashJoinOp<'a> {
    left: BoxOp<'a>,
    right: BoxOp<'a>,
    keys: Vec<VarId>,
    nvars: usize,
    table: HashMap<Vec<B>, Vec<Row>>,
    loose: Vec<Row>,
    built: bool,
    pending: VecDeque<Row>,
    done: bool,
}

impl<'a> HashJoinOp<'a> {
    fn build_side(&mut self, cx: &mut Cx<'a, '_>) -> Result<(), EngineError> {
        while let Some(b) = self.right.next(cx)? {
            cx.ctl.charge(b.len * row_cost(self.nvars))?;
            for i in 0..b.len {
                let row = b.row_at(i);
                match (!self.keys.is_empty())
                    .then(|| key_of(&row, &self.keys))
                    .flatten()
                {
                    Some(k) => self.table.entry(k).or_default().push(row),
                    None => self.loose.push(row),
                }
            }
        }
        self.built = true;
        Ok(())
    }
}

impl<'a> Op<'a> for HashJoinOp<'a> {
    fn next(&mut self, cx: &mut Cx<'a, '_>) -> Result<Option<Batch>, EngineError> {
        cx.ctl.check()?;
        if !self.built {
            self.build_side(cx)?;
        }
        let mut out = Batch::new(self.nvars);
        loop {
            while let Some(r) = self.pending.pop_front() {
                out.push_row(&r);
                if out.is_full() {
                    return Ok(Some(out));
                }
            }
            if self.done {
                break;
            }
            match self.left.next(cx)? {
                None => self.done = true,
                Some(b) => {
                    for i in 0..b.len {
                        let row = b.row_at(i);
                        match (!self.keys.is_empty())
                            .then(|| key_of(&row, &self.keys))
                            .flatten()
                        {
                            Some(k) => {
                                if let Some(bucket) = self.table.get(&k) {
                                    for r in bucket {
                                        if let Some(m) = crate::eval::merge(&row, r) {
                                            self.pending.push_back(m);
                                        }
                                    }
                                }
                            }
                            None => {
                                for bucket in self.table.values() {
                                    for r in bucket {
                                        if let Some(m) = crate::eval::merge(&row, r) {
                                            self.pending.push_back(m);
                                        }
                                    }
                                }
                            }
                        }
                        for r in &self.loose {
                            if let Some(m) = crate::eval::merge(&row, r) {
                                self.pending.push_back(m);
                            }
                        }
                    }
                }
            }
        }
        Ok((!out.is_empty()).then_some(out))
    }
}

struct LeftJoinOp<'a> {
    left: BoxOp<'a>,
    right: BoxOp<'a>,
    expr: Option<Expression>,
    keys: Vec<VarId>,
    scope: Scope,
    nvars: usize,
    table: HashMap<Vec<B>, Vec<Row>>,
    loose: Vec<Row>,
    built: bool,
    pending: VecDeque<Row>,
    done: bool,
}

/// One left row against the built right side: emit every compatible,
/// filter-passing merge; emit the bare left row when none extends it.
fn lj_probe<'a>(
    cx: &mut Cx<'a, '_>,
    expr: &Option<Expression>,
    table: &HashMap<Vec<B>, Vec<Row>>,
    loose: &[Row],
    keys: &[VarId],
    row: Row,
    pending: &mut VecDeque<Row>,
) {
    let mut extended = false;
    let try_pair = |cx: &mut Cx<'a, '_>, r: &Row, pending: &mut VecDeque<Row>| {
        if let Some(m) = crate::eval::merge(&row, r) {
            let pass = match expr {
                Some(e) => cx.ev.truthy(e, &m),
                None => true,
            };
            if pass {
                pending.push_back(m);
                return true;
            }
        }
        false
    };
    match (!keys.is_empty()).then(|| key_of(&row, keys)).flatten() {
        Some(k) => {
            if let Some(bucket) = table.get(&k) {
                for r in bucket {
                    extended |= try_pair(cx, r, pending);
                }
            }
        }
        None => {
            for bucket in table.values() {
                for r in bucket {
                    extended |= try_pair(cx, r, pending);
                }
            }
        }
    }
    for r in loose {
        extended |= try_pair(cx, r, pending);
    }
    if !extended {
        pending.push_back(row);
    }
}

impl<'a> Op<'a> for LeftJoinOp<'a> {
    fn next(&mut self, cx: &mut Cx<'a, '_>) -> Result<Option<Batch>, EngineError> {
        cx.ctl.check()?;
        if !self.built {
            while let Some(b) = self.right.next(cx)? {
                for i in 0..b.len {
                    let row = b.row_at(i);
                    match (!self.keys.is_empty())
                        .then(|| key_of(&row, &self.keys))
                        .flatten()
                    {
                        Some(k) => self.table.entry(k).or_default().push(row),
                        None => self.loose.push(row),
                    }
                }
            }
            self.built = true;
        }
        let mut out = Batch::new(self.nvars);
        loop {
            while let Some(r) = self.pending.pop_front() {
                out.push_row(&r);
                if out.is_full() {
                    return Ok(Some(out));
                }
            }
            if self.done {
                break;
            }
            match self.left.next(cx)? {
                None => self.done = true,
                Some(b) => {
                    cx.ev.expr_scope = self.scope;
                    for i in 0..b.len {
                        lj_probe(
                            cx,
                            &self.expr,
                            &self.table,
                            &self.loose,
                            &self.keys,
                            b.row_at(i),
                            &mut self.pending,
                        );
                    }
                }
            }
        }
        Ok((!out.is_empty()).then_some(out))
    }
}

struct MinusOp<'a> {
    left: BoxOp<'a>,
    right: BoxOp<'a>,
    nvars: usize,
    rows: Vec<Row>,
    built: bool,
}

impl<'a> Op<'a> for MinusOp<'a> {
    fn next(&mut self, cx: &mut Cx<'a, '_>) -> Result<Option<Batch>, EngineError> {
        cx.ctl.check()?;
        if !self.built {
            while let Some(b) = self.right.next(cx)? {
                cx.ctl.charge(b.len * row_cost(self.nvars))?;
                for i in 0..b.len {
                    self.rows.push(b.row_at(i));
                }
            }
            self.built = true;
        }
        loop {
            let Some(b) = self.left.next(cx)? else {
                return Ok(None);
            };
            let mut out = Batch::new(self.nvars);
            for i in 0..b.len {
                let row = b.row_at(i);
                let excluded = self.rows.iter().any(|r| {
                    // §18.5: compatible AND sharing a bound var.
                    crate::eval::merge(&row, r).is_some()
                        && row.iter().zip(r).any(|(x, y)| x.is_some() && y.is_some())
                });
                if !excluded {
                    out.push_row(&row);
                }
            }
            if !out.is_empty() {
                return Ok(Some(out));
            }
        }
    }
}

// ------------------------------------------------------------ transforms

struct FilterOp<'a> {
    input: BoxOp<'a>,
    prog: Prog,
    scope: Scope,
    nvars: usize,
}

impl<'a> Op<'a> for FilterOp<'a> {
    fn next(&mut self, cx: &mut Cx<'a, '_>) -> Result<Option<Batch>, EngineError> {
        cx.ctl.check()?;
        loop {
            let Some(b) = self.input.next(cx)? else {
                return Ok(None);
            };
            cx.ev.expr_scope = self.scope;
            let mut out = Batch::new(self.nvars);
            let mut row = Vec::new();
            for i in 0..b.len {
                b.row_into(i, &mut row);
                if self.prog.truthy(cx.ev, &row) {
                    out.push_row(&row);
                }
            }
            if !out.is_empty() {
                return Ok(Some(out));
            }
        }
    }
}

struct ExtendOp<'a> {
    input: BoxOp<'a>,
    var: VarId,
    prog: Prog,
    scope: Scope,
}

impl<'a> Op<'a> for ExtendOp<'a> {
    fn next(&mut self, cx: &mut Cx<'a, '_>) -> Result<Option<Batch>, EngineError> {
        let Some(mut b) = self.input.next(cx)? else {
            return Ok(None);
        };
        cx.ev.expr_scope = self.scope;
        let mut row = Vec::new();
        for i in 0..b.len {
            b.row_into(i, &mut row);
            if let Ok(v) = self.prog.eval(cx.ev, &row) {
                b.cols[self.var.0 as usize][i] = Some(v);
            }
        }
        Ok(Some(b))
    }
}

struct UnionOp<'a> {
    left: Option<BoxOp<'a>>,
    right: Option<BoxOp<'a>>,
}

impl<'a> Op<'a> for UnionOp<'a> {
    fn next(&mut self, cx: &mut Cx<'a, '_>) -> Result<Option<Batch>, EngineError> {
        if let Some(l) = self.left.as_mut() {
            if let Some(b) = l.next(cx)? {
                return Ok(Some(b));
            }
            self.left = None;
        }
        if let Some(r) = self.right.as_mut() {
            if let Some(b) = r.next(cx)? {
                return Ok(Some(b));
            }
            self.right = None;
        }
        Ok(None)
    }
}

/// GRAPH ?g: enumerate visible named graphs, run the subtree under each,
/// and join the graph binding in afterwards (§18.3).
struct GraphVarOp<'a> {
    var: VarId,
    plan: Phys,
    nvars: usize,
    cols: Option<Vec<u64>>,
    idx: usize,
    cur: Option<BoxOp<'a>>,
    cur_gid: Option<B>,
}

impl<'a> Op<'a> for GraphVarOp<'a> {
    fn next(&mut self, cx: &mut Cx<'a, '_>) -> Result<Option<Batch>, EngineError> {
        loop {
            if self.cur.is_none() {
                let cols = match &self.cols {
                    Some(c) => c,
                    None => {
                        self.cols = Some(cx.ev.named_graph_cols());
                        self.cols.as_ref().unwrap()
                    }
                };
                if self.idx >= cols.len() {
                    return Ok(None);
                }
                let col = cols[self.idx];
                self.idx += 1;
                self.cur_gid = Some(cx.ev.canonical_graph_b(col)?);
                let ctl = cx.ctl.rebuild();
                self.cur = Some(build(
                    &self.plan,
                    Scope::Named(col),
                    self.nvars,
                    cx.ev,
                    &ctl,
                    0,
                ));
            }
            let gid = self.cur_gid.unwrap();
            match self.cur.as_mut().unwrap().next(cx)? {
                None => {
                    self.cur = None;
                    continue;
                }
                Some(b) => {
                    let mut out = Batch::new(self.nvars);
                    let mut row = Vec::new();
                    for i in 0..b.len {
                        b.row_into(i, &mut row);
                        match row[self.var.0 as usize] {
                            // Inner bindings may carry another section's
                            // id for the same term.
                            Some(existing) if !cx.ev.same_term(existing, gid)? => {}
                            _ => {
                                row[self.var.0 as usize] = Some(gid);
                                out.push_row(&row);
                            }
                        }
                    }
                    if !out.is_empty() {
                        return Ok(Some(out));
                    }
                }
            }
        }
    }
}

// --------------------------------------------------------------- sinks

struct GroupOp<'a> {
    input: BoxOp<'a>,
    keys: Vec<(VarId, Option<Expression>)>,
    aggregates: Vec<(VarId, AggregateExpr)>,
    scope: Scope,
    nvars: usize,
    out: Option<VecDeque<Row>>,
}

impl<'a> Op<'a> for GroupOp<'a> {
    fn next(&mut self, cx: &mut Cx<'a, '_>) -> Result<Option<Batch>, EngineError> {
        if self.out.is_none() {
            let rows = drain(&mut self.input, cx)?;
            cx.ev.expr_scope = self.scope;
            let seed = vec![None; self.nvars];
            let grouped = cx
                .ev
                .group_rows(&self.keys, &self.aggregates, rows, &seed)?;
            self.out = Some(grouped.into());
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

struct SortOp<'a> {
    input: BoxOp<'a>,
    conditions: Vec<(Expression, bool)>,
    scope: Scope,
    nvars: usize,
    out: Option<VecDeque<Row>>,
}

impl<'a> Op<'a> for SortOp<'a> {
    fn next(&mut self, cx: &mut Cx<'a, '_>) -> Result<Option<Batch>, EngineError> {
        if self.out.is_none() {
            let rows = drain(&mut self.input, cx)?;
            cx.ev.expr_scope = self.scope;
            let sorted = cx.ev.sort_rows(&self.conditions, rows);
            self.out = Some(sorted.into());
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

struct ProjectOp<'a> {
    input: BoxOp<'a>,
    keep: HashSet<usize>,
}

impl<'a> Op<'a> for ProjectOp<'a> {
    fn next(&mut self, cx: &mut Cx<'a, '_>) -> Result<Option<Batch>, EngineError> {
        let Some(mut b) = self.input.next(cx)? else {
            return Ok(None);
        };
        for (i, col) in b.cols.iter_mut().enumerate() {
            if !self.keep.contains(&i) {
                col.iter_mut().for_each(|c| *c = None);
            }
        }
        Ok(Some(b))
    }
}

struct DistinctOp<'a> {
    input: BoxOp<'a>,
    nvars: usize,
    seen: HashSet<Row>,
}

impl<'a> Op<'a> for DistinctOp<'a> {
    fn next(&mut self, cx: &mut Cx<'a, '_>) -> Result<Option<Batch>, EngineError> {
        cx.ctl.check()?;
        loop {
            let Some(b) = self.input.next(cx)? else {
                return Ok(None);
            };
            let mut out = Batch::new(self.nvars);
            for i in 0..b.len {
                let row = b.row_at(i);
                if self.seen.insert(row.clone()) {
                    out.push_row(&row);
                }
            }
            if !out.is_empty() {
                return Ok(Some(out));
            }
        }
    }
}

struct SliceOp<'a> {
    input: BoxOp<'a>,
    nvars: usize,
    skip: usize,
    remain: Option<usize>,
}

impl<'a> Op<'a> for SliceOp<'a> {
    fn next(&mut self, cx: &mut Cx<'a, '_>) -> Result<Option<Batch>, EngineError> {
        loop {
            if self.remain == Some(0) {
                return Ok(None);
            }
            let Some(b) = self.input.next(cx)? else {
                return Ok(None);
            };
            let mut out = Batch::new(self.nvars);
            for i in 0..b.len {
                if self.skip > 0 {
                    self.skip -= 1;
                    continue;
                }
                match self.remain.as_mut() {
                    Some(0) => break,
                    Some(n) => *n -= 1,
                    None => {}
                }
                out.push_row(&b.row_at(i));
            }
            if !out.is_empty() {
                return Ok(Some(out));
            }
            if self.remain == Some(0) {
                return Ok(None);
            }
        }
    }
}
