//! Core RDF term model for graphy-rs.
//!
//! This crate defines the vocabulary every other graphy crate speaks
//! (design: `graphy-rs/docs/01-data-model.md`):
//!
//! - the **concise term encoding** ([`concise`]): a type-sigil-first,
//!   delimiter-free byte encoding of RDF terms adapted from graphy.js "c1"
//!   terms, whose plain byte order is the total order *(term kind, value)*;
//! - borrowed and owned term types ([`TermRef`], [`Term`]) that are thin
//!   views over / owners of concise bytes;
//! - the tagged 64-bit [`TermId`] with inlined values for numerics, booleans,
//!   dateTimes, and (feature-gated) short strings;
//! - IRI validation and RFC 3986/3987 reference resolution ([`iri`]).
//!
//! No I/O, no storage, no SPARQL — those live upstack.

pub mod concise;
mod error;
pub mod id;
pub mod iri;
mod term;
pub mod text;
pub mod vocab;

pub use error::TermError;
pub use id::{InlineValue, Section, Tag, TermId};
pub use term::{Dir, GraphName, LiteralParts, Quad, Term, TermRef, Triple, TripleTermRef};

/// Crate-wide result alias.
pub type Result<T, E = TermError> = std::result::Result<T, E>;

pub(crate) mod varint {
    /// LEB128 unsigned varint, used only inside concise triple-term payloads.
    pub fn write(buf: &mut Vec<u8>, mut v: u64) {
        loop {
            let byte = (v & 0x7f) as u8;
            v >>= 7;
            if v == 0 {
                buf.push(byte);
                return;
            }
            buf.push(byte | 0x80);
        }
    }

    /// Returns (value, bytes consumed), or None on truncation/overflow.
    pub fn read(buf: &[u8]) -> Option<(u64, usize)> {
        let mut v: u64 = 0;
        for (i, &b) in buf.iter().enumerate().take(10) {
            v |= u64::from(b & 0x7f) << (7 * i);
            if b & 0x80 == 0 {
                // Reject non-minimal 10th byte overflowing 64 bits.
                if i == 9 && b > 1 {
                    return None;
                }
                return Some((v, i + 1));
            }
        }
        None
    }
}
