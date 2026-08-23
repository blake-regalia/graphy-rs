//! Succinct data structure primitives for graphy-rs (docs 02, plan M0):
//! rank/select bit vectors, packed integer sequences, plain-front-coded
//! string dictionaries, a wavelet matrix, and disk-backed external sorting.
//!
//! Safe Rust except for the owner-backed views in [`mem`] (the zero-copy
//! storage seam, M3); the CI Miri job keeps all of it honest.

pub mod bitvec;
pub mod extsort;
pub mod intvec;
pub mod mem;
pub mod pfc;
pub mod serial;
pub mod wavelet;

pub(crate) mod varint;

pub use bitvec::{BitVector, BitVectorBuilder};
pub use extsort::{ExtSorter, Record};
pub use intvec::{AlignedInts, PackedInts};
pub use mem::{Bytes, Cursor, Words};
pub use pfc::{Pfc, PfcBuilder};
pub use wavelet::WaveletMatrix;
