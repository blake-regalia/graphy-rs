//! Language server for the RDF text formats (N-Triples, N-Quads, Turtle,
//! TriG, JSON-LD) and SPARQL — design in `docs/10-lsp.md`.
//!
//! This is the protocol-agnostic core: the resilient-tokenizer → semantic-token
//! pipeline and the UTF-16 document coordinate model, all testable without a
//! transport. The JSON-RPC server loop (`lsp-server` + `lsp-types`, stdio) and
//! the incremental document store (`ropey`) land in a later increment on top of
//! this core (docs/10 §4, §14 M11a).
//!
//! ## Two tiers (docs/10 §3.4)
//! Tier 1 — semantic tokens — is driven by the resilient lexers only and never
//! consults the parser, so a syntax error localizes and never blanks the
//! highlighting. Tier 2 — diagnostics/outline from the recovering parsers —
//! arrives in M11b.

pub mod completion;
pub mod diagnostics;
pub mod document;
pub mod folding;
pub mod format;
pub mod hover;
pub mod jsonld;
pub mod legend;
pub mod line_index;
pub mod semantic;
pub mod server;
pub mod sparql;
pub mod symbols;

pub use completion::{
    jsonld_completions, sparql_completions, turtle_completions, CompKind, Completion,
    WELL_KNOWN_PREFIXES,
};
pub use diagnostics::{
    jsonld_diagnostics, sparql_diagnostics, turtle_diagnostics, Diag, Fix, FixEdit, FixKind, Sev,
};
pub use folding::{jsonld_folds, sparql_folds, turtle_folds, FoldRange};
pub use format::turtle_pretty;
pub use hover::{jsonld_hover, sparql_hover, turtle_hover, HoverInfo};
pub use jsonld::jsonld_semantic_tokens;
pub use legend::{SemKind, SemMod};
pub use line_index::LineIndex;
pub use semantic::{encode, turtle_semantic_tokens, SemToken};
pub use sparql::sparql_semantic_tokens;
pub use symbols::{jsonld_symbols, sparql_symbols, turtle_symbols, SymKind, Symbol};
