//! Wavelet matrix over u64 symbols (doc 02): one [`BitVector`] per bit of
//! symbol width, most-significant level first, each level stably partitioned
//! by the previous level's bits (zeros left, ones right). Supports
//! `access`, `rank(sym, i)`, and `select(sym, k)`.

use crate::bitvec::{BitVector, BitVectorBuilder};
use crate::intvec::{PackedInts, PackedIntsBuilder};

#[derive(Debug, Clone)]
pub struct WaveletMatrix {
    width: u32,
    len: usize,
    levels: Vec<BitVector>,
    /// Zeros at each level (the boundary where the ones' half starts).
    zeros: Vec<u64>,
}

impl WaveletMatrix {
    /// Build over `values`, each of which must fit in `width` bits.
    pub fn new(values: &[u64], width: u32) -> WaveletMatrix {
        assert!(width <= 64);
        debug_assert!(
            width == 64 || values.iter().all(|&v| v >> width == 0),
            "value exceeds symbol width {width}"
        );
        let mut cur = values.to_vec();
        let mut levels = Vec::with_capacity(width as usize);
        let mut zeros = Vec::with_capacity(width as usize);
        for level in 0..width {
            let shift = width - 1 - level;
            let mut builder = BitVectorBuilder::with_capacity(cur.len());
            for &v in &cur {
                builder.push(v >> shift & 1 == 1);
            }
            let bv = builder.build();
            // Stable partition for the next level: zeros first, then ones.
            let (mut zs, mut os) = (Vec::new(), Vec::new());
            for &v in &cur {
                if v >> shift & 1 == 1 {
                    os.push(v);
                } else {
                    zs.push(v);
                }
            }
            zs.append(&mut os);
            cur = zs;
            zeros.push(bv.count_zeros());
            levels.push(bv);
        }
        WaveletMatrix {
            width,
            len: values.len(),
            levels,
            zeros,
        }
    }

    /// Build over a packed sequence (width = the sequence's width) holding
    /// at most `2 × n × width` bits of scratch instead of `new`'s
    /// `2–3 × n × 64`: each level streams the previous level's partition
    /// (zeros half then ones half) and writes its own packed halves.
    /// Output is identical to [`WaveletMatrix::new`] over the same values.
    pub fn from_packed(values: &PackedInts) -> WaveletMatrix {
        let width = values.width();
        let n = values.len();
        let mut levels = Vec::with_capacity(width as usize);
        let mut zeros = Vec::with_capacity(width as usize);
        // The previous level's stable partition, zeros half + ones half
        // (`None` = level 0 reads `values` directly).
        let mut parts: Option<(PackedInts, PackedInts)> = None;
        for level in 0..width {
            let shift = width - 1 - level;
            let last = level + 1 == width;
            let mut builder = BitVectorBuilder::with_capacity(n);
            let mut zs = PackedIntsBuilder::new(width);
            let mut os = PackedIntsBuilder::new(width);
            let mut route = |v: u64| {
                let one = v >> shift & 1 == 1;
                builder.push(one);
                if last {
                    return; // no next level reads the partition
                }
                if one {
                    os.push(v);
                } else {
                    zs.push(v);
                }
            };
            match &parts {
                None => values.iter().for_each(&mut route),
                Some((z, o)) => z.iter().chain(o.iter()).for_each(&mut route),
            }
            let bv = builder.build();
            zeros.push(bv.count_zeros());
            levels.push(bv);
            if !last {
                parts = Some((zs.build(), os.build()));
            }
        }
        WaveletMatrix {
            width,
            len: n,
            levels,
            zeros,
        }
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn width(&self) -> u32 {
        self.width
    }

    /// Zeros per level (the boundary where each level's ones start).
    pub fn zeros(&self) -> &[u64] {
        &self.zeros
    }

    /// The per-level bit vectors, most significant bit first.
    pub fn levels(&self) -> &[BitVector] {
        &self.levels
    }

    /// Reassemble from raw parts (deserialization). Validates the shape and
    /// per-level zero counts, which is sufficient to keep every accessor
    /// in bounds.
    pub fn from_parts(
        width: u32,
        len: usize,
        levels: Vec<BitVector>,
        zeros: Vec<u64>,
    ) -> Result<WaveletMatrix, String> {
        if width > 64 {
            return Err(format!("width {width} > 64"));
        }
        if levels.len() != width as usize || zeros.len() != width as usize {
            return Err("level/zeros count does not match width".to_owned());
        }
        for (i, level) in levels.iter().enumerate() {
            if level.len() != len {
                return Err(format!(
                    "level {i} has length {}, expected {len}",
                    level.len()
                ));
            }
            if level.count_zeros() != zeros[i] {
                return Err(format!("level {i} zeros mismatch"));
            }
        }
        Ok(WaveletMatrix {
            width,
            len,
            levels,
            zeros,
        })
    }

    /// The symbol at position `i`.
    pub fn access(&self, i: usize) -> u64 {
        assert!(i < self.len, "index {i} out of bounds ({})", self.len);
        let mut i = i as u64;
        let mut v = 0u64;
        for (level, bv) in self.levels.iter().enumerate() {
            if bv.get(i as usize) {
                v |= 1 << (self.width - 1 - level as u32);
                i = self.zeros[level] + bv.rank1(i as usize);
            } else {
                i = bv.rank0(i as usize);
            }
        }
        v
    }

    /// Occurrences of `sym` in `[0, i)`. `i` may equal `len`.
    pub fn rank(&self, sym: u64, i: usize) -> u64 {
        assert!(i <= self.len, "rank index {i} out of bounds ({})", self.len);
        let (mut l, mut r) = (0u64, i as u64);
        for (level, bv) in self.levels.iter().enumerate() {
            if sym >> (self.width - 1 - level as u32) & 1 == 1 {
                l = self.zeros[level] + bv.rank1(l as usize);
                r = self.zeros[level] + bv.rank1(r as usize);
            } else {
                l = bv.rank0(l as usize);
                r = bv.rank0(r as usize);
            }
        }
        r - l
    }

    /// Position of the k-th (0-indexed) occurrence of `sym`.
    pub fn select(&self, sym: u64, k: u64) -> Option<usize> {
        // Descend to the bottom-level bucket of `sym`.
        let (mut l, mut r) = (0u64, self.len as u64);
        for (level, bv) in self.levels.iter().enumerate() {
            if sym >> (self.width - 1 - level as u32) & 1 == 1 {
                l = self.zeros[level] + bv.rank1(l as usize);
                r = self.zeros[level] + bv.rank1(r as usize);
            } else {
                l = bv.rank0(l as usize);
                r = bv.rank0(r as usize);
            }
        }
        if k >= r - l {
            return None;
        }
        // Ascend, inverting each level's mapping with select.
        let mut pos = l + k;
        for (level, bv) in self.levels.iter().enumerate().rev() {
            pos = if sym >> (self.width - 1 - level as u32) & 1 == 1 {
                bv.select1(pos - self.zeros[level])? as u64
            } else {
                bv.select0(pos)? as u64
            };
        }
        Some(pos as usize)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn xorshift(state: &mut u64) -> u64 {
        *state ^= *state << 13;
        *state ^= *state >> 7;
        *state ^= *state << 17;
        *state
    }

    fn check_against_brute_force(values: &[u64], width: u32) {
        let wm = WaveletMatrix::new(values, width);
        assert_eq!(wm.len(), values.len());
        let mut counts: std::collections::HashMap<u64, u64> = std::collections::HashMap::new();
        for (i, &v) in values.iter().enumerate() {
            assert_eq!(wm.access(i), v, "access({i})");
            let seen = counts.entry(v).or_default();
            assert_eq!(wm.rank(v, i), *seen, "rank({v}, {i})");
            assert_eq!(wm.select(v, *seen), Some(i), "select({v}, {seen})");
            *seen += 1;
        }
        for (&v, &c) in &counts {
            assert_eq!(wm.rank(v, values.len()), c);
            assert_eq!(wm.select(v, c), None);
        }
        // A symbol that never occurs.
        let absent = (1u64 << width.min(63)) - 1;
        if !counts.contains_key(&absent) {
            assert_eq!(wm.rank(absent, values.len()), 0);
            assert_eq!(wm.select(absent, 0), None);
        }
    }

    #[test]
    fn small_alphabet() {
        let values = [3u64, 1, 4, 1, 5, 2, 6, 5, 3, 5, 0, 7, 7, 1];
        check_against_brute_force(&values, 3);
    }

    #[test]
    fn random_symbols() {
        let mut state = 0xDEAD_BEEF_CAFE_F00D;
        for width in [1u32, 5, 16, 40] {
            let mask = if width == 64 {
                u64::MAX
            } else {
                (1 << width) - 1
            };
            let values: Vec<u64> = (0..500).map(|_| xorshift(&mut state) & mask).collect();
            check_against_brute_force(&values, width);
        }
    }

    #[test]
    fn from_packed_matches_new() {
        let mut state = 0x243F_6A88_85A3_08D3;
        for width in [1u32, 3, 6, 17] {
            let mask = (1u64 << width) - 1;
            let vals: Vec<u64> = (0..4097).map(|_| xorshift(&mut state) & mask).collect();
            let a = WaveletMatrix::new(&vals, width);
            let b =
                WaveletMatrix::from_packed(&PackedInts::with_width(vals.iter().copied(), width));
            assert_eq!(a.len(), b.len());
            assert_eq!(a.width(), b.width());
            assert_eq!(a.zeros(), b.zeros());
            for (la, lb) in a.levels().iter().zip(b.levels()) {
                assert_eq!(la.words(), lb.words());
            }
        }
        // Empty and width-0 inputs mirror `new`.
        assert!(WaveletMatrix::from_packed(&PackedInts::with_width([], 4)).is_empty());
        let z = WaveletMatrix::from_packed(&PackedInts::with_width([0, 0], 0));
        assert_eq!((z.len(), z.width()), (2, 0));
    }

    #[test]
    fn empty_and_width_zero() {
        let wm = WaveletMatrix::new(&[], 8);
        assert!(wm.is_empty());
        assert_eq!(wm.rank(3, 0), 0);
        assert_eq!(wm.select(3, 0), None);

        // Width 0: every value is the empty symbol 0.
        let wm = WaveletMatrix::new(&[0, 0, 0], 0);
        assert_eq!(wm.access(1), 0);
        assert_eq!(wm.rank(0, 3), 3);
        assert_eq!(wm.select(0, 2), Some(2));
        assert_eq!(wm.select(0, 3), None);
    }
}
