//! The SPARQL algebra (doc 04 §3): a plain tree with interned variables
//! and constant terms in concise form — the contract between the parsing
//! stack and the query engine (doc 05). Produced by [`crate::translate`];
//! printed by [`crate::sse`] for golden tests and EXPLAIN.

use std::collections::HashMap;

/// An interned variable. User variables keep their names; internal
/// variables (path decomposition, aggregate extraction) get `.`-prefixed
/// names the grammar cannot produce.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct VarId(pub u32);

/// Variable name table for one translated query.
#[derive(Debug, Default, Clone)]
pub struct VarTable {
    names: Vec<String>,
    ids: HashMap<String, VarId>,
    fresh: u32,
}

impl VarTable {
    pub fn intern(&mut self, name: &str) -> VarId {
        if let Some(&id) = self.ids.get(name) {
            return id;
        }
        let id = VarId(self.names.len() as u32);
        self.names.push(name.to_owned());
        self.ids.insert(name.to_owned(), id);
        id
    }

    /// A fresh internal variable (never collides with user names).
    pub fn fresh(&mut self, what: &str) -> VarId {
        let name = format!(".{what}{}", self.fresh);
        self.fresh += 1;
        self.intern(&name)
    }

    pub fn name(&self, id: VarId) -> &str {
        &self.names[id.0 as usize]
    }

    pub fn get(&self, name: &str) -> Option<VarId> {
        self.ids.get(name).copied()
    }

    pub fn len(&self) -> usize {
        self.names.len()
    }

    pub fn is_empty(&self) -> bool {
        self.names.is_empty()
    }
}

/// A pattern position: a constant term (concise bytes) or a variable.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum P {
    /// Concise-encoded constant term (doc 01 §3).
    Term(Vec<u8>),
    Var(VarId),
    /// A triple term with variable components (all-ground triple terms
    /// encode as `Term` bytes).
    Triple(Box<TriplePat>),
}

/// One triple pattern with final (link-only) predicates.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TriplePat {
    pub s: P,
    pub p: P,
    pub o: P,
}

/// Irreducible property-path expressions (fixed-length forms decompose
/// into BGPs during translation, per §18.2.2.4).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PathExpr {
    Link(Vec<u8>),
    Inverse(Box<PathExpr>),
    Seq(Box<PathExpr>, Box<PathExpr>),
    Alt(Box<PathExpr>, Box<PathExpr>),
    ZeroOrMore(Box<PathExpr>),
    OneOrMore(Box<PathExpr>),
    ZeroOrOne(Box<PathExpr>),
    /// Negated property set: (concise IRI, inverted) members.
    Nps(Vec<(Vec<u8>, bool)>),
}

/// The algebra tree. `Bgp(vec![])` is the unit table (the spec's Z).
#[derive(Debug, Clone, PartialEq)]
pub enum Algebra {
    Bgp(Vec<TriplePat>),
    /// An irreducible path step (§18.2.2.4 leaves only `*`,`+`,`?`, NPS,
    /// and alternatives/sequences containing them).
    Path {
        s: P,
        path: PathExpr,
        o: P,
    },
    Join(Box<Algebra>, Box<Algebra>),
    /// OPTIONAL (filter fused per §18.2.2.6; `None` = unconditional).
    LeftJoin {
        left: Box<Algebra>,
        right: Box<Algebra>,
        expr: Option<Expression>,
    },
    Filter {
        expr: Expression,
        input: Box<Algebra>,
    },
    Union(Box<Algebra>, Box<Algebra>),
    Graph {
        graph: P,
        input: Box<Algebra>,
    },
    Service {
        silent: bool,
        target: P,
        input: Box<Algebra>,
    },
    /// BIND / projection expression.
    Extend {
        input: Box<Algebra>,
        var: VarId,
        expr: Expression,
    },
    Minus(Box<Algebra>, Box<Algebra>),
    /// Inline data (VALUES); `None` = UNDEF.
    Table {
        vars: Vec<VarId>,
        rows: Vec<Vec<Option<Vec<u8>>>>,
    },
    /// Subquery boundary.
    ToMultiSet(Box<Algebra>),
    /// Grouping + aggregation (§18.2.4): keys in order (alias, expr —
    /// plain key variables have `expr = None` and alias = the variable),
    /// aggregate bindings computed per group.
    Group {
        keys: Vec<(VarId, Option<Expression>)>,
        aggregates: Vec<(VarId, AggregateExpr)>,
        input: Box<Algebra>,
    },
    OrderBy {
        input: Box<Algebra>,
        /// (expression, descending) in priority order.
        conditions: Vec<(Expression, bool)>,
    },
    Project {
        input: Box<Algebra>,
        vars: Vec<VarId>,
    },
    Distinct(Box<Algebra>),
    Reduced(Box<Algebra>),
    Slice {
        input: Box<Algebra>,
        offset: u64,
        limit: Option<u64>,
    },
}

/// Scalar expressions after translation: variables interned, constants
/// concise, aggregate calls replaced by references to the internal
/// variables bound by the enclosing [`Algebra::Group`].
#[derive(Debug, Clone, PartialEq)]
pub enum Expression {
    Term(Vec<u8>),
    Var(VarId),
    Or(Box<Expression>, Box<Expression>),
    And(Box<Expression>, Box<Expression>),
    Cmp(CmpOp, Box<Expression>, Box<Expression>),
    In {
        expr: Box<Expression>,
        list: Vec<Expression>,
        negated: bool,
    },
    Add(Box<Expression>, Box<Expression>),
    Sub(Box<Expression>, Box<Expression>),
    Mul(Box<Expression>, Box<Expression>),
    Div(Box<Expression>, Box<Expression>),
    Not(Box<Expression>),
    UnaryMinus(Box<Expression>),
    UnaryPlus(Box<Expression>),
    Builtin(Builtin, Vec<Expression>),
    Function {
        /// Concise IRI.
        iri: Vec<u8>,
        args: Vec<Expression>,
        distinct: bool,
    },
    /// EXISTS / NOT EXISTS carrying a translated subtree (substitute
    /// semantics at evaluation time).
    Exists {
        negated: bool,
        pattern: Box<Algebra>,
    },
    /// A triple term with variable components (SPARQL 1.2 expressions).
    TripleTerm {
        s: Box<Expression>,
        p: Box<Expression>,
        o: Box<Expression>,
    },
}

/// One extracted aggregate call.
#[derive(Debug, Clone, PartialEq)]
pub struct AggregateExpr {
    pub func: Aggregate,
    pub distinct: bool,
    /// `None` = `COUNT(*)`.
    pub expr: Option<Expression>,
    pub separator: Option<String>,
}

pub use graphy_sparql_syntax::ast::{Aggregate, Builtin, CmpOp};
