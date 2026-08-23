//! Span-carrying AST for SPARQL Query (doc 04 §2). The parser resolves
//! prefixed names and relative IRIs against the prologue, decodes string
//! escapes, expands collections / blank-node property lists / SPARQL 1.2
//! reification sugar into plain triples, and leaves everything else in
//! syntactic shape — the §18.2 translation (graphy-algebra) consumes this
//! tree, so it mirrors the grammar rather than the algebra.

use crate::token::{Dir, Span};

/// A parsed query: prologue already applied (all IRIs absolute).
#[derive(Debug, Clone, PartialEq)]
pub struct Query {
    /// `VERSION` declaration, if present (SPARQL 1.2).
    pub version: Option<String>,
    /// Final `BASE` of the prologue (static IRI resolution already applied
    /// during the parse; kept for runtime `IRI()` resolution, §17.4.2.8).
    pub base: Option<String>,
    /// `PREFIX` declarations in source order (later shadows earlier).
    /// IRIs in the tree are already absolute — this is retained only so
    /// the printer (plan §M13) can re-compress them into prefixed names.
    pub prefixes: Vec<(String, String)>,
    pub form: QueryForm,
    pub dataset: Vec<DatasetClause>,
    /// The WHERE pattern (empty group for a template-only CONSTRUCT or a
    /// pattern-less DESCRIBE).
    pub pattern: GroupPattern,
    pub modifiers: SolutionModifiers,
    /// Trailing `VALUES` clause.
    pub values: Option<ValuesBlock>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum QueryForm {
    Select(SelectClause),
    /// The template triples (already expanded to plain triples).
    Construct(Vec<TriplePattern>),
    /// `DESCRIBE *` is the empty list with `star = true`.
    Describe {
        targets: Vec<Term>,
        star: bool,
    },
    Ask,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SelectClause {
    pub distinct: bool,
    pub reduced: bool,
    /// Empty = `SELECT *`.
    pub projection: Vec<Projection>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Projection {
    Var(String),
    /// `(expr AS ?var)`
    Expr(Expr, String),
}

#[derive(Debug, Clone, PartialEq)]
pub enum DatasetClause {
    Default(String),
    Named(String),
}

/// One `{ … }` group, in syntactic order: runs of triples interleaved
/// with the non-triples elements. Adjacent triples separated only by `.`
/// stay in one `Triples` run (one future BGP); any other element starts a
/// new run.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct GroupPattern {
    pub elements: Vec<GroupElement>,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq)]
pub enum GroupElement {
    Triples(Vec<TriplePattern>),
    Filter(Expr),
    Optional(GroupPattern),
    Minus(GroupPattern),
    /// One or more `UNION`-joined groups (a single group when no UNION).
    Union(Vec<GroupPattern>),
    Graph(Term, GroupPattern),
    Service {
        silent: bool,
        target: Term,
        pattern: GroupPattern,
    },
    Bind {
        expr: Expr,
        var: String,
        span: Span,
    },
    Values(ValuesBlock),
    SubSelect(Box<SubSelect>),
}

/// A nested `SELECT` inside a group.
#[derive(Debug, Clone, PartialEq)]
pub struct SubSelect {
    pub select: SelectClause,
    pub pattern: GroupPattern,
    pub modifiers: SolutionModifiers,
    pub values: Option<ValuesBlock>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ValuesBlock {
    pub vars: Vec<String>,
    /// Each row has exactly `vars.len()` entries; `None` = UNDEF.
    pub rows: Vec<Vec<Option<Term>>>,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct SolutionModifiers {
    pub group_by: Vec<GroupCondition>,
    pub having: Vec<Expr>,
    pub order_by: Vec<OrderCondition>,
    pub limit: Option<u64>,
    pub offset: Option<u64>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum GroupCondition {
    Var(String),
    /// `(expr)` or `(expr AS ?var)`; bare builtin/function calls land
    /// here with `alias: None`.
    Expr(Expr, Option<String>),
}

#[derive(Debug, Clone, PartialEq)]
pub struct OrderCondition {
    pub descending: bool,
    pub expr: Expr,
}

/// One triple pattern; collections, property lists, and 1.2 sugar are
/// already expanded, so `s`/`o` are plain terms (possibly fresh blank
/// nodes) and the predicate is a term or a property path.
#[derive(Debug, Clone, PartialEq)]
pub struct TriplePattern {
    pub s: Term,
    pub p: Verb,
    pub o: Term,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Verb {
    /// IRI or variable.
    Term(Term),
    /// A non-trivial property path (a bare IRI parses as `Term`).
    Path(Path),
}

/// Property paths (translated further in the algebra; fixed-length
/// decomposition happens there, per spec).
#[derive(Debug, Clone, PartialEq)]
pub enum Path {
    Iri(String),
    Inverse(Box<Path>),
    Seq(Box<Path>, Box<Path>),
    Alt(Box<Path>, Box<Path>),
    ZeroOrMore(Box<Path>),
    OneOrMore(Box<Path>),
    ZeroOrOne(Box<Path>),
    /// Negated property set: allowed IRIs with a per-entry inverse flag.
    Nps(Vec<(String, bool)>),
}

/// A term with its source span (desugared nodes carry the span of the
/// construct that produced them).
#[derive(Debug, Clone, PartialEq)]
pub struct Term {
    pub kind: TermKind,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq)]
pub enum TermKind {
    Iri(String),
    /// User label, or a parser-fresh node (labels starting with `.` —
    /// impossible in the grammar, so collision-free).
    BlankNode(String),
    Literal {
        lexical: String,
        kind: LiteralKind,
    },
    Var(String),
    /// SPARQL 1.2 triple term `<<( s p o )>>` (object position only).
    TripleTerm(Box<TriplePattern>),
}

#[derive(Debug, Clone, PartialEq)]
pub enum LiteralKind {
    Plain,
    Lang {
        tag: String,
        dir: Option<Dir>,
    },
    /// Absolute datatype IRI.
    Typed(String),
}

/// An expression node with its source span.
#[derive(Debug, Clone, PartialEq)]
pub struct Expr {
    pub kind: Box<ExprKind>,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ExprKind {
    Or(Expr, Expr),
    And(Expr, Expr),
    Cmp(CmpOp, Expr, Expr),
    /// `expr [NOT] IN (…)`
    In {
        expr: Expr,
        list: Vec<Expr>,
        negated: bool,
    },
    Add(Expr, Expr),
    Sub(Expr, Expr),
    Mul(Expr, Expr),
    Div(Expr, Expr),
    Not(Expr),
    UnaryMinus(Expr),
    UnaryPlus(Expr),
    /// Builtin call (§17.4 + SPARQL 1.2), by keyword.
    Builtin(Builtin, Vec<Expr>),
    /// `REGEX`-family flags and `GROUP_CONCAT` separators ride the
    /// argument vector; `BOUND`'s variable is a `Term(Var)` argument.
    /// Custom function by absolute IRI.
    Function {
        iri: String,
        args: Vec<Expr>,
        distinct: bool,
    },
    Exists(GroupPattern),
    NotExists(GroupPattern),
    Aggregate {
        func: Aggregate,
        distinct: bool,
        /// `None` = `COUNT(*)`.
        expr: Option<Expr>,
        /// `GROUP_CONCAT(…; SEPARATOR = "…")`, decoded.
        separator: Option<String>,
    },
    Term(Term),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CmpOp {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Aggregate {
    Count,
    Sum,
    Min,
    Max,
    Avg,
    Sample,
    GroupConcat,
}

macro_rules! builtins {
    ($($variant:ident),+ $(,)?) => {
        /// Non-aggregate builtin functions callable in expressions.
        #[derive(Debug, Clone, Copy, PartialEq, Eq)]
        pub enum Builtin {
            $($variant),+
        }
    };
}

builtins! {
    Str, Lang, LangMatches, Datatype, Bound, Iri, BNode, Rand, Abs, Ceil,
    Floor, Round, Concat, StrLen, UCase, LCase, EncodeForUri, Contains,
    StrStarts, StrEnds, StrBefore, StrAfter, Year, Month, Day, Hours,
    Minutes, Seconds, Timezone, Tz, Now, Uuid, StrUuid, Md5, Sha1, Sha256,
    Sha384, Sha512, Coalesce, If, StrLang, StrDt, SameTerm, IsIri, IsBlank,
    IsLiteral, IsNumeric, Regex, Substr, Replace,
    // SPARQL 1.2.
    Triple, Subject, Predicate, Object, IsTriple, LangDir, HasLang,
    HasLangDir, StrLangDir,
}

// ---------------------------------------------------------------------------
// Update (SPARQL 1.1 Update §3; parsed by `parse_update`).
// ---------------------------------------------------------------------------

/// A parsed update request: a `;`-separated sequence of operations
/// (each operation saw the prefixes declared before it).
#[derive(Debug, Clone, PartialEq)]
pub struct UpdateRequest {
    /// `VERSION` declaration, if present (SPARQL 1.2).
    pub version: Option<String>,
    /// All `PREFIX` declarations across the request, in source order
    /// (accumulating, later shadows earlier) — see [`Query::prefixes`].
    pub prefixes: Vec<(String, String)>,
    pub operations: Vec<UpdateOp>,
}

/// One quad in a template: an optional named-graph wrapper around a
/// triple (`None` = the default graph / WITH target).
#[derive(Debug, Clone, PartialEq)]
pub struct Quad {
    pub graph: Option<Term>,
    pub triple: TriplePattern,
}

#[derive(Debug, Clone, PartialEq)]
pub enum UpdateOp {
    /// Ground quads (blank nodes allowed — they mint fresh nodes).
    InsertData(Vec<Quad>),
    /// Ground quads, no blank nodes.
    DeleteData(Vec<Quad>),
    /// Pattern doubles as template; no blank nodes.
    DeleteWhere(Vec<Quad>),
    Modify {
        /// `WITH <g>`.
        with: Option<String>,
        /// `DELETE { … }` template (no blank nodes).
        delete: Option<Vec<Quad>>,
        /// `INSERT { … }` template.
        insert: Option<Vec<Quad>>,
        using: Vec<DatasetClause>,
        pattern: GroupPattern,
    },
    Load {
        silent: bool,
        source: String,
        into: Option<String>,
    },
    Clear {
        silent: bool,
        target: GraphTarget,
    },
    Drop {
        silent: bool,
        target: GraphTarget,
    },
    Create {
        silent: bool,
        graph: String,
    },
    Add {
        silent: bool,
        from: GraphOrDefault,
        to: GraphOrDefault,
    },
    Move {
        silent: bool,
        from: GraphOrDefault,
        to: GraphOrDefault,
    },
    Copy {
        silent: bool,
        from: GraphOrDefault,
        to: GraphOrDefault,
    },
}

/// `GraphRefAll`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GraphTarget {
    Graph(String),
    Default,
    Named,
    All,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GraphOrDefault {
    Default,
    Graph(String),
}
