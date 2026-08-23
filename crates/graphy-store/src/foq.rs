//! FoQ wavelet accessors (`idx/foq.wm`, compact profile; doc 02 §5,
//! docs/08 §4, HDT-FoQ heritage): P-rooted and O-rooted access over the
//! lone SPO ordering.
//!
//! - `Wp` — wavelet matrix over SPO's `Sy` (the predicate of every distinct
//!   (subject, predicate) pair). `select(p, k)` walks a predicate's
//!   occurrences in subject order; each occurrence expands to its `Sz` run,
//!   giving **PSO-order** emission.
//! - `Xo`/`Bo`/`Po` — the object-position index: distinct object values
//!   ascending, first-position marks, and all SPO triple ordinals sorted by
//!   (object, ordinal), giving **OSP-order** emission and O(1) per-object
//!   triple counts.
//!
//! Both accessors yield SPO triple ordinals directly, so the graph layer
//! composes without `Pz`.

use std::io::{self, Write};

use graphy_succinct::intvec::{bits_for, PackedIntsBuilder};
use graphy_succinct::serial::write_u64;
use graphy_succinct::{BitVector, BitVectorBuilder, Cursor, PackedInts, WaveletMatrix};

use crate::bt::Bt;

#[derive(Debug)]
pub(crate) struct Foq {
    /// Wavelet matrix over SPO's `Sy` (predicate per (s, p) pair).
    pub(crate) wp: WaveletMatrix,
    /// Distinct object values, ascending.
    pub(crate) xo: PackedInts,
    /// First `Po` position of each object's run (`count_ones = xo.len()`).
    bo: BitVector,
    /// SPO triple ordinals sorted by (object value, ordinal).
    pub(crate) po: PackedInts,
}

impl Foq {
    /// Build from the finished SPO ordering and the (object, SPO ordinal)
    /// pairs sorted by (object, ordinal) — the builder streams the latter
    /// out of an external sort.
    pub fn build(
        spo: &Bt,
        sorted_obj_ordinals: impl Iterator<Item = (u64, u64)>,
        pred_width: u32,
    ) -> Foq {
        // Packed at the predicate width (inc B: tens of MB at 10⁸
        // triples instead of an 800 MB Vec<u64>).
        let sy = PackedInts::with_width((0..spo.n_y()).map(|yi| spo.y_at(yi)), pred_width);
        let wp = WaveletMatrix::from_packed(&sy);
        drop(sy);
        let n_triples = spo.n_triples();
        let mut xo = Vec::new();
        let mut bo = BitVectorBuilder::with_capacity(n_triples as usize);
        let mut po = PackedIntsBuilder::new(bits_for(n_triples.saturating_sub(1)));
        let mut last_o = None;
        for (o, ordinal) in sorted_obj_ordinals {
            let first = last_o != Some(o);
            if first {
                last_o = Some(o);
                xo.push(o);
            }
            bo.push(first);
            po.push(ordinal);
        }
        Foq {
            wp,
            xo: PackedInts::from_slice(&xo),
            bo: bo.build(),
            po: po.build(),
        }
    }

    /// `[Wp][n_obj u64][Xo][Bo][Po]`
    pub fn serialize_into<W: Write>(&self, w: &mut W) -> io::Result<()> {
        self.wp.serialize_into(w)?;
        write_u64(w, self.xo.len() as u64)?;
        self.xo.serialize_into(w)?;
        self.bo.serialize_into(w)?;
        self.po.serialize_into(w)
    }

    /// Deserialize (zero-copy views) with internal-consistency validation;
    /// shape checks against the SPO ordering happen at segment open.
    pub fn deserialize(c: &mut Cursor) -> io::Result<Foq> {
        let bad = |m: &str| io::Error::new(io::ErrorKind::InvalidData, format!("foq: {m}"));
        let wp = WaveletMatrix::deserialize_view(c)?;
        let n_obj = c.read_u64()?;
        let xo = PackedInts::deserialize_view(c)?;
        if xo.len() as u64 != n_obj {
            return Err(bad("object count mismatch"));
        }
        for i in 1..xo.len() {
            if xo.get(i - 1) >= xo.get(i) {
                return Err(bad("object values not strictly increasing"));
            }
        }
        let bo = BitVector::deserialize_view(c)?;
        let po = PackedInts::deserialize_view(c)?;
        if bo.len() != po.len() {
            return Err(bad("run-mark/position length mismatch"));
        }
        if bo.count_ones() != n_obj {
            return Err(bad("run-mark count does not match object count"));
        }
        if !po.is_empty() {
            if !bo.get(0) {
                return Err(bad("first run-mark bit unset"));
            }
            let n = po.len() as u64;
            if po.iter().any(|v| v >= n) {
                return Err(bad("triple ordinal out of range"));
            }
        }
        Ok(Foq { wp, xo, bo, po })
    }

    /// Number of distinct object values.
    pub fn n_objects(&self) -> u64 {
        self.xo.len() as u64
    }

    /// Index of object value `o`, if present.
    pub fn locate_object(&self, o: u64) -> Option<u64> {
        let (mut lo, mut hi) = (0usize, self.xo.len());
        while lo < hi {
            let mid = (lo + hi) / 2;
            if self.xo.get(mid) < o {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        (lo < self.xo.len() && self.xo.get(lo) == o).then_some(lo as u64)
    }

    /// `Po` position range of object index `j`.
    pub fn object_run(&self, j: u64) -> (u64, u64) {
        let start = self.bo.select1(j).expect("object index in range") as u64;
        let end = self
            .bo
            .select1(j + 1)
            .map_or(self.po.len() as u64, |e| e as u64);
        (start, end)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bt::BtBuilder;

    #[test]
    fn build_and_accessors() {
        // Triples (s, p, o) in SPO order; objects deliberately sparse.
        let triples = [[0u64, 0, 5], [0, 1, 2], [1, 0, 5], [1, 1, 9], [2, 0, 2]];
        let mut b = BtBuilder::new(false, 8, 8);
        for &[s, p, o] in &triples {
            b.push(s, p, o, None).unwrap();
        }
        let spo = b.finish();
        let mut pairs: Vec<(u64, u64)> = triples
            .iter()
            .enumerate()
            .map(|(i, t)| (t[2], i as u64))
            .collect();
        pairs.sort_unstable();
        let foq = Foq::build(&spo, pairs.into_iter(), 8);

        // Objects 2, 5, 9 with runs {1, 4}, {0, 2}, {3}.
        assert_eq!(foq.n_objects(), 3);
        assert_eq!(foq.locate_object(5), Some(1));
        assert_eq!(foq.locate_object(4), None);
        let (lo, hi) = foq.object_run(1);
        let run: Vec<u64> = (lo..hi).map(|r| foq.po.get(r as usize)).collect();
        assert_eq!(run, vec![0, 2]);

        // Wp mirrors Sy: predicates of the 5 distinct (s, p) pairs.
        assert_eq!(foq.wp.len(), 5);
        assert_eq!(foq.wp.rank(0, 5), 3);
        assert_eq!(foq.wp.select(1, 1), Some(3));

        // Round trip.
        let mut buf = Vec::new();
        foq.serialize_into(&mut buf).unwrap();
        let mut c = Cursor::new(graphy_succinct::Bytes::from_vec_aligned(buf));
        let back = Foq::deserialize(&mut c).unwrap();
        assert!(c.is_empty());
        assert_eq!(back.locate_object(9), Some(2));
        assert_eq!(back.object_run(2), (4, 5));
    }
}
