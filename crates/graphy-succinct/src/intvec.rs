//! Fixed-width integer sequences (doc 02): bit-packed [`PackedInts`] with
//! two-word reads, and byte-aligned [`AlignedInts`] trading space for simpler
//! loads. Builders pick the width from the maximum value.

use crate::mem::Words;

/// Bit-packed unsigned integers of a fixed width `0..=64`. The packed words
/// may be a zero-copy view (mmap).
#[derive(Debug, Clone)]
pub struct PackedInts {
    width: u32,
    len: usize,
    data: Words,
}

impl PackedInts {
    /// Pack `values` at the smallest width that fits the maximum.
    pub fn from_slice(values: &[u64]) -> PackedInts {
        let width = bits_for(values.iter().copied().max().unwrap_or(0));
        Self::with_width(values.iter().copied(), width)
    }

    /// Pack at an explicit width; every value must fit.
    pub fn with_width(values: impl IntoIterator<Item = u64>, width: u32) -> PackedInts {
        assert!(width <= 64);
        let mut b = PackedIntsBuilder::new(width);
        for v in values {
            b.push(v);
        }
        b.build()
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

    pub fn get(&self, i: usize) -> u64 {
        assert!(i < self.len, "index {i} out of bounds ({})", self.len);
        if self.width == 0 {
            return 0;
        }
        let bit = i * self.width as usize;
        let word = bit / 64;
        let off = (bit % 64) as u32;
        let mut v = self.data[word] >> off;
        if off + self.width > 64 {
            v |= self.data[word + 1] << (64 - off);
        }
        if self.width == 64 {
            v
        } else {
            v & ((1 << self.width) - 1)
        }
    }

    pub fn iter(&self) -> impl Iterator<Item = u64> + '_ {
        (0..self.len).map(|i| self.get(i))
    }

    /// The raw packed words.
    pub fn data(&self) -> &[u64] {
        &self.data[..]
    }

    /// Reassemble from raw parts (deserialization), validating word count
    /// and clean padding so the round-trip is canonical.
    pub fn from_parts(width: u32, len: usize, data: Vec<u64>) -> Result<PackedInts, String> {
        Self::from_words(width, len, Words::from_vec(data))
    }

    /// [`PackedInts::from_parts`] over an owner-backed (possibly zero-copy)
    /// word view.
    pub fn from_words(width: u32, len: usize, data: Words) -> Result<PackedInts, String> {
        if width > 64 {
            return Err(format!("width {width} > 64"));
        }
        let bits = len as u64 * u64::from(width);
        let expect = bits.div_ceil(64) as usize;
        if data.len() != expect {
            return Err(format!("data has {} words, expected {expect}", data.len()));
        }
        if bits % 64 != 0 {
            if let Some(&last) = data.last() {
                if last >> (bits % 64) != 0 {
                    return Err("padding bits past the last value are not zero".to_owned());
                }
            }
        }
        Ok(PackedInts { width, len, data })
    }
}

/// Smallest width that represents `max` (0 for 0).
pub fn bits_for(max: u64) -> u32 {
    64 - max.leading_zeros()
}

/// Incremental fixed-width builder for streaming construction (the segment
/// builders know each column's maximum before streaming values through).
#[derive(Debug)]
pub struct PackedIntsBuilder {
    width: u32,
    len: usize,
    data: Vec<u64>,
}

impl PackedIntsBuilder {
    pub fn new(width: u32) -> PackedIntsBuilder {
        assert!(width <= 64);
        PackedIntsBuilder {
            width,
            len: 0,
            data: Vec::new(),
        }
    }

    pub fn push(&mut self, v: u64) {
        debug_assert!(
            self.width == 64 || v < 1 << self.width,
            "value {v} exceeds width {}",
            self.width
        );
        if self.width == 0 {
            self.len += 1;
            return;
        }
        let bit = self.len * self.width as usize;
        let word = bit / 64;
        let off = (bit % 64) as u32;
        if word == self.data.len() {
            self.data.push(0);
        }
        self.data[word] |= v << off;
        if off + self.width > 64 {
            self.data.push(v >> (64 - off));
        }
        self.len += 1;
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn build(self) -> PackedInts {
        PackedInts::from_parts(self.width, self.len, self.data)
            .expect("builder maintains the invariants")
    }
}

/// Byte-aligned unsigned integers at the smallest of u8/u16/u32/u64.
#[derive(Debug, Clone)]
pub enum AlignedInts {
    U8(Vec<u8>),
    U16(Vec<u16>),
    U32(Vec<u32>),
    U64(Vec<u64>),
}

impl AlignedInts {
    pub fn from_slice(values: &[u64]) -> AlignedInts {
        match values.iter().copied().max().unwrap_or(0) {
            m if m <= u64::from(u8::MAX) => {
                AlignedInts::U8(values.iter().map(|&v| v as u8).collect())
            }
            m if m <= u64::from(u16::MAX) => {
                AlignedInts::U16(values.iter().map(|&v| v as u16).collect())
            }
            m if m <= u64::from(u32::MAX) => {
                AlignedInts::U32(values.iter().map(|&v| v as u32).collect())
            }
            _ => AlignedInts::U64(values.to_vec()),
        }
    }

    pub fn len(&self) -> usize {
        match self {
            AlignedInts::U8(v) => v.len(),
            AlignedInts::U16(v) => v.len(),
            AlignedInts::U32(v) => v.len(),
            AlignedInts::U64(v) => v.len(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn get(&self, i: usize) -> u64 {
        match self {
            AlignedInts::U8(v) => u64::from(v[i]),
            AlignedInts::U16(v) => u64::from(v[i]),
            AlignedInts::U32(v) => u64::from(v[i]),
            AlignedInts::U64(v) => v[i],
        }
    }

    pub fn iter(&self) -> impl Iterator<Item = u64> + '_ {
        (0..self.len()).map(|i| self.get(i))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn packed_round_trip_all_widths() {
        for width in 0..=64u32 {
            let mask = if width == 64 {
                u64::MAX
            } else {
                (1u64 << width) - 1
            };
            let values: Vec<u64> = (0..200u64)
                .map(|i| i.wrapping_mul(0x9E37_79B9_7F4A_7C15).rotate_left(i as u32) & mask)
                .collect();
            let packed = PackedInts::with_width(values.iter().copied(), width);
            assert_eq!(packed.len(), values.len());
            for (i, &v) in values.iter().enumerate() {
                assert_eq!(packed.get(i), v, "width {width} index {i}");
            }
            assert_eq!(packed.iter().collect::<Vec<_>>(), values);
        }
    }

    #[test]
    fn from_slice_picks_minimal_width() {
        assert_eq!(PackedInts::from_slice(&[]).width(), 0);
        assert_eq!(PackedInts::from_slice(&[0, 0]).width(), 0);
        assert_eq!(PackedInts::from_slice(&[1]).width(), 1);
        assert_eq!(PackedInts::from_slice(&[255]).width(), 8);
        assert_eq!(PackedInts::from_slice(&[256]).width(), 9);
        assert_eq!(PackedInts::from_slice(&[u64::MAX]).width(), 64);
        let packed = PackedInts::from_slice(&[3, 7, 0, 6]);
        assert_eq!(packed.width(), 3);
        assert_eq!(packed.iter().collect::<Vec<_>>(), vec![3, 7, 0, 6]);
    }

    #[test]
    fn aligned_picks_minimal_bytes() {
        assert!(matches!(
            AlignedInts::from_slice(&[0, 255]),
            AlignedInts::U8(_)
        ));
        assert!(matches!(
            AlignedInts::from_slice(&[256]),
            AlignedInts::U16(_)
        ));
        assert!(matches!(
            AlignedInts::from_slice(&[1 << 16]),
            AlignedInts::U32(_)
        ));
        assert!(matches!(
            AlignedInts::from_slice(&[1 << 32]),
            AlignedInts::U64(_)
        ));
        let values = [7u64, 0, 65535, 300];
        let a = AlignedInts::from_slice(&values);
        assert_eq!(a.iter().collect::<Vec<_>>(), values);
    }
}
