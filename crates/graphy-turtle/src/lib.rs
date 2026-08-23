//! Streaming read/write of the RDF text formats (doc 03): N-Triples,
//! N-Quads, Turtle, TriG — RDF 1.1 grammars plus the RDF 1.2 additions
//! (triple terms `<<( … )>>`, reified triples `<< … >>`, reifiers `~`,
//! annotation blocks `{| … |}`, directional language tags `@en--ltr`).
//!
//! Sans-io core: push bytes in with `feed`, pull quads out with `drain`,
//! close with `finish`. Any chunk boundary is legal; the only buffered input
//! is the bytes of the token in flight. Terms are emitted in **concise form**
//! (`graphy-core`) so downstream consumers intern without re-serialization.

pub(crate) mod common;
mod error;
pub mod highlight;
pub(crate) mod lexer;
mod nx;
pub mod par;
mod quad;
pub(crate) mod tables;
mod turtle;
pub(crate) mod unescape;
mod writer;

pub use error::{Error, ParseError};
pub use highlight::{tokenize as highlight_tokens, HlKind, HlToken};
pub use nx::{NQuadsParser, NTriplesParser};
pub use quad::{Options, QuadRef, Shorthand};
pub use turtle::{TriGParser, TurtleParser};
pub use writer::{write_term, NQuadsWriter, TurtleWriter};

/// Base direction re-export for language-tagged strings (RDF 1.2).
pub use graphy_core::Dir;
