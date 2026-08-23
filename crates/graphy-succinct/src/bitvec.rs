//! Rank/select bit vector (doc 02): plain u64 words plus a two-level rank
//! directory — a cumulative u64 per 4096-bit superblock and a relative u16
//! per 512-bit block (~4.7% overhead) — and sampled select acceleration
//! (superblock hint for every 4096th one/zero, then binary search, block
//! scan, and an in-word byte-popcount walk).

use crate::mem::Words;

const SUPER_BITS: usize = 4096;
const BLOCK_BITS: usize = 512;
const BLOCKS_PER_SUPER: usize = SUPER_BITS / BLOCK_BITS; // 8
const WORDS_PER_BLOCK: usize = BLOCK_BITS / 64; // 8
const SELECT_SAMPLE: u64 = 4096;

/// Append-only builder; bits are pushed least-index-first.
#[derive(Debug, Default)]
pub struct BitVectorBuilder {
    words: Vec<u64>,
    len: usize,
}

impl BitVectorBuilder {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_capacity(bits: usize) -> Self {
        BitVectorBuilder {
            words: Vec::with_capacity(bits.div_ceil(64)),
            len: 0,
        }
    }

    pub fn push(&mut self, bit: bool) {
        let word = self.len / 64;
        if word == self.words.len() {
            self.words.push(0);
        }
        if bit {
            self.words[word] |= 1 << (self.len % 64);
        }
        self.len += 1;
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn build(self) -> BitVector {
        BitVector::from_words(Words::from_vec(self.words), self.len)
    }

    /// Bulk-append whole words carrying `len` bits (an efficient path for
    /// deserialization). The builder must be empty; trailing bits of the
    /// last word must be zero.
    pub fn push_words(&mut self, words: &[u64], len: usize) {
        assert!(self.len == 0, "push_words requires an empty builder");
        assert!(words.len() == len.div_ceil(64), "word count mismatch");
        assert!(
            len % 64 == 0 || words.last().is_none_or(|w| w >> (len % 64) == 0),
            "trailing bits past len must be zero"
        );
        self.words.extend_from_slice(words);
        self.len = len;
    }
}

impl Extend<bool> for BitVectorBuilder {
    fn extend<T: IntoIterator<Item = bool>>(&mut self, iter: T) {
        for bit in iter {
            self.push(bit);
        }
    }
}

impl FromIterator<bool> for BitVector {
    fn from_iter<T: IntoIterator<Item = bool>>(iter: T) -> Self {
        let mut b = BitVectorBuilder::new();
        b.extend(iter);
        b.build()
    }
}

/// An immutable bit vector with O(1) rank and near-O(1) select. The payload
/// words may be a zero-copy view (mmap); the rank/select directories always
/// live on the heap (they are rebuilt at open, never persisted).
#[derive(Debug, Clone)]
pub struct BitVector {
    words: Words,
    len: usize,
    ones: u64,
    /// Ones before each superblock; one entry per superblock boundary in
    /// `0..=len/SUPER_BITS`, so `rank` lookups at `i == len` stay in bounds.
    super_ranks: Vec<u64>,
    /// Ones before each block, relative to its superblock (< 4096 fits u16).
    block_ranks: Vec<u16>,
    /// Superblock index containing every SELECT_SAMPLE-th one / zero.
    select1_samples: Vec<u32>,
    select0_samples: Vec<u32>,
}

impl BitVector {
    /// Build the rank/select directories over `words` (one popcount pass).
    /// Callers must have validated the shape (word count, clean padding).
    pub(crate) fn from_words(words: Words, len: usize) -> BitVector {
        debug_assert!(words.len() == len.div_ceil(64));
        debug_assert!(
            len % 64 == 0 || words.last().is_none_or(|w| w >> (len % 64) == 0),
            "trailing bits past len must be zero"
        );
        let n_blocks = len / BLOCK_BITS + 1;
        let mut super_ranks = Vec::with_capacity(len / SUPER_BITS + 2);
        let mut block_ranks = Vec::with_capacity(n_blocks);
        let mut total: u64 = 0;
        let mut in_super: u64 = 0;
        for blk in 0..n_blocks {
            if blk % BLOCKS_PER_SUPER == 0 {
                super_ranks.push(total);
                in_super = 0;
            }
            block_ranks.push(in_super as u16);
            let start = (blk * WORDS_PER_BLOCK).min(words.len());
            let end = ((blk + 1) * WORDS_PER_BLOCK).min(words.len());
            let c: u64 = words[start..end]
                .iter()
                .map(|w| u64::from(w.count_ones()))
                .sum();
            total += c;
            in_super += c;
        }
        let ones = total;
        let zeros = len as u64 - ones;

        // Superblock hints: cumulative ones (zeros) before superblock sb are
        // super_ranks[sb] (sb·4096 − super_ranks[sb]).
        let sample = |cumulative: &dyn Fn(usize) -> u64, count: u64| -> Vec<u32> {
            let n_super = super_ranks.len();
            let mut samples = Vec::with_capacity((count / SELECT_SAMPLE) as usize + 1);
            let mut sb = 0;
            let mut target = 0;
            while target < count {
                while sb + 1 < n_super && cumulative(sb + 1) <= target {
                    sb += 1;
                }
                samples.push(sb as u32);
                target += SELECT_SAMPLE;
            }
            samples
        };
        let select1_samples = sample(&|sb| super_ranks[sb], ones);
        let select0_samples = sample(&|sb| (sb * SUPER_BITS) as u64 - super_ranks[sb], zeros);

        BitVector {
            words,
            len,
            ones,
            super_ranks,
            block_ranks,
            select1_samples,
            select0_samples,
        }
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Number of set bits.
    pub fn count_ones(&self) -> u64 {
        self.ones
    }

    /// Number of clear bits.
    pub fn count_zeros(&self) -> u64 {
        self.len as u64 - self.ones
    }

    /// The raw payload words (little-endian bit order within each word).
    pub fn words(&self) -> &[u64] {
        &self.words[..]
    }

    pub fn get(&self, i: usize) -> bool {
        assert!(i < self.len, "bit index {i} out of bounds ({})", self.len);
        self.words[i / 64] >> (i % 64) & 1 == 1
    }

    /// Ones in `[0, i)`. `i` may equal `len`.
    pub fn rank1(&self, i: usize) -> u64 {
        assert!(i <= self.len, "rank index {i} out of bounds ({})", self.len);
        let blk = i / BLOCK_BITS;
        let mut r = self.super_ranks[i / SUPER_BITS] + u64::from(self.block_ranks[blk]);
        for w in &self.words[blk * WORDS_PER_BLOCK..i / 64] {
            r += u64::from(w.count_ones());
        }
        let partial = i % 64;
        if partial != 0 {
            r += u64::from((self.words[i / 64] & ((1 << partial) - 1)).count_ones());
        }
        r
    }

    /// Zeros in `[0, i)`. `i` may equal `len`.
    pub fn rank0(&self, i: usize) -> u64 {
        i as u64 - self.rank1(i)
    }

    /// Position of the k-th one (0-indexed), or `None` if `k >= count_ones`.
    pub fn select1(&self, k: u64) -> Option<usize> {
        if k >= self.ones {
            return None;
        }
        let sb = self.select_superblock(&self.select1_samples, k, &|sb| self.super_ranks[sb]);
        let mut rem = k - self.super_ranks[sb];

        // Scan the (≤ 8) blocks of the superblock.
        let first_blk = sb * BLOCKS_PER_SUPER;
        let last_blk = (first_blk + BLOCKS_PER_SUPER).min(self.block_ranks.len());
        let mut blk = first_blk;
        while blk + 1 < last_blk && u64::from(self.block_ranks[blk + 1]) <= rem {
            blk += 1;
        }
        rem -= u64::from(self.block_ranks[blk]);

        // Scan words; the answer is guaranteed within the block.
        for (wi, w) in self.words.iter().enumerate().skip(blk * WORDS_PER_BLOCK) {
            let c = u64::from(w.count_ones());
            if rem < c {
                return Some(wi * 64 + select_in_word(*w, rem as u32));
            }
            rem -= c;
        }
        unreachable!("select1 target counted by rank directory");
    }

    /// Position of the k-th zero (0-indexed), or `None` if `k >= count_zeros`.
    pub fn select0(&self, k: u64) -> Option<usize> {
        if k >= self.count_zeros() {
            return None;
        }
        let zeros_before = |sb: usize| (sb * SUPER_BITS) as u64 - self.super_ranks[sb];
        let sb = self.select_superblock(&self.select0_samples, k, &zeros_before);
        let mut rem = k - zeros_before(sb);

        let first_blk = sb * BLOCKS_PER_SUPER;
        let last_blk = (first_blk + BLOCKS_PER_SUPER).min(self.block_ranks.len());
        let zeros_in_super_before =
            |blk: usize| ((blk - first_blk) * BLOCK_BITS) as u64 - u64::from(self.block_ranks[blk]);
        let mut blk = first_blk;
        while blk + 1 < last_blk && zeros_in_super_before(blk + 1) <= rem {
            blk += 1;
        }
        rem -= zeros_in_super_before(blk);

        // The k-th zero lies before `len`, so padding zeros in the final
        // partial word are never reached.
        for (wi, w) in self.words.iter().enumerate().skip(blk * WORDS_PER_BLOCK) {
            let inv = !*w;
            let c = u64::from(inv.count_ones());
            if rem < c {
                return Some(wi * 64 + select_in_word(inv, rem as u32));
            }
            rem -= c;
        }
        unreachable!("select0 target counted by rank directory");
    }

    /// Position of the first set bit at or after `from` — a forward word
    /// scan, so sequential sweeps (`from` = previous hit + 1) cost O(1)
    /// amortized instead of a full `select` per step.
    pub fn next_one(&self, from: usize) -> Option<usize> {
        if from >= self.len {
            return None;
        }
        let mut wi = from / 64;
        let mut w = self.words[wi] & (!0u64 << (from % 64));
        loop {
            if w != 0 {
                let pos = wi * 64 + w.trailing_zeros() as usize;
                return (pos < self.len).then_some(pos);
            }
            wi += 1;
            if wi >= self.words.len() {
                return None;
            }
            w = self.words[wi];
        }
    }

    /// Largest superblock whose cumulative count is ≤ k, narrowed by the
    /// sample hints then binary-searched.
    fn select_superblock(
        &self,
        samples: &[u32],
        k: u64,
        cumulative: &dyn Fn(usize) -> u64,
    ) -> usize {
        let j = (k / SELECT_SAMPLE) as usize;
        let mut lo = samples[j] as usize;
        let mut hi = samples
            .get(j + 1)
            .map_or(self.super_ranks.len(), |&s| s as usize + 1);
        while hi - lo > 1 {
            let mid = (lo + hi) / 2;
            if cumulative(mid) <= k {
                lo = mid;
            } else {
                hi = mid;
            }
        }
        lo
    }
}

/// Bit position of the r-th (0-indexed) set bit of `w`; `r < w.count_ones()`.
fn select_in_word(w: u64, r: u32) -> usize {
    let mut rem = r;
    let mut w = w;
    let mut pos = 0;
    loop {
        let byte = (w & 0xFF) as u8;
        let c = byte.count_ones();
        if rem < c {
            let mut b = byte;
            for _ in 0..rem {
                b &= b - 1; // clear lowest set bit
            }
            return pos + b.trailing_zeros() as usize;
        }
        rem -= c;
        w >>= 8;
        pos += 8;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Deterministic xorshift so tests need no rand dependency.
    fn xorshift(state: &mut u64) -> u64 {
        *state ^= *state << 13;
        *state ^= *state >> 7;
        *state ^= *state << 17;
        *state
    }

    fn build_random(
        len: usize,
        density_num: u64,
        density_den: u64,
        seed: u64,
    ) -> (BitVector, Vec<bool>) {
        let mut state = seed;
        let bits: Vec<bool> = (0..len)
            .map(|_| xorshift(&mut state) % density_den < density_num)
            .collect();
        (bits.iter().copied().collect(), bits)
    }

    fn check_all(bv: &BitVector, bits: &[bool]) {
        assert_eq!(bv.len(), bits.len());
        let mut ones = 0u64;
        for (i, &b) in bits.iter().enumerate() {
            assert_eq!(bv.get(i), b, "get({i})");
            assert_eq!(bv.rank1(i), ones, "rank1({i})");
            assert_eq!(bv.rank0(i), i as u64 - ones, "rank0({i})");
            if b {
                assert_eq!(bv.select1(ones), Some(i), "select1({ones})");
            } else {
                assert_eq!(bv.select0(i as u64 - ones), Some(i), "select0");
            }
            ones += u64::from(b);
        }
        assert_eq!(bv.rank1(bits.len()), ones);
        assert_eq!(bv.count_ones(), ones);
        assert_eq!(bv.select1(ones), None);
        assert_eq!(bv.select0(bits.len() as u64 - ones), None);
    }

    #[test]
    fn empty() {
        let bv: BitVector = std::iter::empty().collect();
        assert_eq!(bv.len(), 0);
        assert_eq!(bv.rank1(0), 0);
        assert_eq!(bv.select1(0), None);
        assert_eq!(bv.select0(0), None);
    }

    #[test]
    fn small_dense_and_sparse() {
        for (num, den) in [(1, 2), (1, 17), (16, 17), (0, 1), (1, 1)] {
            let (bv, bits) = build_random(1000, num, den, 0x9E37_79B9_7F4A_7C15);
            check_all(&bv, &bits);
        }
    }

    #[test]
    fn boundary_lengths() {
        // Exercise word/block/superblock boundaries exactly.
        for len in [1, 63, 64, 65, 511, 512, 513, 4095, 4096, 4097, 8192] {
            let (bv, bits) = build_random(len, 1, 3, len as u64 + 1);
            check_all(&bv, &bits);
        }
    }

    #[test]
    #[cfg_attr(miri, ignore)] // large loop; covered smaller under Miri
    fn crosses_select_samples() {
        // > 4096 ones and > 4096 zeros to exercise the sample directory.
        let (bv, bits) = build_random(40_000, 1, 2, 42);
        check_all(&bv, &bits);
    }

    #[test]
    fn all_ones_run() {
        let bits = vec![true; 5000];
        let bv: BitVector = bits.iter().copied().collect();
        check_all(&bv, &bits);
    }

    #[test]
    fn next_one_agrees_with_select() {
        for (num, den, len) in [
            (1u64, 7u64, 1000usize),
            (1, 2, 300),
            (0, 1, 100),
            (1, 1, 130),
        ] {
            let (bv, bits) = build_random(len, num, den, 0xABCD + len as u64);
            // From every position, next_one finds exactly the next set bit.
            let mut expected: Vec<Option<usize>> = vec![None; len + 1];
            for i in (0..len).rev() {
                expected[i] = if bits[i] { Some(i) } else { expected[i + 1] };
            }
            for (i, &want) in expected[..len].iter().enumerate() {
                assert_eq!(bv.next_one(i), want, "from {i}");
            }
            assert_eq!(bv.next_one(len), None);
            // Sequential sweep enumerates the ones in order.
            let mut got = Vec::new();
            let mut at = 0;
            while let Some(p) = bv.next_one(at) {
                got.push(p);
                at = p + 1;
            }
            let ones: Vec<usize> = (0..len).filter(|&i| bits[i]).collect();
            assert_eq!(got, ones);
        }
    }
}
