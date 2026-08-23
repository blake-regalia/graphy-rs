//! SPARQL algebra (doc 04 §3–4): the spec's AST → algebra translation
//! (§18.2) with interned variables and concise constant terms, plus an
//! SSE-style serializer for golden tests and EXPLAIN. The `Algebra` tree
//! is the contract consumed by the query engine (doc 05).

pub mod algebra;
pub mod rewrite;
pub mod sse;
pub mod translate;

pub use algebra::{AggregateExpr, Algebra, Expression, PathExpr, TriplePat, VarId, VarTable, P};
pub use rewrite::{
    canonicalize, push_filters, rewrite, simplify, transform_bottom_up, well_designed,
};
pub use sse::to_sse;
pub use translate::{
    translate_query, translate_update, Form, GraphTargetT, GroundQuad, QuadPat, TranslateError,
    TranslatedQuery, TranslatedUpdate, UpdateOpT,
};
