//! HDT file import/export (doc 03, M5): read standard hdt-cpp/hdt-java
//! files (FourSection dictionary + BitmapTriples, the only widely deployed
//! configuration) and write files those tools read back. Binary layout —
//! vbyte convention, CRC algorithms (CRC-8/0x07, CRC-16/ARC, CRC-32C),
//! section order (shared, subjects, predicates, objects), bitstream
//! packing — was pinned empirically against an hdt-cpp-produced file and
//! is documented in `codec.rs`.
//!
//! - [`HdtReader`] parses a file and streams its triples as concise terms
//!   in (s, p, o)-id order — already sorted, which is what makes HDT the
//!   cheapest bulk ingest (doc 03: no external sort; the id order maps to
//!   term-sorted order per section).
//! - [`HdtWriter`] collects a triple stream (concise terms), builds the
//!   four dictionary sections, and writes a standard triples-only HDT
//!   file. Quads: callers export the triples view (HDTQ arrives with a
//!   later increment). RDF 1.2 triple terms are not representable and
//!   error out; directional language literals use a lossless non-standard
//!   `@lang--dir` spelling.

mod codec;
mod import;
mod quads;
mod reader;
mod section;
mod term;
mod writer;

pub use import::import_segment;
pub use reader::HdtReader;
pub use writer::HdtWriter;

/// Errors from HDT parsing/serialization.
#[derive(Debug, thiserror::Error)]
pub enum HdtError {
    #[error("hdt i/o: {0}")]
    Io(#[from] std::io::Error),
    #[error("hdt format: {0}")]
    Format(String),
}
