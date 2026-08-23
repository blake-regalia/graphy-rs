//! Reference evaluator (doc 05 §9's semantic oracle): row-at-a-time,
//! correctness-first evaluation of the algebra against a `Snapshot`.
//! BGPs run left-deep bind joins ordered by exact pattern counts;
//! everything else follows the spec's multiset definitions directly.
//! The vectorized morsel engine (later increments) must match this
//! evaluator on every query — it is deliberately simple.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};

use graphy_algebra::algebra::{Aggregate, Builtin, CmpOp};
use graphy_algebra::{
    AggregateExpr, Algebra, Expression, Form, PathExpr, TranslatedQuery, TriplePat, VarId,
    VarTable, P,
};
use graphy_core::TermId;
use graphy_store::{Pattern, QuadBatch, Snapshot, StoreError, TermPos};

use crate::value::{
    arith, cmp_values, decode_value, ebv, encode_value, eq_values, order_cmp, str_of, ArithOp, Dec,
    Num, Value,
};

static BNODE_SCOPE: AtomicU64 = AtomicU64::new(0);

/// Engine-level failure (I/O, corruption, unsupported feature).
#[derive(Debug)]
pub struct EngineError(pub String);

impl std::fmt::Display for EngineError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

impl std::error::Error for EngineError {}

impl From<StoreError> for EngineError {
    fn from(e: StoreError) -> EngineError {
        EngineError(e.to_string())
    }
}

/// A bound term: a store [`TermId`] or a query-local computed term
/// (interned in the evaluator's side arena).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum B {
    Id(TermId),
    Ext(u32),
}

/// One solution row, indexed by [`VarId`].
pub type Row = Vec<Option<B>>;

/// Query results.
#[derive(Debug, Clone, PartialEq)]
pub enum Output {
    /// Projected variable names + rows of concise term bytes.
    Solutions {
        vars: Vec<String>,
        rows: Vec<Vec<Option<Vec<u8>>>>,
    },
    Boolean(bool),
    /// CONSTRUCT triples as concise (s, p, o).
    Triples(Vec<(Vec<u8>, Vec<u8>, Vec<u8>)>),
}

/// Evaluate a translated (and ideally rewritten) query through the
/// reference evaluator (the semantic oracle; `crate::evaluate` routes
/// through the vectorized engine and must agree with this on every
/// query).
pub fn evaluate_ref(snap: &Snapshot, q: &TranslatedQuery) -> Result<Output, EngineError> {
    let dataset = dataset_view(snap, &q.dataset)?;
    let mut ev = Evaluator::new(snap, &q.vars, q.base.clone(), dataset);
    let seed = vec![None; q.vars.len()];
    let rows = ev.eval(&q.root, &Scope::Default, &seed)?;
    finish(&mut ev, q, rows)
}

/// Build the [`DatasetView`] for a query's FROM / FROM NAMED clauses
/// (§13.2: the dataset is *constructed* from the named clauses — an
/// empty clause list keeps the store's own dataset). Graphs absent from
/// the store contribute nothing.
pub(crate) fn dataset_view(
    snap: &Snapshot,
    dataset: &[(bool, Vec<u8>)],
) -> Result<DatasetView, EngineError> {
    if dataset.is_empty() {
        return Ok(DatasetView::default());
    }
    let mut default_union = Vec::new();
    let mut named = Vec::new();
    for (is_default, iri) in dataset {
        let col = pattern_col_g(snap, iri);
        if *is_default {
            if let Some(col) = col {
                default_union.push(col);
            }
        } else if let Some(col) = col {
            named.push(col);
        }
    }
    Ok(DatasetView {
        default_union: Some(default_union),
        named: Some(named),
    })
}

fn pattern_col_g(snap: &Snapshot, bytes: &[u8]) -> Option<u64> {
    pattern_col(snap, bytes, TermPos::Graph).filter(|&c| c > 0)
}

/// Form the query output from the root solution rows (shared by the
/// reference and vectorized engines).
pub(crate) fn finish(
    ev: &mut Evaluator<'_>,
    q: &TranslatedQuery,
    rows: Vec<Row>,
) -> Result<Output, EngineError> {
    match &q.form {
        Form::Select | Form::Ask if matches!(q.form, Form::Ask) => {
            Ok(Output::Boolean(!rows.is_empty()))
        }
        Form::Select | Form::Ask => {
            let projected = projection_of(&q.root);
            let names = projected
                .iter()
                .map(|v| q.vars.name(*v).to_owned())
                .collect();
            let out = rows
                .iter()
                .map(|r| {
                    projected
                        .iter()
                        .map(|v| {
                            r.get(v.0 as usize)
                                .copied()
                                .flatten()
                                .map(|b| ev.bytes_of(b))
                                .transpose()
                        })
                        .collect::<Result<Vec<_>, _>>()
                })
                .collect::<Result<Vec<_>, _>>()?;
            Ok(Output::Solutions {
                vars: names,
                rows: out,
            })
        }
        Form::Construct(template) => {
            let mut seen = HashSet::new();
            let mut out = Vec::new();
            for row in &rows {
                ev.fresh_bnode += 1;
                for t in template {
                    let Some(s) = ev.template_term(&t.s, row)? else {
                        continue;
                    };
                    let Some(p) = ev.template_term(&t.p, row)? else {
                        continue;
                    };
                    let Some(o) = ev.template_term(&t.o, row)? else {
                        continue;
                    };
                    // SPARQL CONSTRUCT drops any instantiated template
                    // triple that is not an RDF triple. In RDF 1.2 only an
                    // IRI or blank node may be a subject, and only an IRI
                    // may be a predicate; literals and triple terms are
                    // therefore invalid subjects regardless of their
                    // concise sigil.
                    let subject_ok = matches!(
                        graphy_core::concise::decode(&s),
                        Ok(graphy_core::TermRef::Iri(_) | graphy_core::TermRef::BlankNode(_))
                    );
                    let predicate_ok = matches!(
                        graphy_core::concise::decode(&p),
                        Ok(graphy_core::TermRef::Iri(_))
                    );
                    if !subject_ok || !predicate_ok {
                        continue;
                    }
                    if seen.insert((s.clone(), p.clone(), o.clone())) {
                        out.push((s, p, o));
                    }
                }
            }
            Ok(Output::Triples(out))
        }
        Form::Describe(targets) => {
            // Collect the described resources: constants plus every
            // binding of each target variable across the solutions.
            let mut nodes: Vec<B> = Vec::new();
            let mut seen_nodes = HashSet::new();
            let mut push = |b: B, nodes: &mut Vec<B>| {
                if seen_nodes.insert(b) {
                    nodes.push(b);
                }
            };
            for t in targets {
                match t {
                    P::Term(bytes) => {
                        let b = ev.intern(bytes.clone());
                        push(b, &mut nodes);
                    }
                    P::Var(v) => {
                        for row in &rows {
                            if let Some(b) = row[v.0 as usize] {
                                push(b, &mut nodes);
                            }
                        }
                    }
                    P::Triple(_) => {
                        return Err(EngineError("DESCRIBE of a triple term".into()));
                    }
                }
            }
            // Concise bounded description over the query's active default
            // graph (the FROM union when a dataset clause is present, else
            // the store's real default graph): outgoing triples of each
            // resource, recursing through blank objects (each blank
            // visited once).
            let default_cols: Vec<u64> = match &ev.dataset.default_union {
                Some(cols) => cols.clone(),
                None => vec![0],
            };
            let mut out = Vec::new();
            let mut emitted = HashSet::new();
            let mut visited = HashSet::new();
            let mut stack: Vec<B> = nodes;
            while let Some(b) = stack.pop() {
                if !visited.insert(b) {
                    continue;
                }
                let bytes = ev.bytes_of(b)?;
                let Some(id) = ev.snap.resolve(&bytes, TermPos::Subject) else {
                    continue;
                };
                let Some(col) = ev.snap.column(id, TermPos::Subject) else {
                    continue;
                };
                for &gcol in &default_cols {
                    let pat = Pattern {
                        s: Some(col),
                        g: Some(gcol),
                        ..Pattern::default()
                    };
                    let mut scan = ev.snap.scan_best(&pat)?;
                    let mut batch = QuadBatch::new();
                    while scan.next_batch(&mut batch)? {
                        for i in 0..batch.len() {
                            let s = ev.snap.decode_value(batch.s[i], TermPos::Subject)?;
                            let p = ev.snap.decode_value(batch.p[i], TermPos::Predicate)?;
                            let o = ev.snap.decode_value(batch.o[i], TermPos::Object)?;
                            if o.starts_with(b"_") {
                                let ob = ev.intern(o.clone());
                                if !visited.contains(&ob) {
                                    stack.push(ob);
                                }
                            }
                            if emitted.insert((s.clone(), p.clone(), o.clone())) {
                                out.push((s, p, o));
                            }
                        }
                    }
                }
            }
            Ok(Output::Triples(out))
        }
    }
}

/// Evaluate a bare pattern for the update executor (M7 inc.3): solution
/// rows still in binding space plus the evaluator that decodes them (the
/// caller memoizes decodes — owning every binding's bytes per row was the
/// dominant term of the 2026-08-07 large-update memory peak), under an
/// optional dataset view (USING) and root scope (`WITH` = a named graph
/// as the default graph; `u64::MAX` names no graph, i.e. an empty
/// default).
pub(crate) fn evaluate_rows<'s>(
    snap: &'s Snapshot,
    vars: &'s VarTable,
    root: &Algebra,
    dataset: DatasetView,
    root_scope_named: Option<u64>,
) -> Result<(Vec<Row>, Evaluator<'s>), EngineError> {
    let mut ev = Evaluator::new(snap, vars, None, dataset);
    let seed = vec![None; vars.len()];
    let scope = match root_scope_named {
        Some(col) => Scope::Named(col),
        None => Scope::Default,
    };
    let rows = ev.eval(root, &scope, &seed)?;
    Ok((rows, ev))
}

/// The outermost projection under the modifier stack.
pub(crate) fn projection_of(a: &Algebra) -> Vec<VarId> {
    match a {
        Algebra::Project { vars, .. } => vars.clone(),
        Algebra::Distinct(x) | Algebra::Reduced(x) => projection_of(x),
        Algebra::Slice { input, .. } | Algebra::OrderBy { input, .. } => projection_of(input),
        _ => Vec::new(),
    }
}

/// Graph scope for scans (SPARQL dataset semantics: plain BGPs match the
/// default graph; `GRAPH <g>` fixes a named graph, and `GRAPH ?g`
/// enumerates named graphs — binding the variable *outside* the subtree,
/// per §18.3's join semantics).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Scope {
    Default,
    /// A fixed named graph (column value ≥ 1).
    Named(u64),
}

/// Dataset view for update WHERE evaluation (USING / USING NAMED,
/// doc 05 §2-adjacent): an optional default-graph union and an optional
/// named-graph restriction. Query FROM clauses stay a harness/protocol
/// concern for now (`evaluate` rejects them).
#[derive(Debug, Clone, Default)]
pub(crate) struct DatasetView {
    /// Graph columns whose union forms the default graph (`None` = the
    /// store's real default graph). Union semantics: member graphs'
    /// triples dedup.
    pub default_union: Option<Vec<u64>>,
    /// Named graphs visible to `GRAPH` (`None` = all).
    pub named: Option<Vec<u64>>,
}

#[derive(Default)]
struct ExtTable {
    bytes: Vec<Vec<u8>>,
    ids: HashMap<Vec<u8>, u32>,
}

pub(crate) struct Evaluator<'a> {
    pub(crate) snap: &'a Snapshot,
    pub(crate) vars: &'a VarTable,
    /// Prologue BASE (runtime `IRI()` resolution).
    pub(crate) base: Option<String>,
    ext: ExtTable,
    regexes: HashMap<(String, String), regex::Regex>,
    /// CONSTRUCT-template blank-node epoch (fresh nodes per row).
    pub(crate) fresh_bnode: u64,
    /// `BNODE(str)` key map (stable within one evaluation, §17.4.2.9).
    bnode_keys: HashMap<String, u64>,
    /// Generator for evaluation-local blank nodes (`BNODE`/`BNODE(str)`).
    gen_bnode: u64,
    /// Process-unique evaluation scope for generated and template blanks.
    bnode_scope: u64,
    /// xorshift64* state for RAND/UUID/STRUUID.
    rng: u64,
    /// NOW() — fixed for the whole evaluation (§17.4.5.1).
    now: Option<String>,
    /// Graph scope active for the expression under evaluation (EXISTS
    /// patterns inherit it — §18.6 substitute semantics keep the enclosing
    /// GRAPH context).
    pub(crate) expr_scope: Scope,
    /// Update-time dataset override (USING / USING NAMED).
    pub(crate) dataset: DatasetView,
}

impl<'a> Evaluator<'a> {
    pub(crate) fn new(
        snap: &'a Snapshot,
        vars: &'a VarTable,
        base: Option<String>,
        dataset: DatasetView,
    ) -> Evaluator<'a> {
        Evaluator {
            snap,
            vars,
            base,
            ext: ExtTable::default(),
            regexes: HashMap::new(),
            fresh_bnode: 0,
            bnode_keys: HashMap::new(),
            gen_bnode: 0,
            bnode_scope: BNODE_SCOPE.fetch_add(1, Ordering::Relaxed),
            rng: {
                // Seed once per evaluation; quality is irrelevant (RAND/
                // UUID only need distinctness), determinism is undesirable.
                wall_clock_millis() ^ (vars as *const _ as u64) | 1
            },
            now: None,
            expr_scope: Scope::Default,
            dataset,
        }
    }

    // ------------------------------------------------------------ terms

    /// Intern computed concise bytes: a store term when it exists
    /// anywhere, else a query-local Ext id.
    pub(crate) fn intern(&mut self, bytes: Vec<u8>) -> B {
        for pos in [
            TermPos::Object,
            TermPos::Subject,
            TermPos::Predicate,
            TermPos::Graph,
        ] {
            if let Some(id) = self.snap.resolve(&bytes, pos) {
                return B::Id(id);
            }
        }
        self.intern_local(bytes)
    }

    /// Intern an evaluator-local value without resolving it into the store.
    /// `BNODE()` identities must remain distinct from every dataset blank,
    /// even when their private lexical labels happen to match.
    fn intern_local(&mut self, bytes: Vec<u8>) -> B {
        if let Some(&i) = self.ext.ids.get(&bytes) {
            return B::Ext(i);
        }
        let i = self.ext.bytes.len() as u32;
        self.ext.ids.insert(bytes.clone(), i);
        self.ext.bytes.push(bytes);
        B::Ext(i)
    }

    pub(crate) fn bytes_of(&self, b: B) -> Result<Vec<u8>, EngineError> {
        match b {
            B::Id(id) => Ok(self.snap.decode(id)?),
            B::Ext(i) => Ok(self.ext.bytes[i as usize].clone()),
        }
    }

    pub(crate) fn value_of(&self, b: B) -> Result<Value, EngineError> {
        Ok(decode_value(&self.bytes_of(b)?))
    }

    pub(crate) fn template_term(
        &mut self,
        p: &P,
        row: &Row,
    ) -> Result<Option<Vec<u8>>, EngineError> {
        match p {
            P::Term(bytes) => Ok(Some(bytes.clone())),
            P::Var(v) => {
                let name = self.vars.name(*v);
                if let Some(b) = row[v.0 as usize] {
                    return Ok(Some(self.bytes_of(b)?));
                }
                if name.starts_with('.') {
                    // Template blank node: fresh per solution row.
                    return Ok(Some(
                        format!(
                            "_ct{:032x}e{}r{}x{}",
                            crate::fresh::session(),
                            self.bnode_scope,
                            self.fresh_bnode,
                            v.0
                        )
                        .into_bytes(),
                    ));
                }
                Ok(None)
            }
            P::Triple(t) => {
                let (Some(s), Some(p), Some(o)) = (
                    self.template_term(&t.s, row)?,
                    self.template_term(&t.p, row)?,
                    self.template_term(&t.o, row)?,
                ) else {
                    return Ok(None);
                };
                if !matches!(
                    graphy_core::concise::decode(&s),
                    Ok(graphy_core::TermRef::Iri(_) | graphy_core::TermRef::BlankNode(_))
                ) || !matches!(
                    graphy_core::concise::decode(&p),
                    Ok(graphy_core::TermRef::Iri(_))
                ) {
                    return Ok(None);
                }
                let mut out = Vec::new();
                graphy_core::concise::encode_triple_term(&mut out, &s, &p, &o);
                Ok(Some(out))
            }
        }
    }

    /// Term equality across position-local id spaces: equal ids are equal
    /// terms; distinct ids may still name the same term from different
    /// sections (e.g. an IRI bound in object position vs the graphs
    /// section) — compare concise bytes then.
    pub(crate) fn same_term(&mut self, a: B, b: B) -> Result<bool, EngineError> {
        if a == b {
            return Ok(true);
        }
        Ok(self.bytes_of(a)? == self.bytes_of(b)?)
    }

    /// The named-graph column of a bound term, translating through the
    /// concise bytes when the binding carries another position's id.
    pub(crate) fn graph_col_of(&mut self, b: B) -> Result<Option<u64>, EngineError> {
        if let B::Id(id) = b {
            if let Some(col) = self.snap.column(id, TermPos::Graph) {
                return Ok((col > 0).then_some(col));
            }
        }
        let bytes = self.bytes_of(b)?;
        Ok(self
            .snap
            .resolve(&bytes, TermPos::Graph)
            .and_then(|id| self.snap.column(id, TermPos::Graph))
            .filter(|&c| c > 0))
    }

    /// The canonical binding for an enumerated named graph: the same B a parsed or
    /// computed constant of this term interns to, so joins against pre-bound graph
    /// variables compare equal even when the graph's IRI also lives in other term
    /// sections (sections are position-local id spaces).
    pub(crate) fn canonical_graph_b(&mut self, col: u64) -> Result<B, EngineError> {
        let bytes = self.snap.decode_value(col, TermPos::Graph)?;
        Ok(self.intern(bytes))
    }

    /// Whether a named-graph column is visible under the dataset view.
    pub(crate) fn named_visible(&self, col: u64) -> bool {
        match &self.dataset.named {
            Some(named) => named.contains(&col),
            None => true,
        }
    }

    /// Named-graph column values of this snapshot (base then overlay),
    /// for `GRAPH ?g` enumeration.
    pub(crate) fn named_graph_cols(&self) -> Vec<u64> {
        if let Some(named) = &self.dataset.named {
            return named.clone();
        }
        let mut out = Vec::new();
        let mut c = 1u64;
        while self.snap.decode_value(c, TermPos::Graph).is_ok() {
            out.push(c);
            c += 1;
        }
        out
    }

    // ------------------------------------------------------------- eval

    pub(crate) fn eval(
        &mut self,
        a: &Algebra,
        scope: &Scope,
        seed: &Row,
    ) -> Result<Vec<Row>, EngineError> {
        match a {
            Algebra::Bgp(patterns) => self.eval_bgp(patterns, scope, seed),
            Algebra::Path { s, path, o } => {
                let mut out = Vec::new();
                self.eval_path(s, path, o, scope, seed.clone(), &mut out)?;
                Ok(out)
            }
            Algebra::Join(l, r) => {
                let left = self.eval(l, scope, seed)?;
                let right = self.eval(r, scope, seed)?;
                let mut out = Vec::new();
                for a in &left {
                    for b in &right {
                        if let Some(m) = merge(a, b) {
                            out.push(m);
                        }
                    }
                }
                Ok(out)
            }
            Algebra::LeftJoin { left, right, expr } => {
                let l = self.eval(left, scope, seed)?;
                let r = self.eval(right, scope, seed)?;
                self.expr_scope = *scope;
                let mut out = Vec::new();
                for a in &l {
                    let mut extended = false;
                    for b in &r {
                        if let Some(m) = merge(a, b) {
                            let pass = match expr {
                                Some(e) => self.truthy(e, &m),
                                None => true,
                            };
                            if pass {
                                out.push(m);
                                extended = true;
                            }
                        }
                    }
                    if !extended {
                        out.push(a.clone());
                    }
                }
                Ok(out)
            }
            Algebra::Filter { expr, input } => {
                let rows = self.eval(input, scope, seed)?;
                self.expr_scope = *scope;
                Ok(rows.into_iter().filter(|r| self.truthy(expr, r)).collect())
            }
            Algebra::Union(l, r) => {
                let mut rows = self.eval(l, scope, seed)?;
                rows.extend(self.eval(r, scope, seed)?);
                Ok(rows)
            }
            Algebra::Graph { graph, input } => match graph {
                P::Term(bytes) => {
                    let Some(col) = pattern_col(self.snap, bytes, TermPos::Graph) else {
                        return Ok(Vec::new());
                    };
                    if !self.named_visible(col) {
                        return Ok(Vec::new());
                    }
                    self.eval(input, &Scope::Named(col), seed)
                }
                P::Var(v) => match seed[v.0 as usize] {
                    // A bound graph variable may hold the term under
                    // another position's id (sections are position-local)
                    // — translate through the concise bytes.
                    Some(b) => match self.graph_col_of(b)? {
                        Some(col) if self.named_visible(col) => {
                            self.eval(input, &Scope::Named(col), seed)
                        }
                        _ => Ok(Vec::new()),
                    },
                    // Unbound: enumerate named graphs and join the binding
                    // in AFTERWARDS (§18.3) — inner solutions never carry
                    // the graph variable, so MINUS disjointness, VALUES
                    // conflicts, and subquery projections behave.
                    None => {
                        let mut out = Vec::new();
                        for col in self.named_graph_cols() {
                            let gid = self.canonical_graph_b(col)?;
                            for mut r in self.eval(input, &Scope::Named(col), seed)? {
                                match r[v.0 as usize] {
                                    // Inner bindings may carry another
                                    // section's id for the same term.
                                    Some(existing) if !self.same_term(existing, gid)? => {}
                                    _ => {
                                        r[v.0 as usize] = Some(gid);
                                        out.push(r);
                                    }
                                }
                            }
                        }
                        Ok(out)
                    }
                },
                P::Triple(_) => Err(EngineError("triple term as graph name".into())),
            },
            Algebra::Service { .. } => Err(EngineError(
                "SERVICE federation is not implemented yet".into(),
            )),
            Algebra::Extend { input, var, expr } => {
                let rows = self.eval(input, scope, seed)?;
                self.expr_scope = *scope;
                Ok(rows
                    .into_iter()
                    .map(|mut r| {
                        if let Ok(b) = self.eval_expr(expr, &r) {
                            r[var.0 as usize] = Some(b);
                        }
                        r
                    })
                    .collect())
            }
            Algebra::Minus(l, r) => {
                let left = self.eval(l, scope, seed)?;
                let right = self.eval(r, scope, seed)?;
                Ok(left
                    .into_iter()
                    .filter(|a| {
                        !right.iter().any(|b| {
                            // §18.5: compatible AND sharing a bound var.
                            merge(a, b).is_some()
                                && a.iter().zip(b).any(|(x, y)| x.is_some() && y.is_some())
                        })
                    })
                    .collect())
            }
            Algebra::Table { vars, rows } => {
                let mut out = Vec::new();
                for row in rows {
                    let mut r = seed.clone();
                    let mut ok = true;
                    for (v, cell) in vars.iter().zip(row) {
                        if let Some(bytes) = cell {
                            let b = self.intern(bytes.clone());
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
                        out.push(r);
                    }
                }
                Ok(out)
            }
            Algebra::ToMultiSet(x) => self.eval(x, scope, seed),
            Algebra::Group {
                keys,
                aggregates,
                input,
            } => self.eval_group(keys, aggregates, input, scope, seed),
            Algebra::OrderBy { input, conditions } => {
                let rows = self.eval(input, scope, seed)?;
                self.expr_scope = *scope;
                Ok(self.sort_rows(conditions, rows))
            }
            Algebra::Project { input, vars } => {
                let rows = self.eval(input, scope, seed)?;
                Ok(rows
                    .into_iter()
                    .map(|r| {
                        let mut p = vec![None; r.len()];
                        for v in vars {
                            p[v.0 as usize] = r[v.0 as usize];
                        }
                        p
                    })
                    .collect())
            }
            Algebra::Distinct(x) => {
                let rows = self.eval(x, scope, seed)?;
                let mut seen = HashSet::new();
                Ok(rows
                    .into_iter()
                    .filter(|r| seen.insert(r.clone()))
                    .collect())
            }
            Algebra::Reduced(x) => self.eval(x, scope, seed),
            Algebra::Slice {
                input,
                offset,
                limit,
            } => {
                let rows = self.eval(input, scope, seed)?;
                Ok(rows
                    .into_iter()
                    .skip(*offset as usize)
                    .take(limit.map_or(usize::MAX, |l| l as usize))
                    .collect())
            }
        }
    }

    // ------------------------------------------------------------- BGP

    fn eval_bgp(
        &mut self,
        patterns: &[TriplePat],
        scope: &Scope,
        seed: &Row,
    ) -> Result<Vec<Row>, EngineError> {
        // Exact-count greedy order (doc 05 §5.1: exact leaf costing).
        // A single pattern has nothing to order, and counting is not
        // free — a per-row EXISTS re-enters here, and a pattern whose
        // bound components are not a prefix of any index order walks
        // the whole delta to count (the 2026-08-07 quadratic commit
        // diff) — so skip costing entirely for the 1-pattern BGP.
        let mut order: Vec<usize> = (0..patterns.len()).collect();
        if patterns.len() > 1 {
            let mut counts = Vec::with_capacity(patterns.len());
            for t in patterns {
                counts.push(self.pattern_count(t, scope)?);
            }
            order.sort_by_key(|&i| counts[i]);
        }

        let mut rows = vec![seed.clone()];
        for &i in &order {
            let t = &patterns[i];
            let mut next = Vec::new();
            for row in &rows {
                self.scan_pattern(t, scope, row, &mut next)?;
            }
            rows = next;
            if rows.is_empty() {
                break;
            }
        }
        Ok(rows)
    }

    /// Match-count *estimate* for costing (join ordering, leaf
    /// costing): exact over the base, upper-bound over the delta —
    /// [`Snapshot::count_estimate`] — so a delta-resident store never
    /// pays a whole-delta walk per costed pattern.
    pub(crate) fn pattern_count(&self, t: &TriplePat, scope: &Scope) -> Result<u64, EngineError> {
        if *scope == Scope::Default {
            if let Some(cols) = self.dataset.default_union.clone() {
                // Ordering heuristic only — the union overcount is fine.
                let mut n = 0u64;
                for col in cols {
                    n += self.pattern_count(t, &Scope::Named(col))?;
                }
                return Ok(n);
            }
        }
        let Some(pat) = self.pattern_of(t, scope, None)? else {
            return Ok(0);
        };
        Ok(self.snap.count_estimate(&pat)?)
    }

    /// Build a storage [`Pattern`] for a triple under `row` bindings;
    /// `None` = provably empty.
    pub(crate) fn pattern_of(
        &self,
        t: &TriplePat,
        scope: &Scope,
        row: Option<&Row>,
    ) -> Result<Option<Pattern>, EngineError> {
        pattern_of_in(self.snap, t, scope, row)
    }

    pub(crate) fn scan_pattern(
        &mut self,
        t: &TriplePat,
        scope: &Scope,
        row: &Row,
        out: &mut Vec<Row>,
    ) -> Result<(), EngineError> {
        scan_rows(self.snap, &self.dataset, t, scope, row, out)
    }

    // ------------------------------------------------------------ paths

    pub(crate) fn eval_path(
        &mut self,
        s: &P,
        path: &PathExpr,
        o: &P,
        scope: &Scope,
        row: Row,
        out: &mut Vec<Row>,
    ) -> Result<(), EngineError> {
        let s_bound = self.endpoint(s, &row)?;
        let o_bound = self.endpoint(o, &row)?;
        match (s_bound, o_bound) {
            (Some(from), Some(to)) => {
                if self
                    .reachable(path, from, scope, matches!(s, P::Term(_)))?
                    .contains(&to)
                {
                    out.push(row);
                }
            }
            (Some(from), None) => {
                for id in self.reachable(path, from, scope, matches!(s, P::Term(_)))? {
                    let mut r = row.clone();
                    if bind_endpoint(o, id, &mut r) {
                        out.push(r);
                    }
                }
            }
            (None, Some(to)) => {
                for id in self.reachable(
                    &PathExpr::Inverse(Box::new(path.clone())),
                    to,
                    scope,
                    matches!(o, P::Term(_)),
                )? {
                    let mut r = row.clone();
                    if bind_endpoint(s, id, &mut r) {
                        out.push(r);
                    }
                }
            }
            (None, None) => {
                for id in self.all_nodes(scope)? {
                    for target in self.reach(path, id, scope)? {
                        let mut r = row.clone();
                        if bind_endpoint(s, B::Id(id), &mut r)
                            && bind_endpoint(o, B::Id(target), &mut r)
                        {
                            out.push(r);
                        }
                    }
                }
            }
        }
        Ok(())
    }

    fn endpoint(&mut self, p: &P, row: &Row) -> Result<Option<B>, EngineError> {
        Ok(match p {
            // Keep absent constants as evaluator-local terms. SPARQL
            // zero-length paths are identity relations and therefore bind
            // an absent constant to itself even though no store column
            // exists for it.
            P::Term(bytes) => Some(self.intern(bytes.clone())),
            P::Var(v) => row[v.0 as usize],
            P::Triple(_) => {
                return Err(EngineError("triple-term path endpoints".into()));
            }
        })
    }

    /// Reachable bindings, retaining an evaluator-local endpoint for the
    /// zero-length case. Non-store terms cannot participate in an edge.
    fn reachable(
        &mut self,
        path: &PathExpr,
        from: B,
        scope: &Scope,
        retain_absent_constant: bool,
    ) -> Result<Vec<B>, EngineError> {
        match from {
            B::Id(id)
                if !retain_absent_constant
                    && path_nullable(path)
                    && !self.all_nodes(scope)?.contains(&id) =>
            {
                Ok(Vec::new())
            }
            B::Id(id) => Ok(self
                .reach(path, id, scope)?
                .into_iter()
                .map(B::Id)
                .collect()),
            B::Ext(_) if retain_absent_constant && path_nullable(path) => Ok(vec![from]),
            B::Ext(_) => Ok(Vec::new()),
        }
    }

    /// Reachable set from `from` (closure semantics per §9: visited-set,
    /// not path enumeration).
    fn reach(
        &mut self,
        path: &PathExpr,
        from: TermId,
        scope: &Scope,
    ) -> Result<Vec<TermId>, EngineError> {
        match path {
            PathExpr::ZeroOrMore(inner) => self.closure(inner, from, scope, true),
            PathExpr::OneOrMore(inner) => self.closure(inner, from, scope, false),
            PathExpr::ZeroOrOne(inner) => {
                let mut out = vec![from];
                for id in self.step(inner, from, scope)? {
                    if !out.contains(&id) {
                        out.push(id);
                    }
                }
                Ok(out)
            }
            other => self.step(other, from, scope),
        }
    }

    fn closure(
        &mut self,
        inner: &PathExpr,
        from: TermId,
        scope: &Scope,
        include_start: bool,
    ) -> Result<Vec<TermId>, EngineError> {
        let mut visited = HashSet::new();
        let mut frontier = vec![from];
        let mut out = Vec::new();
        if include_start {
            visited.insert(from);
            out.push(from);
        }
        while let Some(cur) = frontier.pop() {
            for next in self.step(inner, cur, scope)? {
                if visited.insert(next) {
                    out.push(next);
                    frontier.push(next);
                }
            }
        }
        Ok(out)
    }

    /// One step of a path expression.
    fn step(
        &mut self,
        path: &PathExpr,
        from: TermId,
        scope: &Scope,
    ) -> Result<Vec<TermId>, EngineError> {
        match path {
            PathExpr::Link(iri) => self.neighbors(from, Some(iri), scope, false, &[]),
            PathExpr::Inverse(inner) => match &**inner {
                PathExpr::Link(iri) => self.neighbors(from, Some(iri), scope, true, &[]),
                PathExpr::Inverse(x) => self.step(x, from, scope),
                PathExpr::Seq(a, b) => self.step(
                    &PathExpr::Seq(
                        Box::new(PathExpr::Inverse(b.clone())),
                        Box::new(PathExpr::Inverse(a.clone())),
                    ),
                    from,
                    scope,
                ),
                PathExpr::Alt(a, b) => self.step(
                    &PathExpr::Alt(
                        Box::new(PathExpr::Inverse(a.clone())),
                        Box::new(PathExpr::Inverse(b.clone())),
                    ),
                    from,
                    scope,
                ),
                PathExpr::ZeroOrMore(x) => {
                    self.closure(&PathExpr::Inverse(x.clone()), from, scope, true)
                }
                PathExpr::OneOrMore(x) => {
                    self.closure(&PathExpr::Inverse(x.clone()), from, scope, false)
                }
                PathExpr::ZeroOrOne(x) => {
                    let mut out = vec![from];
                    for id in self.step(&PathExpr::Inverse(x.clone()), from, scope)? {
                        if !out.contains(&id) {
                            out.push(id);
                        }
                    }
                    Ok(out)
                }
                PathExpr::Nps(items) => {
                    let flipped: Vec<(Vec<u8>, bool)> =
                        items.iter().map(|(i, inv)| (i.clone(), !inv)).collect();
                    self.step(&PathExpr::Nps(flipped), from, scope)
                }
            },
            PathExpr::Seq(a, b) => {
                let mut out = Vec::new();
                for mid in self.step_or_reach(a, from, scope)? {
                    for id in self.step_or_reach(b, mid, scope)? {
                        if !out.contains(&id) {
                            out.push(id);
                        }
                    }
                }
                Ok(out)
            }
            PathExpr::Alt(a, b) => {
                let mut out = self.step_or_reach(a, from, scope)?;
                for id in self.step_or_reach(b, from, scope)? {
                    if !out.contains(&id) {
                        out.push(id);
                    }
                }
                Ok(out)
            }
            PathExpr::ZeroOrMore(_) | PathExpr::OneOrMore(_) | PathExpr::ZeroOrOne(_) => {
                self.reach(path, from, scope)
            }
            PathExpr::Nps(items) => {
                let fwd: Vec<&[u8]> = items
                    .iter()
                    .filter(|(_, inv)| !inv)
                    .map(|(i, _)| i.as_slice())
                    .collect();
                let rev: Vec<&[u8]> = items
                    .iter()
                    .filter(|(_, inv)| *inv)
                    .map(|(i, _)| i.as_slice())
                    .collect();
                let mut out = Vec::new();
                if !fwd.is_empty() || rev.is_empty() {
                    out.extend(self.neighbors(from, None, scope, false, &fwd)?);
                }
                if !rev.is_empty() {
                    for id in self.neighbors(from, None, scope, true, &rev)? {
                        if !out.contains(&id) {
                            out.push(id);
                        }
                    }
                }
                Ok(out)
            }
        }
    }

    /// Nested closures inside seq/alt take their full reachable sets.
    fn step_or_reach(
        &mut self,
        path: &PathExpr,
        from: TermId,
        scope: &Scope,
    ) -> Result<Vec<TermId>, EngineError> {
        self.reach(path, from, scope)
    }

    /// Forward or reverse neighbors over one predicate (or all-but-set
    /// for NPS).
    fn neighbors(
        &mut self,
        from: TermId,
        link: Option<&[u8]>,
        scope: &Scope,
        reverse: bool,
        exclude: &[&[u8]],
    ) -> Result<Vec<TermId>, EngineError> {
        let (from_pos, to_pos) = if reverse {
            (TermPos::Object, TermPos::Subject)
        } else {
            (TermPos::Subject, TermPos::Object)
        };
        let Some(from_col) = self.snap.column(from, from_pos) else {
            return Ok(Vec::new());
        };
        let mut pat = Pattern::default();
        match from_pos {
            TermPos::Subject => pat.s = Some(from_col),
            _ => pat.o = Some(from_col),
        }
        if let Some(iri) = link {
            let Some(pcol) = pattern_col(self.snap, iri, TermPos::Predicate) else {
                return Ok(Vec::new());
            };
            pat.p = Some(pcol);
        }
        pat.g = match scope {
            Scope::Default => Some(0),
            Scope::Named(col) => Some(*col),
        };
        let excluded: HashSet<u64> = exclude
            .iter()
            .filter_map(|iri| pattern_col(self.snap, iri, TermPos::Predicate))
            .collect();
        let mut out = Vec::new();
        let mut scan = self.snap.scan_best(&pat)?;
        let mut batch = QuadBatch::new();
        let mut seen = HashSet::new();
        while scan.next_batch(&mut batch)? {
            for i in 0..batch.len() {
                if !excluded.is_empty() && excluded.contains(&batch.p[i]) {
                    continue;
                }
                let col = match to_pos {
                    TermPos::Subject => batch.s[i],
                    _ => batch.o[i],
                };
                let id = self.snap.term_id(col, to_pos);
                if seen.insert(id) {
                    out.push(id);
                }
            }
        }
        Ok(out)
    }

    /// Every node (subject or object) in scope — the zero-length-path
    /// domain for fully unbound closure paths.
    fn all_nodes(&mut self, scope: &Scope) -> Result<Vec<TermId>, EngineError> {
        let pat = Pattern {
            g: match scope {
                Scope::Default => Some(0),
                Scope::Named(col) => Some(*col),
            },
            ..Pattern::default()
        };
        let mut out = Vec::new();
        let mut seen = HashSet::new();
        let mut scan = self.snap.scan_best(&pat)?;
        let mut batch = QuadBatch::new();
        while scan.next_batch(&mut batch)? {
            for i in 0..batch.len() {
                for (col, pos) in [
                    (batch.s[i], TermPos::Subject),
                    (batch.o[i], TermPos::Object),
                ] {
                    let id = self.snap.term_id(col, pos);
                    if seen.insert(id) {
                        out.push(id);
                    }
                }
            }
        }
        Ok(out)
    }

    // ------------------------------------------------------- aggregation

    fn eval_group(
        &mut self,
        keys: &[(VarId, Option<Expression>)],
        aggregates: &[(VarId, AggregateExpr)],
        input: &Algebra,
        scope: &Scope,
        seed: &Row,
    ) -> Result<Vec<Row>, EngineError> {
        let rows = self.eval(input, scope, seed)?;
        self.expr_scope = *scope;
        self.group_rows(keys, aggregates, rows, seed)
    }

    /// Group + aggregate already-materialized rows (shared with the
    /// vectorized engine's Group operator).
    pub(crate) fn group_rows(
        &mut self,
        keys: &[(VarId, Option<Expression>)],
        aggregates: &[(VarId, AggregateExpr)],
        rows: Vec<Row>,
        seed: &Row,
    ) -> Result<Vec<Row>, EngineError> {
        let mut groups: Vec<(Vec<Option<B>>, Vec<Row>)> = Vec::new();
        let mut index: HashMap<Vec<Option<B>>, usize> = HashMap::new();
        for r in rows {
            let key: Vec<Option<B>> = keys
                .iter()
                .map(|(v, e)| match e {
                    Some(e) => self.eval_expr(e, &r).ok(),
                    None => r[v.0 as usize],
                })
                .collect();
            let at = *index.entry(key.clone()).or_insert_with(|| {
                groups.push((key, Vec::new()));
                groups.len() - 1
            });
            groups[at].1.push(r);
        }
        // A group-less aggregation over zero rows yields one empty group.
        if keys.is_empty() && groups.is_empty() {
            groups.push((Vec::new(), Vec::new()));
        }
        let mut out = Vec::new();
        for (key, members) in groups {
            let mut row = seed.clone();
            for ((v, _), k) in keys.iter().zip(&key) {
                row[v.0 as usize] = *k;
            }
            for (v, agg) in aggregates {
                if let Some(b) = self.eval_aggregate(agg, &members)? {
                    row[v.0 as usize] = Some(b);
                }
            }
            out.push(row);
        }
        Ok(out)
    }

    /// Sort rows by ORDER BY conditions (§18.2.5.1's total order; shared
    /// with the vectorized engine's Sort operator).
    pub(crate) fn sort_rows(
        &mut self,
        conditions: &[(Expression, bool)],
        rows: Vec<Row>,
    ) -> Vec<Row> {
        let mut keyed: Vec<(Vec<Option<Value>>, Row)> = rows
            .into_iter()
            .map(|r| {
                let k = conditions
                    .iter()
                    .map(|(e, _)| {
                        self.eval_expr(e, &r)
                            .ok()
                            .and_then(|b| self.value_of(b).ok())
                    })
                    .collect();
                (k, r)
            })
            .collect();
        keyed.sort_by(|(ka, _), (kb, _)| {
            for ((a, b), (_, desc)) in ka.iter().zip(kb).zip(conditions) {
                let o = match (a, b) {
                    (None, None) => std::cmp::Ordering::Equal,
                    (None, Some(_)) => std::cmp::Ordering::Less,
                    (Some(_), None) => std::cmp::Ordering::Greater,
                    (Some(x), Some(y)) => order_cmp(x, y),
                };
                let o = if *desc { o.reverse() } else { o };
                if o != std::cmp::Ordering::Equal {
                    return o;
                }
            }
            std::cmp::Ordering::Equal
        });
        keyed.into_iter().map(|(_, r)| r).collect()
    }

    pub(crate) fn eval_aggregate(
        &mut self,
        agg: &AggregateExpr,
        rows: &[Row],
    ) -> Result<Option<B>, EngineError> {
        // Argument sequence (errors skipped per spec; COUNT(*) counts rows).
        let mut args: Vec<B> = Vec::new();
        match &agg.expr {
            None => {
                let n = rows.len() as i64;
                let b = self.intern(encode_value(&Value::Num(Num::Int(n))));
                return Ok(Some(b));
            }
            Some(e) => {
                for r in rows {
                    if let Ok(b) = self.eval_expr(e, r) {
                        args.push(b);
                    }
                }
            }
        }
        if agg.distinct {
            let mut seen = HashSet::new();
            args.retain(|b| seen.insert(*b));
        }
        let result = match agg.func {
            Aggregate::Count => Some(Value::Num(Num::Int(args.len() as i64))),
            Aggregate::Sample => {
                return Ok(args.first().copied());
            }
            Aggregate::Sum | Aggregate::Avg => {
                let mut acc = Value::Num(Num::Int(0));
                let n = args.len();
                for b in args {
                    let v = self.value_of(b)?;
                    match arith(ArithOp::Add, &acc, &v) {
                        Some(next) => acc = next,
                        None => return Ok(None),
                    }
                }
                if agg.func == Aggregate::Avg {
                    if n == 0 {
                        Some(Value::Num(Num::Int(0)))
                    } else {
                        arith(ArithOp::Div, &acc, &Value::Num(Num::Int(n as i64)))
                    }
                } else {
                    Some(acc)
                }
            }
            Aggregate::Min | Aggregate::Max => {
                let mut best: Option<Value> = None;
                for b in args {
                    let v = self.value_of(b)?;
                    best = Some(match best {
                        None => v,
                        Some(cur) => {
                            let take = order_cmp(&v, &cur)
                                == if agg.func == Aggregate::Min {
                                    std::cmp::Ordering::Less
                                } else {
                                    std::cmp::Ordering::Greater
                                };
                            if take {
                                v
                            } else {
                                cur
                            }
                        }
                    });
                }
                best
            }
            Aggregate::GroupConcat => {
                let sep = agg.separator.as_deref().unwrap_or(" ");
                let mut parts = Vec::new();
                for b in args {
                    match str_of(&self.value_of(b)?) {
                        Some(s) => parts.push(s),
                        None => return Ok(None),
                    }
                }
                Some(Value::Str {
                    lex: parts.join(sep),
                    lang: None,
                })
            }
        };
        Ok(result.map(|v| self.intern(encode_value(&v))))
    }

    // ------------------------------------------------------- expressions

    pub(crate) fn truthy(&mut self, e: &Expression, row: &Row) -> bool {
        self.eval_expr(e, row)
            .ok()
            .and_then(|b| self.value_of(b).ok())
            .and_then(|v| ebv(&v))
            .unwrap_or(false)
    }

    pub(crate) fn eval_expr(&mut self, e: &Expression, row: &Row) -> Result<B, ()> {
        use Expression as E;
        match e {
            E::Term(bytes) => Ok(self.intern(bytes.clone())),
            E::Var(v) => row[v.0 as usize].ok_or(()),
            E::Or(a, b) => {
                let x = self.expr_ebv(a, row);
                let y = self.expr_ebv(b, row);
                match (x, y) {
                    (Some(true), _) | (_, Some(true)) => Ok(self.bool_b(true)),
                    (Some(false), Some(false)) => Ok(self.bool_b(false)),
                    _ => Err(()),
                }
            }
            E::And(a, b) => {
                let x = self.expr_ebv(a, row);
                let y = self.expr_ebv(b, row);
                match (x, y) {
                    (Some(false), _) | (_, Some(false)) => Ok(self.bool_b(false)),
                    (Some(true), Some(true)) => Ok(self.bool_b(true)),
                    _ => Err(()),
                }
            }
            E::Not(a) => {
                let v = self.expr_ebv(a, row).ok_or(())?;
                Ok(self.bool_b(!v))
            }
            E::Cmp(op, a, b) => {
                let x = self.expr_value(a, row)?;
                let y = self.expr_value(b, row)?;
                let r = match op {
                    CmpOp::Eq => eq_values(&x, &y).ok_or(())?,
                    CmpOp::Ne => !eq_values(&x, &y).ok_or(())?,
                    _ => {
                        let o = cmp_values(&x, &y).ok_or(())?;
                        match op {
                            CmpOp::Lt => o == std::cmp::Ordering::Less,
                            CmpOp::Le => o != std::cmp::Ordering::Greater,
                            CmpOp::Gt => o == std::cmp::Ordering::Greater,
                            CmpOp::Ge => o != std::cmp::Ordering::Less,
                            _ => unreachable!(),
                        }
                    }
                };
                Ok(self.bool_b(r))
            }
            E::In {
                expr,
                list,
                negated,
            } => {
                let x = self.expr_value(expr, row)?;
                let mut found = false;
                let mut errored = false;
                for item in list {
                    match self.expr_value(item, row) {
                        Ok(y) => match eq_values(&x, &y) {
                            Some(true) => {
                                found = true;
                                break;
                            }
                            Some(false) => {}
                            None => errored = true,
                        },
                        Err(()) => errored = true,
                    }
                }
                if !found && errored {
                    return Err(());
                }
                Ok(self.bool_b(found != *negated))
            }
            E::Add(a, b) => self.arith_expr(ArithOp::Add, a, b, row),
            E::Sub(a, b) => self.arith_expr(ArithOp::Sub, a, b, row),
            E::Mul(a, b) => self.arith_expr(ArithOp::Mul, a, b, row),
            E::Div(a, b) => self.arith_expr(ArithOp::Div, a, b, row),
            E::UnaryMinus(a) => {
                let v = self.expr_value(a, row)?;
                let z = Value::Num(Num::Int(0));
                let out = arith(ArithOp::Sub, &z, &v).ok_or(())?;
                Ok(self.intern(encode_value(&out)))
            }
            E::UnaryPlus(a) => {
                let v = self.expr_value(a, row)?;
                matches!(v, Value::Num(_)).then(|| ()).ok_or(())?;
                self.eval_expr(a, row)
            }
            E::Builtin(b, args) => self.builtin(*b, args, row),
            E::Function { iri, args, .. } => {
                // §17.5: XPath constructor functions (casts) by XSD IRI;
                // other extension functions are unsupported (error).
                let iri_s = std::str::from_utf8(iri)
                    .ok()
                    .and_then(|s| s.strip_prefix('>'))
                    .ok_or(())?;
                let local = iri_s
                    .strip_prefix("http://www.w3.org/2001/XMLSchema#")
                    .ok_or(())?;
                if args.len() != 1 {
                    return Err(());
                }
                let v = self.expr_value(&args[0], row)?;
                let out = cast_xsd(local, v).ok_or(())?;
                Ok(self.intern(encode_value(&out)))
            }
            E::Exists { negated, pattern } => {
                let scope = self.expr_scope;
                let rows = self.eval(pattern, &scope, row).map_err(|_| ())?;
                self.expr_scope = scope; // subtree may have overwritten it
                Ok(self.bool_b(rows.is_empty() == *negated))
            }
            E::TripleTerm { s, p, o } => {
                let sv = self.eval_expr(s, row)?;
                let pv = self.eval_expr(p, row)?;
                let ov = self.eval_expr(o, row)?;
                let (sb, pb, ob) = (
                    self.bytes_of(sv).map_err(|_| ())?,
                    self.bytes_of(pv).map_err(|_| ())?,
                    self.bytes_of(ov).map_err(|_| ())?,
                );
                if !matches!(
                    graphy_core::concise::decode(&sb),
                    Ok(graphy_core::TermRef::Iri(_) | graphy_core::TermRef::BlankNode(_))
                ) || !matches!(
                    graphy_core::concise::decode(&pb),
                    Ok(graphy_core::TermRef::Iri(_))
                ) {
                    return Err(());
                }
                let mut out = Vec::new();
                graphy_core::concise::encode_triple_term(&mut out, &sb, &pb, &ob);
                let has_local_blank = [(sv, &sb), (pv, &pb), (ov, &ob)]
                    .into_iter()
                    .any(|(b, bytes)| matches!(b, B::Ext(_)) && concise_has_bnode(bytes));
                Ok(if has_local_blank {
                    self.intern_local(out)
                } else {
                    self.intern(out)
                })
            }
        }
    }

    fn expr_value(&mut self, e: &Expression, row: &Row) -> Result<Value, ()> {
        let b = self.eval_expr(e, row)?;
        self.value_of(b).map_err(|_| ())
    }

    fn expr_ebv(&mut self, e: &Expression, row: &Row) -> Option<bool> {
        self.eval_expr(e, row)
            .ok()
            .and_then(|b| self.value_of(b).ok())
            .and_then(|v| ebv(&v))
    }

    fn bool_b(&mut self, v: bool) -> B {
        self.intern(encode_value(&Value::Bool(v)))
    }

    fn arith_expr(
        &mut self,
        op: ArithOp,
        a: &Expression,
        b: &Expression,
        row: &Row,
    ) -> Result<B, ()> {
        let x = self.expr_value(a, row)?;
        let y = self.expr_value(b, row)?;
        let out = arith(op, &x, &y).ok_or(())?;
        Ok(self.intern(encode_value(&out)))
    }

    fn builtin(&mut self, b: Builtin, args: &[Expression], row: &Row) -> Result<B, ()> {
        use Builtin as F;
        let out: Value = match b {
            F::Bound => {
                let ok = self.eval_expr(&args[0], row).is_ok();
                Value::Bool(ok)
            }
            F::Coalesce => {
                for a in args {
                    if let Ok(v) = self.eval_expr(a, row) {
                        return Ok(v);
                    }
                }
                return Err(());
            }
            F::If => {
                let c = self.expr_ebv(&args[0], row).ok_or(())?;
                return self.eval_expr(&args[if c { 1 } else { 2 }], row);
            }
            F::SameTerm => {
                let x = self.eval_expr(&args[0], row)?;
                let y = self.eval_expr(&args[1], row)?;
                Value::Bool(x == y)
            }
            F::Str => {
                let binding = self.eval_expr(&args[0], row)?;
                let bytes = self.bytes_of(binding).map_err(|_| ())?;
                let lex = match graphy_core::concise::decode(&bytes).map_err(|_| ())? {
                    graphy_core::TermRef::Iri(i) => i.to_owned(),
                    graphy_core::TermRef::Literal(l) => l.lexical().to_owned(),
                    graphy_core::TermRef::BlankNode(_) | graphy_core::TermRef::TripleTerm(_) => {
                        return Err(())
                    }
                };
                Value::Str { lex, lang: None }
            }
            F::Lang => match self.expr_value(&args[0], row)? {
                Value::Str {
                    lang: Some((tag, _)),
                    ..
                } => Value::Str {
                    lex: tag,
                    lang: None,
                },
                Value::Str { .. }
                | Value::Num(_)
                | Value::Bool(_)
                | Value::DateTime { .. }
                | Value::Typed { .. } => Value::Str {
                    lex: String::new(),
                    lang: None,
                },
                _ => return Err(()),
            },
            F::LangDir => match self.expr_value(&args[0], row)? {
                Value::Str {
                    lang: Some((_, Some(dir))),
                    ..
                } => Value::Str {
                    lex: match dir {
                        graphy_core::Dir::Ltr => "ltr",
                        graphy_core::Dir::Rtl => "rtl",
                    }
                    .to_owned(),
                    lang: None,
                },
                Value::Str { .. }
                | Value::Num(_)
                | Value::Bool(_)
                | Value::DateTime { .. }
                | Value::Typed { .. } => Value::Str {
                    lex: String::new(),
                    lang: None,
                },
                _ => return Err(()),
            },
            F::HasLang => Value::Bool(matches!(
                self.expr_value(&args[0], row)?,
                Value::Str { lang: Some(_), .. }
            )),
            F::HasLangDir => Value::Bool(matches!(
                self.expr_value(&args[0], row)?,
                Value::Str {
                    lang: Some((_, Some(_))),
                    ..
                }
            )),
            F::Datatype => {
                let dt = match self.expr_value(&args[0], row)? {
                    Value::Str { lang: None, .. } => graphy_core::vocab::XSD_STRING.to_owned(),
                    Value::Str {
                        lang: Some((_, None)),
                        ..
                    } => graphy_core::vocab::RDF_LANG_STRING.to_owned(),
                    Value::Str {
                        lang: Some((_, Some(_))),
                        ..
                    } => graphy_core::vocab::RDF_DIR_LANG_STRING.to_owned(),
                    Value::Num(Num::Int(_)) => graphy_core::vocab::XSD_INTEGER.to_owned(),
                    Value::Num(Num::IntSub(_, dt)) => (*dt).to_owned(),
                    Value::Num(Num::Dec(_)) => graphy_core::vocab::XSD_DECIMAL.to_owned(),
                    Value::Num(Num::Flt(_)) => graphy_core::vocab::XSD_FLOAT.to_owned(),
                    Value::Num(Num::Dbl(_)) => graphy_core::vocab::XSD_DOUBLE.to_owned(),
                    Value::Bool(_) => graphy_core::vocab::XSD_BOOLEAN.to_owned(),
                    Value::DateTime { dt, .. } => dt,
                    Value::Typed { dt, .. } => dt,
                    _ => return Err(()),
                };
                Value::Iri(dt)
            }
            F::Iri => {
                let reference = match self.expr_value(&args[0], row)? {
                    Value::Iri(i) => i,
                    Value::Str { lex, lang: None } => lex,
                    _ => return Err(()),
                };
                // §17.4.2.8: resolve against the prologue base.
                let abs = if graphy_core::iri::validate_iri(&reference).is_ok() {
                    reference
                } else {
                    let base = self.base.as_deref().ok_or(())?;
                    graphy_core::iri::resolve(base, &reference).map_err(|_| ())?
                };
                Value::Iri(abs)
            }
            F::IsIri => Value::Bool(matches!(self.expr_value(&args[0], row)?, Value::Iri(_))),
            F::IsBlank => Value::Bool(matches!(self.expr_value(&args[0], row)?, Value::Blank(_))),
            F::IsLiteral => Value::Bool(matches!(
                self.expr_value(&args[0], row)?,
                Value::Str { .. }
                    | Value::Num(_)
                    | Value::Bool(_)
                    | Value::DateTime { .. }
                    | Value::Typed { .. }
            )),
            F::IsNumeric => Value::Bool(matches!(self.expr_value(&args[0], row)?, Value::Num(_))),
            F::IsTriple => Value::Bool(matches!(self.expr_value(&args[0], row)?, Value::Triple(_))),
            F::StrLen => {
                let s = self.string_arg(&args[0], row)?;
                Value::Num(Num::Int(s.0.chars().count() as i64))
            }
            F::UCase => {
                let (s, lang) = self.string_arg(&args[0], row)?;
                Value::Str {
                    lex: s.to_uppercase(),
                    lang,
                }
            }
            F::LCase => {
                let (s, lang) = self.string_arg(&args[0], row)?;
                Value::Str {
                    lex: s.to_lowercase(),
                    lang,
                }
            }
            F::Contains => {
                let (a, la) = self.string_arg(&args[0], row)?;
                let (b, lb) = self.string_arg(&args[1], row)?;
                if !langs_compatible(&la, &lb) {
                    return Err(());
                }
                Value::Bool(a.contains(&b))
            }
            F::StrStarts => {
                let (a, la) = self.string_arg(&args[0], row)?;
                let (b, lb) = self.string_arg(&args[1], row)?;
                if !langs_compatible(&la, &lb) {
                    return Err(());
                }
                Value::Bool(a.starts_with(&b))
            }
            F::StrEnds => {
                let (a, la) = self.string_arg(&args[0], row)?;
                let (b, lb) = self.string_arg(&args[1], row)?;
                if !langs_compatible(&la, &lb) {
                    return Err(());
                }
                Value::Bool(a.ends_with(&b))
            }
            F::StrBefore => {
                let (a, lang) = self.string_arg(&args[0], row)?;
                let (b, lang2) = self.string_arg(&args[1], row)?;
                if !langs_compatible(&lang, &lang2) {
                    return Err(()); // §17.4.3: argument compatibility
                }
                match a.find(&b) {
                    Some(at) => Value::Str {
                        lex: a[..at].to_owned(),
                        lang,
                    },
                    None => Value::Str {
                        lex: String::new(),
                        lang: None,
                    },
                }
            }
            F::StrAfter => {
                let (a, lang) = self.string_arg(&args[0], row)?;
                let (b, lang2) = self.string_arg(&args[1], row)?;
                if !langs_compatible(&lang, &lang2) {
                    return Err(());
                }
                match a.find(&b) {
                    Some(at) => Value::Str {
                        lex: a[at + b.len()..].to_owned(),
                        lang,
                    },
                    None => Value::Str {
                        lex: String::new(),
                        lang: None,
                    },
                }
            }
            F::Substr => {
                let (s, lang) = self.string_arg(&args[0], row)?;
                let start = match self.expr_value(&args[1], row)? {
                    Value::Num(Num::Int(i)) => i,
                    _ => return Err(()),
                };
                let len = match args.get(2) {
                    Some(a) => match self.expr_value(a, row)? {
                        Value::Num(Num::Int(i)) => Some(i),
                        _ => return Err(()),
                    },
                    None => None,
                };
                let chars: Vec<char> = s.chars().collect();
                let from = (start.max(1) - 1) as usize;
                let taken: String = match len {
                    Some(l) => chars.iter().skip(from).take(l.max(0) as usize).collect(),
                    None => chars.iter().skip(from).collect(),
                };
                Value::Str { lex: taken, lang }
            }
            F::Concat => {
                // §17.4.3.12: the result keeps a language tag only when
                // every argument carries the same one.
                let mut lex = String::new();
                let mut lang: Option<Lang> = None; // None = no args yet
                for a in args {
                    let (s, l) = self.string_arg(a, row)?;
                    lex.push_str(&s);
                    lang = Some(match lang {
                        None => l,
                        Some(prev) if langs_same(&prev, &l) => prev,
                        Some(_) => None,
                    });
                }
                Value::Str {
                    lex,
                    lang: lang.flatten(),
                }
            }
            F::EncodeForUri => {
                let (s, _) = self.string_arg(&args[0], row)?;
                let mut out = String::new();
                for byte in s.bytes() {
                    match byte {
                        b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                            out.push(byte as char)
                        }
                        _ => out.push_str(&format!("%{byte:02X}")),
                    }
                }
                Value::Str {
                    lex: out,
                    lang: None,
                }
            }
            F::LangMatches => {
                let (tag, _) = self.string_arg(&args[0], row)?;
                let (range, _) = self.string_arg(&args[1], row)?;
                let m = if range == "*" {
                    !tag.is_empty()
                } else {
                    let t = tag.to_ascii_lowercase();
                    let r = range.to_ascii_lowercase();
                    t == r || t.starts_with(&format!("{r}-"))
                };
                Value::Bool(m)
            }
            F::StrLang => {
                // §17.4.2.6: both arguments must be simple/xsd:string.
                let (lex, l1) = self.string_arg(&args[0], row)?;
                let (tag, l2) = self.string_arg(&args[1], row)?;
                if l1.is_some() || l2.is_some() || tag.is_empty() {
                    return Err(());
                }
                Value::Str {
                    // Concise lang tags are lowercase-normalized at
                    // construction (RDF compares them case-insensitively).
                    lang: Some((tag.to_ascii_lowercase(), None)),
                    lex,
                }
            }
            F::StrLangDir => {
                let (lex, l1) = self.string_arg(&args[0], row)?;
                let (tag, l2) = self.string_arg(&args[1], row)?;
                let (dir, l3) = self.string_arg(&args[2], row)?;
                if l1.is_some() || l2.is_some() || l3.is_some() || tag.is_empty() {
                    return Err(());
                }
                let dir = match dir.as_str() {
                    "ltr" => graphy_core::Dir::Ltr,
                    "rtl" => graphy_core::Dir::Rtl,
                    _ => return Err(()),
                };
                Value::Str {
                    lex,
                    lang: Some((tag.to_ascii_lowercase(), Some(dir))),
                }
            }
            F::StrDt => {
                let (lex, l1) = self.string_arg(&args[0], row)?;
                if l1.is_some() {
                    return Err(());
                }
                let dt = match self.expr_value(&args[1], row)? {
                    Value::Iri(i) => i,
                    _ => return Err(()),
                };
                // Dedicated-form datatypes: xsd:string is the simple
                // spelling; language-string types cannot be constructed
                // without a tag (§17.4.2.7 → error).
                if dt == graphy_core::vocab::RDF_LANG_STRING
                    || dt == graphy_core::vocab::RDF_DIR_LANG_STRING
                {
                    return Err(());
                }
                let mut out = Vec::new();
                if dt == graphy_core::vocab::XSD_STRING {
                    graphy_core::concise::encode_simple(&mut out, &lex);
                } else {
                    graphy_core::concise::encode_datatype(&mut out, &lex, &dt);
                }
                return Ok(self.intern(out));
            }
            F::Abs | F::Ceil | F::Floor | F::Round => {
                let v = self.expr_value(&args[0], row)?;
                let Value::Num(n) = v else { return Err(()) };
                Value::Num(match (b, n) {
                    (F::Abs, Num::Int(i)) => Num::Int(i.checked_abs().ok_or(())?),
                    (F::Abs, Num::IntSub(i, dt)) => Num::IntSub(i.checked_abs().ok_or(())?, dt),
                    (F::Abs, Num::Dec(d)) => Num::Dec(d.abs().ok_or(())?),
                    (F::Abs, Num::Flt(d)) => Num::Flt(d.abs()),
                    (F::Abs, Num::Dbl(d)) => Num::Dbl(d.abs()),
                    (F::Ceil, Num::Int(i)) => Num::Int(i),
                    (F::Ceil, Num::IntSub(i, dt)) => Num::IntSub(i, dt),
                    (F::Ceil, Num::Dec(d)) => Num::Dec(d.ceil()),
                    (F::Ceil, Num::Flt(d)) => Num::Flt(d.ceil()),
                    (F::Ceil, Num::Dbl(d)) => Num::Dbl(d.ceil()),
                    (F::Floor, Num::Int(i)) => Num::Int(i),
                    (F::Floor, Num::IntSub(i, dt)) => Num::IntSub(i, dt),
                    (F::Floor, Num::Dec(d)) => Num::Dec(d.floor()),
                    (F::Floor, Num::Flt(d)) => Num::Flt(d.floor()),
                    (F::Floor, Num::Dbl(d)) => Num::Dbl(d.floor()),
                    (F::Round, Num::Int(i)) => Num::Int(i),
                    (F::Round, Num::IntSub(i, dt)) => Num::IntSub(i, dt),
                    (F::Round, Num::Dec(d)) => Num::Dec(d.round()),
                    (F::Round, Num::Flt(d)) => Num::Flt(if d.fract() == -0.5 {
                        d.trunc()
                    } else {
                        d.round()
                    }),
                    // F&O fn:round: half toward positive infinity (f64
                    // round() is half-away-from-zero — adjust negatives).
                    (F::Round, Num::Dbl(d)) => Num::Dbl(if d.fract() == -0.5 {
                        d.trunc()
                    } else {
                        d.round()
                    }),
                    _ => unreachable!(),
                })
            }
            F::Regex | F::Replace => {
                let (s, lang) = self.string_arg(&args[0], row)?;
                let (pat, _) = self.string_arg(&args[1], row)?;
                let flags_at = if b == F::Regex { 2 } else { 3 };
                let flags = match args.get(flags_at) {
                    Some(a) => self.string_arg(a, row)?.0,
                    None => String::new(),
                };
                let rep = if b == F::Replace {
                    Some(self.string_arg(&args[2], row)?.0)
                } else {
                    None
                };
                let re = self.compile(&pat, &flags)?;
                match rep {
                    None => Value::Bool(re.is_match(&s)),
                    Some(rep) => Value::Str {
                        lex: re.replace_all(&s, rep.as_str()).into_owned(),
                        lang,
                    },
                }
            }
            F::Subject | F::Predicate | F::Object => {
                let source = self.eval_expr(&args[0], row)?;
                let v = self.value_of(source).map_err(|_| ())?;
                let Value::Triple(bytes) = v else {
                    return Err(());
                };
                let t = graphy_core::concise::decode(&bytes).map_err(|_| ())?;
                let graphy_core::TermRef::TripleTerm(view) = t else {
                    return Err(());
                };
                let component = match b {
                    F::Subject => view.subject(),
                    F::Predicate => view.predicate(),
                    _ => view.object(),
                };
                let mut out = Vec::new();
                write_term_ref(&mut out, &component);
                return Ok(if matches!(source, B::Ext(_)) && concise_has_bnode(&out) {
                    self.intern_local(out)
                } else {
                    self.intern(out)
                });
            }
            F::Triple => {
                let e = Expression::TripleTerm {
                    s: Box::new(args[0].clone()),
                    p: Box::new(args[1].clone()),
                    o: Box::new(args[2].clone()),
                };
                return self.eval_expr(&e, row);
            }
            F::Md5 | F::Sha1 | F::Sha256 | F::Sha384 | F::Sha512 => {
                let (s, lang) = self.string_arg(&args[0], row)?;
                if lang.is_some() {
                    return Err(()); // §17.4.4: simple/xsd:string only
                }
                use md5::Digest as _;
                let hex =
                    |bytes: &[u8]| bytes.iter().map(|b| format!("{b:02x}")).collect::<String>();
                let lex = match b {
                    F::Md5 => hex(&md5::Md5::digest(s.as_bytes())),
                    F::Sha1 => hex(&sha1::Sha1::digest(s.as_bytes())),
                    F::Sha256 => hex(&sha2::Sha256::digest(s.as_bytes())),
                    F::Sha384 => hex(&sha2::Sha384::digest(s.as_bytes())),
                    _ => hex(&sha2::Sha512::digest(s.as_bytes())),
                };
                Value::Str { lex, lang: None }
            }
            F::Year
            | F::Month
            | F::Day
            | F::Hours
            | F::Minutes
            | F::Seconds
            | F::Timezone
            | F::Tz => {
                let Value::DateTime { lex, .. } = self.expr_value(&args[0], row)? else {
                    return Err(());
                };
                let dt = DateTimeParts::parse(&lex).ok_or(())?;
                match b {
                    F::Year => Value::Num(Num::Int(dt.year)),
                    F::Month => Value::Num(Num::Int(dt.month)),
                    F::Day => Value::Num(Num::Int(dt.day)),
                    F::Hours => Value::Num(Num::Int(dt.hours.ok_or(())?)),
                    F::Minutes => Value::Num(Num::Int(dt.minutes.ok_or(())?)),
                    F::Seconds => Value::Num(Num::Dec(dt.seconds.ok_or(())?)),
                    F::Timezone => Value::Typed {
                        lex: dt.tz.ok_or(())?.day_time_duration(),
                        dt: "http://www.w3.org/2001/XMLSchema#dayTimeDuration".to_owned(),
                    },
                    _ => Value::Str {
                        lex: dt.tz.map(|t| t.tz_string()).unwrap_or_default(),
                        lang: None,
                    },
                }
            }
            F::Now => {
                if self.now.is_none() {
                    self.now = Some(now_lexical());
                }
                Value::DateTime {
                    lex: self.now.clone().expect("now set"),
                    dt: graphy_core::vocab::XSD_DATE_TIME.to_owned(),
                }
            }
            F::Rand => {
                // 53 uniform mantissa bits → [0, 1).
                Value::Num(Num::Dbl(
                    (self.next_rand() >> 11) as f64 / (1u64 << 53) as f64,
                ))
            }
            F::Uuid => Value::Iri(format!("urn:uuid:{}", self.gen_uuid())),
            F::StrUuid => Value::Str {
                lex: self.gen_uuid(),
                lang: None,
            },
            F::BNode => {
                let label = match args.first() {
                    None => {
                        self.gen_bnode += 1;
                        format!(
                            "gen{:032x}e{}b{}",
                            crate::fresh::session(),
                            self.bnode_scope,
                            self.gen_bnode
                        )
                    }
                    Some(a) => {
                        let (key, lang) = self.string_arg(a, row)?;
                        if lang.is_some() {
                            return Err(());
                        }
                        // §17.4.2.9: the same argument yields the same
                        // blank node WITHIN one solution mapping, fresh
                        // across solutions. Solution identity = the row's
                        // bindings, with unbound cells and previously
                        // generated blank nodes as one marker (chained
                        // Extends re-present the same solution plus prior
                        // BNODE results).
                        let mut fp = String::new();
                        for cell in row {
                            match cell {
                                Some(B::Id(id)) => fp.push_str(&format!("i{:x},", id.raw())),
                                Some(B::Ext(i))
                                    if !self.ext.bytes[*i as usize].starts_with(b"_gen") =>
                                {
                                    fp.push_str(&format!("e{i},"));
                                }
                                _ => fp.push('n'),
                            }
                        }
                        let full = format!("{fp}\u{0}{key}");
                        let n = match self.bnode_keys.get(&full) {
                            Some(&n) => n,
                            None => {
                                self.gen_bnode += 1;
                                self.bnode_keys.insert(full, self.gen_bnode);
                                self.gen_bnode
                            }
                        };
                        format!(
                            "gen{:032x}e{}b{n}",
                            crate::fresh::session(),
                            self.bnode_scope
                        )
                    }
                };
                return Ok(self.intern_local(encode_value(&Value::Blank(label))));
            }
        };
        Ok(self.intern(encode_value(&out)))
    }

    fn next_rand(&mut self) -> u64 {
        // xorshift64* — cheap non-cryptographic distinctness.
        let mut x = self.rng;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.rng = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    /// Random (version-4-shaped) UUID in canonical lowercase-hex form.
    fn gen_uuid(&mut self) -> String {
        let hi = self.next_rand();
        let lo = self.next_rand();
        let mut bytes = [0u8; 16];
        bytes[..8].copy_from_slice(&hi.to_be_bytes());
        bytes[8..].copy_from_slice(&lo.to_be_bytes());
        bytes[6] = (bytes[6] & 0x0f) | 0x40; // version 4
        bytes[8] = (bytes[8] & 0x3f) | 0x80; // RFC 4122 variant
        let h = |r: std::ops::Range<usize>| {
            bytes[r]
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect::<String>()
        };
        format!(
            "{}-{}-{}-{}-{}",
            h(0..4),
            h(4..6),
            h(6..8),
            h(8..10),
            h(10..16)
        )
    }

    /// A string-typed argument (§17.4.3 accepts simple/lang strings).
    fn string_arg(&mut self, e: &Expression, row: &Row) -> Result<(String, Lang), ()> {
        match self.expr_value(e, row)? {
            Value::Str { lex, lang } => Ok((lex, lang)),
            _ => Err(()),
        }
    }

    fn compile(&mut self, pattern: &str, flags: &str) -> Result<&regex::Regex, ()> {
        let key = (pattern.to_owned(), flags.to_owned());
        if !self.regexes.contains_key(&key) {
            let quoted = flags.contains('q').then(|| regex::escape(pattern));
            let mut builder = regex::RegexBuilder::new(quoted.as_deref().unwrap_or(pattern));
            for f in flags.chars() {
                match f {
                    'i' => {
                        builder.case_insensitive(true);
                    }
                    's' => {
                        builder.dot_matches_new_line(true);
                    }
                    'm' => {
                        builder.multi_line(true);
                    }
                    'x' => {
                        builder.ignore_whitespace(true);
                    }
                    // XPath/XQuery `q`: match the pattern literally.
                    'q' => {}
                    _ => return Err(()),
                }
            }
            let re = builder.size_limit(1 << 20).build().map_err(|_| ())?;
            self.regexes.insert(key.clone(), re);
        }
        Ok(&self.regexes[&key])
    }
}

type Lang = Option<(String, Option<graphy_core::Dir>)>;

/// Same language annotation (tags case-insensitive).
fn langs_same(a: &Lang, b: &Lang) -> bool {
    match (a, b) {
        (None, None) => true,
        (Some((ta, da)), Some((tb, db))) => ta.eq_ignore_ascii_case(tb) && da == db,
        _ => false,
    }
}

/// §17.4.3 argument compatibility: the second string must be simple or
/// carry the same language tag as the first.
fn langs_compatible(a: &Lang, b: &Lang) -> bool {
    match (a, b) {
        (_, None) => true,
        (Some((ta, _)), Some((tb, _))) => ta.eq_ignore_ascii_case(tb),
        (None, Some(_)) => false,
    }
}

/// §17.5 XPath constructor casts (`xsd:<local>(value)`); `None` = error.
fn cast_xsd(local: &str, v: Value) -> Option<Value> {
    use Value as V;
    match local {
        "string" => Some(V::Str {
            lex: match &v {
                // XPath canonical decimal, not the term-lexical form.
                V::Num(Num::Dec(d)) => d.xpath_lexical(),
                V::Num(Num::Flt(d)) => xpath_float_lexical(f64::from(*d), true),
                V::Num(Num::Dbl(d)) => xpath_float_lexical(*d, false),
                V::DateTime { lex, dt } if dt == graphy_core::vocab::XSD_DATE_TIME => {
                    canonical_date_time_lexical(lex)
                }
                _ => str_of(&v)?,
            },
            lang: None,
        }),
        "boolean" => match v {
            V::Bool(b) => Some(V::Bool(b)),
            V::Num(n) => Some(V::Bool(match n {
                Num::Int(i) => i != 0,
                Num::IntSub(i, _) => i != 0,
                Num::Dec(d) => !d.is_zero(),
                Num::Flt(d) => d != 0.0 && !d.is_nan(),
                Num::Dbl(d) => d != 0.0 && !d.is_nan(),
            })),
            V::Str { lex, lang: None } => match lex.trim() {
                "true" | "1" => Some(V::Bool(true)),
                "false" | "0" => Some(V::Bool(false)),
                _ => None,
            },
            _ => None,
        },
        "integer" => match v {
            V::Bool(b) => Some(V::Num(Num::Int(i64::from(b)))),
            V::Num(Num::Int(i)) => Some(V::Num(Num::Int(i))),
            V::Num(Num::IntSub(i, _)) => Some(V::Num(Num::Int(i))),
            V::Num(Num::Dec(d)) => i64::try_from(d.trunc()).ok().map(|i| V::Num(Num::Int(i))),
            V::Num(Num::Flt(d)) => (d.is_finite() && d.trunc().abs() <= i64::MAX as f32)
                .then(|| V::Num(Num::Int(d.trunc() as i64))),
            V::Num(Num::Dbl(d)) => (d.is_finite() && d.trunc().abs() <= i64::MAX as f64)
                .then(|| V::Num(Num::Int(d.trunc() as i64))),
            V::Str { lex, lang: None } => {
                lex.trim().parse::<i64>().ok().map(|i| V::Num(Num::Int(i)))
            }
            _ => None,
        },
        "decimal" => match v {
            V::Bool(b) => Some(V::Num(Num::Dec(Dec::from_int(i64::from(b))))),
            V::Num(Num::Int(i)) => Some(V::Num(Num::Dec(Dec::from_int(i)))),
            V::Num(Num::IntSub(i, _)) => Some(V::Num(Num::Dec(Dec::from_int(i)))),
            V::Num(Num::Dec(d)) => Some(V::Num(Num::Dec(d))),
            V::Num(Num::Flt(d)) => d
                .is_finite()
                .then(|| Dec::parse(&format!("{d}")))?
                .map(|d| V::Num(Num::Dec(d))),
            V::Num(Num::Dbl(d)) => d
                .is_finite()
                .then(|| Dec::parse(&format!("{d}")))?
                .map(|d| V::Num(Num::Dec(d))),
            V::Str { lex, lang: None } => Dec::parse(lex.trim()).map(|d| V::Num(Num::Dec(d))),
            _ => None,
        },
        "float" | "double" => {
            let d = match v {
                V::Bool(b) => f64::from(u8::from(b)),
                V::Num(n) => n.as_f64(),
                V::Str { lex, lang: None } => match lex.trim() {
                    "INF" | "+INF" => f64::INFINITY,
                    "-INF" => f64::NEG_INFINITY,
                    "NaN" => f64::NAN,
                    t => t.parse::<f64>().ok().filter(|_| {
                        // XSD double lexicals: no rust-isms like "inf"/"1e_2".
                        t.bytes().all(|c| {
                            c.is_ascii_digit() || matches!(c, b'+' | b'-' | b'.' | b'e' | b'E')
                        })
                    })?,
                },
                _ => return None,
            };
            if local == "double" {
                Some(V::Num(Num::Dbl(d)))
            } else {
                Some(V::Num(Num::Flt(d as f32)))
            }
        }
        "dateTime" => match v {
            V::DateTime { lex, dt } if dt == graphy_core::vocab::XSD_DATE_TIME => {
                Some(V::DateTime { lex, dt })
            }
            V::Str { lex, lang: None } => {
                let t = lex.trim();
                DateTimeParts::parse(t).map(|_| V::DateTime {
                    lex: t.to_owned(),
                    dt: graphy_core::vocab::XSD_DATE_TIME.to_owned(),
                })
            }
            _ => None,
        },
        _ => None,
    }
}

/// XPath's string form uses plain notation in [1E-6, 1E6), scientific
/// notation outside it, and XML Schema spellings for non-finite values.
fn xpath_float_lexical(value: f64, declared_float: bool) -> String {
    if value.is_nan() {
        return "NaN".to_owned();
    }
    if value.is_infinite() {
        return if value.is_sign_positive() {
            "INF"
        } else {
            "-INF"
        }
        .to_owned();
    }
    if value == 0.0 {
        return if value.is_sign_negative() { "-0" } else { "0" }.to_owned();
    }
    let magnitude = if declared_float {
        f64::from((value as f32).abs())
    } else {
        value.abs()
    };
    let lower = if declared_float {
        f64::from(1e-6f32)
    } else {
        1e-6
    };
    if (lower..1e6).contains(&magnitude) {
        return if declared_float {
            (value as f32).to_string()
        } else {
            value.to_string()
        };
    }
    graphy_core::InlineValue::Double {
        value,
        declared_float,
    }
    .canonical_lexical()
}

/// The cases where the lexical and canonical xsd:dateTime forms differ
/// without changing the represented instant.
fn canonical_date_time_lexical(lex: &str) -> String {
    let mut out = if let Some(body) = lex.strip_suffix("-00:00") {
        format!("{body}Z")
    } else if let Some(body) = lex.strip_suffix("+00:00") {
        format!("{body}Z")
    } else {
        lex.to_owned()
    };
    let Some(t) = out.find("T24:00:00") else {
        return out;
    };
    let date = &out[..t];
    let (negative, date) = date.strip_prefix('-').map_or((false, date), |d| (true, d));
    let mut fields = date.split('-');
    let (Some(year), Some(month), Some(day), None) =
        (fields.next(), fields.next(), fields.next(), fields.next())
    else {
        return out;
    };
    let (Ok(mut year), Ok(mut month), Ok(mut day)) = (
        year.parse::<i64>(),
        month.parse::<u32>(),
        day.parse::<u32>(),
    ) else {
        return out;
    };
    if negative {
        year = -year;
    }
    let leap = year.rem_euclid(4) == 0 && (year.rem_euclid(100) != 0 || year.rem_euclid(400) == 0);
    let max_day = match month {
        2 if leap => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    };
    day += 1;
    if day > max_day {
        day = 1;
        month += 1;
        if month > 12 {
            month = 1;
            year += 1;
        }
    }
    let year_text = if year < 0 {
        format!("-{:04}", year.unsigned_abs())
    } else {
        format!("{year:04}")
    };
    out.replace_range(
        ..t + "T24:00:00".len(),
        &format!("{year_text}-{month:02}-{day:02}T00:00:00"),
    );
    out
}

#[cfg(test)]
mod oracle_regression_tests {
    use super::{canonical_date_time_lexical, xpath_float_lexical};

    #[test]
    fn xpath_float_strings_use_xsd_thresholds_and_special_values() {
        assert_eq!(xpath_float_lexical(f64::from(1e-6f32), true), "0.000001");
        assert_eq!(xpath_float_lexical(1e-7, false), "1.0E-7");
        assert_eq!(xpath_float_lexical(-0.0, false), "-0");
        assert_eq!(xpath_float_lexical(f64::INFINITY, false), "INF");
    }

    #[test]
    fn xpath_datetime_strings_normalize_zero_zone_and_end_of_day() {
        assert_eq!(
            canonical_date_time_lexical("30000-01-02T03:04:05-00:00"),
            "30000-01-02T03:04:05Z"
        );
        assert_eq!(
            canonical_date_time_lexical("2000-01-01T24:00:00Z"),
            "2000-01-02T00:00:00Z"
        );
    }
}

/// Parsed components of an xsd:dateTime / xsd:date lexical form.
struct DateTimeParts {
    year: i64,
    month: i64,
    day: i64,
    hours: Option<i64>,
    minutes: Option<i64>,
    seconds: Option<Dec>,
    tz: Option<Tz>,
}

#[derive(Clone, Copy)]
enum Tz {
    Zulu,
    Offset { neg: bool, h: u32, m: u32 },
}

impl Tz {
    /// `TZ()` string form (§17.4.5.7 — verbatim tz designator).
    fn tz_string(self) -> String {
        match self {
            Tz::Zulu => "Z".to_owned(),
            Tz::Offset { neg, h, m } => {
                format!("{}{h:02}:{m:02}", if neg { '-' } else { '+' })
            }
        }
    }

    /// `TIMEZONE()` as a canonical xsd:dayTimeDuration (§17.4.5.6).
    fn day_time_duration(self) -> String {
        let (neg, h, m) = match self {
            Tz::Zulu => (false, 0, 0),
            Tz::Offset { neg, h, m } => (neg, h, m),
        };
        if h == 0 && m == 0 {
            return "PT0S".to_owned();
        }
        let mut out = String::new();
        if neg {
            out.push('-');
        }
        out.push_str("PT");
        if h > 0 {
            out.push_str(&format!("{h}H"));
        }
        if m > 0 {
            out.push_str(&format!("{m}M"));
        }
        out
    }
}

impl DateTimeParts {
    fn parse(lex: &str) -> Option<DateTimeParts> {
        let (neg_year, rest) = match lex.strip_prefix('-') {
            Some(r) => (true, r),
            None => (false, lex),
        };
        // Timezone suffix.
        let (rest, tz) = if let Some(r) = rest.strip_suffix('Z') {
            (r, Some(Tz::Zulu))
        } else {
            // ±hh:mm — only if it follows a time (or date) body.
            let bytes = rest.as_bytes();
            if rest.len() > 6
                && (bytes[rest.len() - 6] == b'+' || bytes[rest.len() - 6] == b'-')
                && bytes[rest.len() - 3] == b':'
            {
                let tz_s = &rest[rest.len() - 6..];
                let neg = tz_s.starts_with('-');
                let h: u32 = tz_s[1..3].parse().ok()?;
                let m: u32 = tz_s[4..6].parse().ok()?;
                (&rest[..rest.len() - 6], Some(Tz::Offset { neg, h, m }))
            } else {
                (rest, None)
            }
        };
        let (date, time) = match rest.split_once('T') {
            Some((d, t)) => (d, Some(t)),
            None => (rest, None),
        };
        let mut dp = date.split('-');
        let year: i64 = dp.next()?.parse().ok()?;
        let month: i64 = dp.next()?.parse().ok()?;
        let day: i64 = dp.next()?.parse().ok()?;
        if dp.next().is_some() || !(1..=12).contains(&month) || !(1..=31).contains(&day) {
            return None;
        }
        let (hours, minutes, seconds) = match time {
            None => (None, None, None),
            Some(t) => {
                let mut tp = t.split(':');
                let h: i64 = tp.next()?.parse().ok()?;
                let m: i64 = tp.next()?.parse().ok()?;
                let s = Dec::parse(tp.next()?)?;
                if tp.next().is_some() || !(0..=24).contains(&h) || !(0..=59).contains(&m) {
                    return None;
                }
                (Some(h), Some(m), Some(s))
            }
        };
        Some(DateTimeParts {
            year: if neg_year { -year } else { year },
            month,
            day,
            hours,
            minutes,
            seconds,
            tz,
        })
    }
}

/// Injected wall clock (docs/11 §3): milliseconds since the Unix epoch, set
/// by hosts on targets without an ambient clock (`SystemTime::now` PANICS on
/// `wasm32-unknown-unknown` — the wasm binding sets this from `Date.now()`
/// before each evaluation). An injected value also wins on native.
static WALL_CLOCK_MS: std::sync::atomic::AtomicI64 = std::sync::atomic::AtomicI64::new(-1);

/// Inject (or clear, with `None`) the wall clock used by `NOW()` and the
/// per-evaluation rng seed.
pub fn set_wall_clock_millis(ms: Option<u64>) {
    WALL_CLOCK_MS.store(
        ms.map_or(-1, |v| v.min(i64::MAX as u64) as i64),
        std::sync::atomic::Ordering::Relaxed,
    );
}

/// The injected clock, else the ambient one; 0 on targets with neither
/// (deterministic, but never a panic mid-query).
fn wall_clock_millis() -> u64 {
    let injected = WALL_CLOCK_MS.load(std::sync::atomic::Ordering::Relaxed);
    if injected >= 0 {
        return injected as u64;
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0)
    }
    #[cfg(target_arch = "wasm32")]
    {
        0
    }
}

/// Current instant as a canonical UTC xsd:dateTime lexical form.
fn now_lexical() -> String {
    let secs = (wall_clock_millis() / 1000) as i64;
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    // Howard Hinnant's civil-from-days.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    )
}

/// Re-encode a decoded component term (triple-term accessors).
fn write_term_ref(out: &mut Vec<u8>, t: &graphy_core::TermRef<'_>) {
    use graphy_core::concise;
    match t {
        graphy_core::TermRef::Iri(i) => concise::encode_iri(out, i),
        graphy_core::TermRef::BlankNode(l) => concise::encode_blank(out, l),
        graphy_core::TermRef::Literal(l) => {
            if let Some((tag, dir)) = l.lang() {
                concise::encode_lang(out, l.lexical(), tag, dir);
            } else if l.datatype() == graphy_core::vocab::XSD_STRING {
                concise::encode_simple(out, l.lexical());
            } else {
                concise::encode_datatype(out, l.lexical(), l.datatype());
            }
        }
        graphy_core::TermRef::TripleTerm(view) => {
            let mut s = Vec::new();
            write_term_ref(&mut s, &view.subject());
            let mut p = Vec::new();
            write_term_ref(&mut p, &view.predicate());
            let mut o = Vec::new();
            write_term_ref(&mut o, &view.object());
            concise::encode_triple_term(out, &s, &p, &o);
        }
    }
}

/// Whether concise bytes contain a blank node, directly or in a nested
/// triple term. Used to preserve evaluator-local blank provenance while
/// computed RDF-star terms and accessors are interned.
fn concise_has_bnode(bytes: &[u8]) -> bool {
    fn term_has_bnode(term: &graphy_core::TermRef<'_>) -> bool {
        match term {
            graphy_core::TermRef::BlankNode(_) => true,
            graphy_core::TermRef::TripleTerm(view) => {
                term_has_bnode(&view.subject()) || term_has_bnode(&view.object())
            }
            _ => false,
        }
    }

    graphy_core::concise::decode(bytes).is_ok_and(|term| term_has_bnode(&term))
}

/// Bind a path endpoint variable (constants just sanity-match).
fn path_nullable(path: &PathExpr) -> bool {
    match path {
        PathExpr::ZeroOrMore(_) | PathExpr::ZeroOrOne(_) => true,
        PathExpr::OneOrMore(inner) | PathExpr::Inverse(inner) => path_nullable(inner),
        PathExpr::Alt(a, b) => path_nullable(a) || path_nullable(b),
        PathExpr::Seq(a, b) => path_nullable(a) && path_nullable(b),
        PathExpr::Link(_) | PathExpr::Nps(_) => false,
    }
}

pub(crate) fn bind_endpoint(p: &P, id: B, row: &mut Row) -> bool {
    match p {
        P::Var(v) => match row[v.0 as usize] {
            Some(existing) => existing == id,
            None => {
                row[v.0 as usize] = Some(id);
                true
            }
        },
        P::Term(_) => true,
        P::Triple(_) => false,
    }
}

/// Compatible-merge of two rows (§18.3): equal on shared bound vars.
pub(crate) fn merge(a: &Row, b: &Row) -> Option<Row> {
    let mut out = a.clone();
    for (i, cell) in b.iter().enumerate() {
        match (out[i], cell) {
            (Some(x), Some(y)) if x != *y => return None,
            (None, Some(y)) => out[i] = Some(*y),
            _ => {}
        }
    }
    Some(out)
}

/// A constant's column value in a position (`None` = absent → empty).
pub(crate) fn pattern_col(snap: &Snapshot, bytes: &[u8], pos: TermPos) -> Option<u64> {
    let id = snap.resolve(bytes, pos)?;
    snap.column(id, pos)
}

/// Build a storage [`Pattern`] for a triple pattern under optional row
/// bindings (free function — shared with the morsel workers, which run
/// without an `Evaluator`).
pub(crate) fn pattern_of_in(
    snap: &Snapshot,
    t: &TriplePat,
    scope: &Scope,
    row: Option<&Row>,
) -> Result<Option<Pattern>, EngineError> {
    let mut pat = Pattern::default();
    let set = |p: &P, pos: TermPos, slot: &mut Option<u64>| -> Result<bool, EngineError> {
        match p {
            P::Term(bytes) => match pattern_col(snap, bytes, pos) {
                Some(col) => {
                    *slot = Some(col);
                    Ok(true)
                }
                None => Ok(false),
            },
            P::Var(v) => {
                if let Some(row) = row {
                    match row[v.0 as usize] {
                        Some(B::Id(id)) => match snap.column(id, pos) {
                            Some(col) => {
                                *slot = Some(col);
                                return Ok(true);
                            }
                            None => return Ok(false),
                        },
                        Some(B::Ext(_)) => return Ok(false),
                        None => {}
                    }
                }
                Ok(true)
            }
            P::Triple(_) => {
                if let Some(row) = row {
                    if let Some(bytes) = bound_pattern_bytes(snap, p, row)? {
                        return match pattern_col(snap, &bytes, pos) {
                            Some(col) => {
                                *slot = Some(col);
                                Ok(true)
                            }
                            None => Ok(false),
                        };
                    }
                }
                Ok(true)
            }
        }
    };
    let (mut s, mut p, mut o) = (None, None, None);
    if !set(&t.s, TermPos::Subject, &mut s)?
        || !set(&t.p, TermPos::Predicate, &mut p)?
        || !set(&t.o, TermPos::Object, &mut o)?
    {
        return Ok(None);
    }
    pat.s = s;
    pat.p = p;
    pat.o = o;
    pat.g = match scope {
        Scope::Default => Some(0),
        Scope::Named(col) => Some(*col),
    };
    Ok(Some(pat))
}

/// Bind-scan one pattern under `row` bindings (free function — the
/// pure-ID core shared by the reference evaluator, the vectorized bind
/// join, and the morsel workers). A USING/FROM default graph is the
/// union of its member graphs with cross-member triple dedup.
pub(crate) fn scan_rows(
    snap: &Snapshot,
    dataset: &DatasetView,
    t: &TriplePat,
    scope: &Scope,
    row: &Row,
    out: &mut Vec<Row>,
) -> Result<(), EngineError> {
    if *scope == Scope::Default {
        if let Some(cols) = &dataset.default_union {
            let mut seen: HashSet<(u64, u64, u64)> = HashSet::new();
            for col in cols {
                scan_rows_in(snap, t, &Scope::Named(*col), row, out, Some(&mut seen))?;
            }
            return Ok(());
        }
    }
    scan_rows_in(snap, t, scope, row, out, None)
}

fn scan_rows_in(
    snap: &Snapshot,
    t: &TriplePat,
    scope: &Scope,
    row: &Row,
    out: &mut Vec<Row>,
    mut union_seen: Option<&mut HashSet<(u64, u64, u64)>>,
) -> Result<(), EngineError> {
    let Some(pat) = pattern_of_in(snap, t, scope, Some(row))? else {
        return Ok(());
    };
    let structural = matches!(&t.s, P::Triple(_))
        || matches!(&t.p, P::Triple(_))
        || matches!(&t.o, P::Triple(_));
    let repeated = repeated_vars(t);
    let mut scan = snap.scan_best(&pat)?;
    let mut batch = QuadBatch::new();
    while scan.next_batch(&mut batch)? {
        for i in 0..batch.len() {
            let (s, p, o) = (batch.s[i], batch.p[i], batch.o[i]);
            if let Some(seen) = union_seen.as_deref_mut() {
                if !seen.insert((s, p, o)) {
                    continue; // triple already seen in another member
                }
            }
            let mut r = row.clone();
            let ok = if structural {
                match_pattern_col(snap, &t.s, s, TermPos::Subject, &mut r)?
                    && match_pattern_col(snap, &t.p, p, TermPos::Predicate, &mut r)?
                    && match_pattern_col(snap, &t.o, o, TermPos::Object, &mut r)?
            } else {
                let bind = |pv: &P, col: u64, pos: TermPos, row: &mut Row| {
                    if let P::Var(v) = pv {
                        if row[v.0 as usize].is_none() {
                            row[v.0 as usize] = Some(B::Id(snap.term_id(col, pos)));
                        }
                    }
                };
                bind(&t.s, s, TermPos::Subject, &mut r);
                bind(&t.p, p, TermPos::Predicate, &mut r);
                bind(&t.o, o, TermPos::Object, &mut r);
                !repeated || consistent_in(snap, t, &r, s, p, o)
            };
            if ok {
                out.push(r);
            }
        }
    }
    Ok(())
}

/// Materialize a pattern component only when every variable it contains is
/// bound to a store term. `None` means the storage scan must leave that
/// column unconstrained and perform recursive matching afterwards.
fn bound_pattern_bytes(snap: &Snapshot, p: &P, row: &Row) -> Result<Option<Vec<u8>>, EngineError> {
    match p {
        P::Term(bytes) => Ok(Some(bytes.clone())),
        P::Var(v) => match row[v.0 as usize] {
            Some(B::Id(id)) => Ok(Some(snap.decode(id)?)),
            Some(B::Ext(_)) | None => Ok(None),
        },
        P::Triple(t) => {
            let (Some(s), Some(p), Some(o)) = (
                bound_pattern_bytes(snap, &t.s, row)?,
                bound_pattern_bytes(snap, &t.p, row)?,
                bound_pattern_bytes(snap, &t.o, row)?,
            ) else {
                return Ok(None);
            };
            let mut out = Vec::new();
            graphy_core::concise::encode_triple_term(&mut out, &s, &p, &o);
            Ok(Some(out))
        }
    }
}

pub(crate) fn repeated_vars(t: &TriplePat) -> bool {
    let mut vars = Vec::new();
    for p in [&t.s, &t.p, &t.o] {
        if let P::Var(v) = p {
            vars.push(*v);
        }
    }
    vars.sort();
    vars.windows(2).any(|w| w[0] == w[1])
}

pub(crate) fn consistent_in(
    snap: &Snapshot,
    t: &TriplePat,
    row: &Row,
    s: u64,
    p: u64,
    o: u64,
) -> bool {
    let check = |pat: &P, col: u64, pos: TermPos| match pat {
        P::Var(v) => match row[v.0 as usize] {
            Some(B::Id(id)) => snap.term_id(col, pos) == id,
            Some(B::Ext(_)) => false,
            None => true,
        },
        P::Term(_) | P::Triple(_) => true,
    };
    check(&t.s, s, TermPos::Subject)
        && check(&t.p, p, TermPos::Predicate)
        && check(&t.o, o, TermPos::Object)
}

fn binding_for_bytes(snap: &Snapshot, bytes: &[u8]) -> Option<B> {
    [
        TermPos::Object,
        TermPos::Subject,
        TermPos::Predicate,
        TermPos::Graph,
    ]
    .into_iter()
    .find_map(|pos| snap.resolve(bytes, pos))
    .map(B::Id)
}

fn match_pattern_ref(
    snap: &Snapshot,
    p: &P,
    term: graphy_core::TermRef<'_>,
    row: &mut Row,
) -> Result<bool, EngineError> {
    let mut bytes = Vec::new();
    write_term_ref(&mut bytes, &term);
    match p {
        P::Term(want) => Ok(*want == bytes),
        P::Var(v) => match row[v.0 as usize] {
            Some(B::Id(id)) => Ok(snap.decode(id)? == bytes),
            Some(B::Ext(_)) => Ok(false),
            None => {
                let Some(id) = binding_for_bytes(snap, &bytes) else {
                    return Ok(false);
                };
                row[v.0 as usize] = Some(id);
                Ok(true)
            }
        },
        P::Triple(t) => {
            let graphy_core::TermRef::TripleTerm(tt) = term else {
                return Ok(false);
            };
            Ok(match_pattern_ref(snap, &t.s, tt.subject(), row)?
                && match_pattern_ref(snap, &t.p, tt.predicate(), row)?
                && match_pattern_ref(snap, &t.o, tt.object(), row)?)
        }
    }
}

/// Recursively match one storage column against an RDF 1.2 triple-term
/// pattern, binding variables found at any nesting level.
pub(crate) fn match_pattern_col(
    snap: &Snapshot,
    p: &P,
    col: u64,
    pos: TermPos,
    row: &mut Row,
) -> Result<bool, EngineError> {
    match p {
        // Constants were already resolved into the storage pattern.
        P::Term(_) => Ok(true),
        P::Var(v) => match row[v.0 as usize] {
            // An external binding is a term absent from this snapshot, so it
            // cannot match a scanned column.
            Some(B::Ext(_)) => Ok(false),
            Some(B::Id(id)) => Ok(snap.term_id(col, pos) == id),
            None => {
                row[v.0 as usize] = Some(B::Id(snap.term_id(col, pos)));
                Ok(true)
            }
        },
        // Only structural triple-term patterns require materialization and
        // recursive component matching. Keep the ordinary triple-pattern
        // hot path entirely in ID space.
        P::Triple(_) => {
            let bytes = snap.decode_value(col, pos)?;
            let term = graphy_core::concise::decode(&bytes)
                .map_err(|e| EngineError(format!("invalid stored concise term: {e}")))?;
            match_pattern_ref(snap, p, term, row)
        }
    }
}
