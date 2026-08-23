//! §M13d: variable→term substitution over the AST (parameterized
//! queries). Injection-safe by construction — values are [`Term`]s
//! placed into the tree, never text spliced into a string — and
//! scope-safe by validation: binding a variable that the query *declares*
//! (projects, aliases, `BIND`s, or lists in `VALUES`) is an error, as is
//! a value that cannot occupy the variable's position (a non-IRI as a
//! predicate or graph/service name, a blank node anywhere — pattern
//! blank nodes are existentials, so substituting one would change
//! semantics silently).

use std::collections::HashMap;

use crate::ast::{
    Expr, ExprKind, GroupCondition, GroupElement, GroupPattern, Projection, Query, QueryForm,
    SelectClause, SolutionModifiers, Term, TermKind, TriplePattern, UpdateOp, UpdateRequest,
    ValuesBlock, Verb,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SubstError {
    /// The variable is declared by the query (projection, `AS` alias,
    /// `BIND`, `GROUP BY` alias, or a `VALUES` column) — substituting it
    /// would leave ungrammatical or semantics-shifting text.
    DeclaredVar(String),
    /// The value is a blank node (pattern blank nodes are existentials).
    BlankValue(String),
    /// The variable stands in predicate position but the value is not an
    /// IRI.
    InvalidPredicate(String),
    /// The variable names a graph or service target but the value is not
    /// an IRI.
    InvalidGraphName(String),
}

impl std::fmt::Display for SubstError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SubstError::DeclaredVar(v) => write!(f, "variable ?{v} is declared by the query"),
            SubstError::BlankValue(v) => write!(f, "value for ?{v} is a blank node"),
            SubstError::InvalidPredicate(v) => {
                write!(f, "value for predicate-position ?{v} is not an IRI")
            }
            SubstError::InvalidGraphName(v) => {
                write!(f, "value for graph/service-position ?{v} is not an IRI")
            }
        }
    }
}

impl std::error::Error for SubstError {}

struct Subst<'a> {
    bindings: HashMap<&'a str, &'a Term>,
}

type R = Result<(), SubstError>;

/// Substitute `bindings` (variable name without sigil → value) throughout
/// a query, returning the rewritten copy.
pub fn substitute_query(q: &Query, bindings: &[(String, Term)]) -> Result<Query, SubstError> {
    let s = Subst::new(bindings)?;
    let mut q = q.clone();
    match &mut q.form {
        QueryForm::Select(sc) => s.select_clause(sc)?,
        QueryForm::Construct(template) => {
            for t in template {
                s.triple(t)?;
            }
        }
        QueryForm::Describe { targets, .. } => {
            for t in targets {
                s.term(t)?;
            }
        }
        QueryForm::Ask => {}
    }
    s.group(&mut q.pattern)?;
    s.modifiers(&mut q.modifiers)?;
    if let Some(vb) = &mut q.values {
        s.values(vb)?;
    }
    Ok(q)
}

/// Substitute `bindings` throughout an update request.
pub fn substitute_update(
    u: &UpdateRequest,
    bindings: &[(String, Term)],
) -> Result<UpdateRequest, SubstError> {
    let s = Subst::new(bindings)?;
    let mut u = u.clone();
    for op in &mut u.operations {
        match op {
            UpdateOp::InsertData(quads)
            | UpdateOp::DeleteData(quads)
            | UpdateOp::DeleteWhere(quads) => {
                for q in quads {
                    if let Some(g) = &mut q.graph {
                        s.graph_term(g)?;
                    }
                    s.triple(&mut q.triple)?;
                }
            }
            UpdateOp::Modify {
                delete,
                insert,
                pattern,
                ..
            } => {
                for quads in [delete, insert].into_iter().flatten() {
                    for q in quads.iter_mut() {
                        if let Some(g) = &mut q.graph {
                            s.graph_term(g)?;
                        }
                        s.triple(&mut q.triple)?;
                    }
                }
                s.group(pattern)?;
            }
            _ => {}
        }
    }
    Ok(u)
}

impl<'a> Subst<'a> {
    fn new(bindings: &'a [(String, Term)]) -> Result<Subst<'a>, SubstError> {
        let mut map = HashMap::new();
        for (name, value) in bindings {
            if matches!(value.kind, TermKind::BlankNode(_)) {
                return Err(SubstError::BlankValue(name.clone()));
            }
            map.insert(name.as_str(), value);
        }
        Ok(Subst { bindings: map })
    }

    fn declared(&self, name: &str) -> R {
        if self.bindings.contains_key(name) {
            return Err(SubstError::DeclaredVar(name.to_owned()));
        }
        Ok(())
    }

    /// A term in subject/object position: any non-blank value fits.
    fn term(&self, t: &mut Term) -> R {
        match &mut t.kind {
            TermKind::Var(name) => {
                if let Some(value) = self.bindings.get(name.as_str()) {
                    t.kind = value.kind.clone();
                }
                Ok(())
            }
            TermKind::TripleTerm(tp) => self.triple(tp),
            _ => Ok(()),
        }
    }

    /// A graph name / service target: the value must be an IRI.
    fn graph_term(&self, t: &mut Term) -> R {
        if let TermKind::Var(name) = &t.kind {
            if let Some(value) = self.bindings.get(name.as_str()) {
                if !matches!(value.kind, TermKind::Iri(_)) {
                    return Err(SubstError::InvalidGraphName(name.clone()));
                }
                t.kind = value.kind.clone();
            }
        }
        Ok(())
    }

    fn triple(&self, tp: &mut TriplePattern) -> R {
        self.term(&mut tp.s)?;
        if let Verb::Term(v) = &mut tp.p {
            if let TermKind::Var(name) = &v.kind {
                if let Some(value) = self.bindings.get(name.as_str()) {
                    if !matches!(value.kind, TermKind::Iri(_)) {
                        return Err(SubstError::InvalidPredicate(name.clone()));
                    }
                    v.kind = value.kind.clone();
                }
            }
        }
        self.term(&mut tp.o)
    }

    fn group(&self, g: &mut GroupPattern) -> R {
        for el in &mut g.elements {
            match el {
                GroupElement::Triples(ts) => {
                    for t in ts {
                        self.triple(t)?;
                    }
                }
                GroupElement::Filter(e) => self.expr(e)?,
                GroupElement::Optional(g) | GroupElement::Minus(g) => self.group(g)?,
                GroupElement::Union(gs) => {
                    for g in gs {
                        self.group(g)?;
                    }
                }
                GroupElement::Graph(t, g) => {
                    self.graph_term(t)?;
                    self.group(g)?;
                }
                GroupElement::Service {
                    target, pattern, ..
                } => {
                    self.graph_term(target)?;
                    self.group(pattern)?;
                }
                GroupElement::Bind { expr, var, .. } => {
                    self.declared(var)?;
                    self.expr(expr)?;
                }
                GroupElement::Values(vb) => self.values(vb)?,
                GroupElement::SubSelect(ss) => {
                    self.select_clause(&mut ss.select)?;
                    self.group(&mut ss.pattern)?;
                    self.modifiers(&mut ss.modifiers)?;
                    if let Some(vb) = &mut ss.values {
                        self.values(vb)?;
                    }
                }
            }
        }
        Ok(())
    }

    fn select_clause(&self, sc: &mut SelectClause) -> R {
        for p in &mut sc.projection {
            match p {
                Projection::Var(v) => self.declared(v)?,
                Projection::Expr(e, v) => {
                    self.declared(v)?;
                    self.expr(e)?;
                }
            }
        }
        Ok(())
    }

    fn modifiers(&self, m: &mut SolutionModifiers) -> R {
        for c in &mut m.group_by {
            match c {
                GroupCondition::Var(v) => self.declared(v)?,
                GroupCondition::Expr(e, alias) => {
                    if let Some(v) = alias {
                        self.declared(v)?;
                    }
                    self.expr(e)?;
                }
            }
        }
        for e in &mut m.having {
            self.expr(e)?;
        }
        for c in &mut m.order_by {
            self.expr(&mut c.expr)?;
        }
        Ok(())
    }

    fn values(&self, vb: &mut ValuesBlock) -> R {
        for v in &vb.vars {
            self.declared(v)?;
        }
        Ok(())
    }

    fn expr(&self, e: &mut Expr) -> R {
        match &mut *e.kind {
            ExprKind::Or(a, b)
            | ExprKind::And(a, b)
            | ExprKind::Cmp(_, a, b)
            | ExprKind::Add(a, b)
            | ExprKind::Sub(a, b)
            | ExprKind::Mul(a, b)
            | ExprKind::Div(a, b) => {
                self.expr(a)?;
                self.expr(b)
            }
            ExprKind::In { expr, list, .. } => {
                self.expr(expr)?;
                for x in list {
                    self.expr(x)?;
                }
                Ok(())
            }
            ExprKind::Not(x) | ExprKind::UnaryMinus(x) | ExprKind::UnaryPlus(x) => self.expr(x),
            ExprKind::Builtin(_, args) | ExprKind::Function { args, .. } => {
                for x in args {
                    self.expr(x)?;
                }
                Ok(())
            }
            ExprKind::Exists(g) | ExprKind::NotExists(g) => self.group(g),
            ExprKind::Aggregate { expr, .. } => match expr {
                Some(e) => self.expr(e),
                None => Ok(()),
            },
            ExprKind::Term(t) => self.term(t),
        }
    }
}
