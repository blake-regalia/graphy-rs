//! Serialization for the succinct structures (segment format v2, doc 02 §6 /
//! docs/08-segment-format.md).
//!
//! Only essential state is stored (lengths + payload words); acceleration
//! directories (rank/select, PFC nothing, wavelet nothing) are rebuilt at
//! load — a single popcount pass, ~milliseconds even at 10⁹ bits. All
//! integers little-endian. Deserialization fully validates: these routines
//! must be safe on untrusted bytes (the store checksums components, but
//! structural validation is defense in depth).
//!
//! Each structure has two read paths with identical validation: `deserialize`
//! (any `Read`, copies into heap-owned storage) and `deserialize_view`
//! (a [`Cursor`] over owner-backed bytes — payload arrays become zero-copy
//! views, per the format's v2 alignment rule).

use std::io::{self, Read, Write};

use crate::bitvec::{BitVector, BitVectorBuilder};
use crate::intvec::PackedInts;
use crate::mem::Cursor;
use crate::pfc::Pfc;
use crate::wavelet::WaveletMatrix;

pub fn write_u64<W: Write>(w: &mut W, v: u64) -> io::Result<()> {
    w.write_all(&v.to_le_bytes())
}

pub fn read_u64<R: Read>(r: &mut R) -> io::Result<u64> {
    let mut b = [0u8; 8];
    r.read_exact(&mut b)?;
    Ok(u64::from_le_bytes(b))
}

fn read_len(r: &mut impl Read, cap: u64, what: &str) -> io::Result<usize> {
    let v = read_u64(r)?;
    if v > cap {
        return Err(bad(format!("{what} length {v} exceeds sanity cap {cap}")));
    }
    Ok(v as usize)
}

fn bad(msg: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg.into())
}

/// Write a u64 slice in bulk (LE), buffering to avoid per-word syscalls.
pub fn write_u64s<W: Write>(w: &mut W, values: &[u64]) -> io::Result<()> {
    let mut buf = [0u8; 8 * 1024];
    for chunk in values.chunks(1024) {
        for (i, v) in chunk.iter().enumerate() {
            buf[i * 8..i * 8 + 8].copy_from_slice(&v.to_le_bytes());
        }
        w.write_all(&buf[..chunk.len() * 8])?;
    }
    Ok(())
}

pub fn read_u64s<R: Read>(r: &mut R, n: usize) -> io::Result<Vec<u64>> {
    let mut out = Vec::with_capacity(n);
    let mut buf = [0u8; 8 * 1024];
    let mut remaining = n;
    while remaining > 0 {
        let take = remaining.min(1024);
        r.read_exact(&mut buf[..take * 8])?;
        for i in 0..take {
            out.push(u64::from_le_bytes(
                buf[i * 8..i * 8 + 8].try_into().expect("8 bytes"),
            ));
        }
        remaining -= take;
    }
    Ok(out)
}

/// Sanity cap for element counts read from untrusted input (2⁴⁸ elements).
const CAP: u64 = 1 << 48;

impl BitVector {
    /// `[len_bits u64][words ⌈len/64⌉ × u64]`
    pub fn serialize_into<W: Write>(&self, w: &mut W) -> io::Result<()> {
        write_u64(w, self.len() as u64)?;
        write_u64s(w, self.words())
    }

    pub fn deserialize<R: Read>(r: &mut R) -> io::Result<BitVector> {
        let len = read_len(r, CAP, "bitvector")?;
        let words = read_u64s(r, len.div_ceil(64))?;
        if len % 64 != 0 {
            if let Some(&last) = words.last() {
                if last >> (len % 64) != 0 {
                    return Err(bad("bitvector trailing bits past len are not zero"));
                }
            }
        }
        let mut b = BitVectorBuilder::with_capacity(len);
        // Rebuild through the builder to recompute directories; bulk-copy of
        // whole words keeps this a memcpy + one directory pass.
        b.push_words(&words, len);
        Ok(b.build())
    }

    /// Zero-copy [`BitVector::deserialize`]: payload words stay a view; the
    /// rank/select directories are built with the usual popcount pass.
    pub fn deserialize_view(c: &mut Cursor) -> io::Result<BitVector> {
        let len = c.read_u64()?;
        if len > CAP {
            return Err(bad(format!("bitvector length {len} exceeds sanity cap")));
        }
        let len = len as usize;
        let words = c.take_words(len.div_ceil(64))?;
        if len % 64 != 0 {
            if let Some(&last) = words.last() {
                if last >> (len % 64) != 0 {
                    return Err(bad("bitvector trailing bits past len are not zero"));
                }
            }
        }
        Ok(BitVector::from_words(words, len))
    }
}

impl PackedInts {
    /// `[width u64][len u64][data words]`
    pub fn serialize_into<W: Write>(&self, w: &mut W) -> io::Result<()> {
        write_u64(w, u64::from(self.width()))?;
        write_u64(w, self.len() as u64)?;
        write_u64s(w, self.data())
    }

    pub fn deserialize<R: Read>(r: &mut R) -> io::Result<PackedInts> {
        let width = read_u64(r)?;
        if width > 64 {
            return Err(bad(format!("packed-int width {width} > 64")));
        }
        let len = read_len(r, CAP, "packed ints")?;
        let n_words = (len as u64 * width).div_ceil(64) as usize;
        let data = read_u64s(r, n_words)?;
        PackedInts::from_parts(width as u32, len, data)
            .map_err(|m| bad(format!("packed ints: {m}")))
    }

    /// Zero-copy [`PackedInts::deserialize`].
    pub fn deserialize_view(c: &mut Cursor) -> io::Result<PackedInts> {
        let width = c.read_u64()?;
        if width > 64 {
            return Err(bad(format!("packed-int width {width} > 64")));
        }
        let len = c.read_u64()?;
        if len > CAP {
            return Err(bad(format!("packed-int length {len} exceeds sanity cap")));
        }
        let n_words = (len * width).div_ceil(64) as usize;
        let data = c.take_words(n_words)?;
        PackedInts::from_words(width as u32, len as usize, data)
            .map_err(|m| bad(format!("packed ints: {m}")))
    }
}

impl Pfc {
    /// `[block_size u64][n u64][n_offsets u64][offsets…][data_len u64][data]`
    pub fn serialize_into<W: Write>(&self, w: &mut W) -> io::Result<()> {
        write_u64(w, self.block_size() as u64)?;
        write_u64(w, self.len() as u64)?;
        let offsets = self.block_offsets();
        write_u64(w, offsets.len() as u64)?;
        write_u64s(w, offsets)?;
        write_u64(w, self.data().len() as u64)?;
        w.write_all(self.data())
    }

    pub fn deserialize<R: Read>(r: &mut R) -> io::Result<Pfc> {
        let block_size = read_len(r, 1 << 20, "PFC block size")?;
        let n = read_len(r, CAP, "PFC")?;
        let n_offsets = read_len(r, CAP, "PFC offsets")?;
        if block_size == 0 {
            return Err(bad("PFC block size zero"));
        }
        if n_offsets != n.div_ceil(block_size) {
            return Err(bad("PFC offset count does not match key count"));
        }
        let offsets = read_u64s(r, n_offsets)?;
        let data_len = read_len(r, CAP, "PFC data")?;
        let mut data = vec![0u8; data_len];
        r.read_exact(&mut data)?;
        Pfc::from_parts(block_size, n, data, offsets).map_err(|m| bad(format!("PFC: {m}")))
    }

    /// Zero-copy [`Pfc::deserialize`]: the coded heap and offset index stay
    /// views; the full validation walk still runs.
    pub fn deserialize_view(c: &mut Cursor) -> io::Result<Pfc> {
        let block_size = c.read_u64()?;
        if block_size == 0 || block_size > 1 << 20 {
            return Err(bad(format!("PFC block size {block_size} out of range")));
        }
        let n = c.read_u64()?;
        if n > CAP {
            return Err(bad(format!("PFC length {n} exceeds sanity cap")));
        }
        let n_offsets = c.read_u64()? as usize;
        if n_offsets != (n as usize).div_ceil(block_size as usize) {
            return Err(bad("PFC offset count does not match key count"));
        }
        let offsets = c.take_words(n_offsets)?;
        let data_len = c.read_u64()?;
        if data_len > CAP {
            return Err(bad(format!(
                "PFC data length {data_len} exceeds sanity cap"
            )));
        }
        let data = c.take_bytes(data_len as usize)?;
        Pfc::from_views(block_size as usize, n as usize, data, offsets)
            .map_err(|m| bad(format!("PFC: {m}")))
    }
}

impl WaveletMatrix {
    /// `[width u64][len u64][zeros × width][levels × BitVector]`
    pub fn serialize_into<W: Write>(&self, w: &mut W) -> io::Result<()> {
        write_u64(w, u64::from(self.width()))?;
        write_u64(w, self.len() as u64)?;
        write_u64s(w, self.zeros())?;
        for level in self.levels() {
            level.serialize_into(w)?;
        }
        Ok(())
    }

    pub fn deserialize<R: Read>(r: &mut R) -> io::Result<WaveletMatrix> {
        let width = read_u64(r)?;
        if width > 64 {
            return Err(bad(format!("wavelet width {width} > 64")));
        }
        let len = read_len(r, CAP, "wavelet")?;
        let zeros = read_u64s(r, width as usize)?;
        let mut levels = Vec::with_capacity(width as usize);
        for i in 0..width {
            let level = BitVector::deserialize(r)?;
            if level.len() != len {
                return Err(bad(format!("wavelet level {i} length mismatch")));
            }
            if level.count_zeros() != zeros[i as usize] {
                return Err(bad(format!("wavelet level {i} zeros mismatch")));
            }
            levels.push(level);
        }
        WaveletMatrix::from_parts(width as u32, len, levels, zeros)
            .map_err(|m| bad(format!("wavelet: {m}")))
    }

    /// Zero-copy [`WaveletMatrix::deserialize`]: level payloads stay views.
    pub fn deserialize_view(c: &mut Cursor) -> io::Result<WaveletMatrix> {
        let width = c.read_u64()?;
        if width > 64 {
            return Err(bad(format!("wavelet width {width} > 64")));
        }
        let len = c.read_u64()?;
        if len > CAP {
            return Err(bad(format!("wavelet length {len} exceeds sanity cap")));
        }
        let len = len as usize;
        let zeros: Vec<u64> = (0..width)
            .map(|_| c.read_u64())
            .collect::<io::Result<_>>()?;
        let mut levels = Vec::with_capacity(width as usize);
        for i in 0..width {
            let level = BitVector::deserialize_view(c)?;
            if level.len() != len {
                return Err(bad(format!("wavelet level {i} length mismatch")));
            }
            if level.count_zeros() != zeros[i as usize] {
                return Err(bad(format!("wavelet level {i} zeros mismatch")));
            }
            levels.push(level);
        }
        WaveletMatrix::from_parts(width as u32, len, levels, zeros)
            .map_err(|m| bad(format!("wavelet: {m}")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pfc::PfcBuilder;

    fn round_trip<T>(
        value: &T,
        ser: impl Fn(&T, &mut Vec<u8>) -> io::Result<()>,
        de: impl Fn(&mut &[u8]) -> io::Result<T>,
    ) -> T {
        let mut buf = Vec::new();
        ser(value, &mut buf).unwrap();
        let mut slice = buf.as_slice();
        let back = de(&mut slice).unwrap();
        assert!(slice.is_empty(), "trailing bytes after deserialize");
        back
    }

    #[test]
    fn bitvector_round_trip() {
        for len in [0usize, 1, 63, 64, 65, 5000] {
            let bits: Vec<bool> = (0..len).map(|i| i % 3 == 0).collect();
            let bv: BitVector = bits.iter().copied().collect();
            let back = round_trip(
                &bv,
                |v, w| v.serialize_into(w),
                |r| BitVector::deserialize(r),
            );
            assert_eq!(back.len(), bv.len());
            assert_eq!(back.count_ones(), bv.count_ones());
            for (i, &b) in bits.iter().enumerate() {
                assert_eq!(back.get(i), b);
                assert_eq!(back.rank1(i), bv.rank1(i));
            }
        }
    }

    #[test]
    fn bitvector_rejects_dirty_padding() {
        let bv: BitVector = std::iter::repeat_n(true, 10).collect();
        let mut buf = Vec::new();
        bv.serialize_into(&mut buf).unwrap();
        // Set a bit beyond len in the last word.
        let last = buf.len() - 1;
        buf[last] |= 0x80;
        assert!(BitVector::deserialize(&mut buf.as_slice()).is_err());
    }

    #[test]
    fn packed_ints_round_trip() {
        for width in [0u32, 1, 7, 33, 64] {
            let mask = if width == 64 {
                u64::MAX
            } else {
                (1u64 << width) - 1
            };
            let values: Vec<u64> = (0..300u64).map(|i| (i * 0x9E37) & mask).collect();
            let p = PackedInts::with_width(values.iter().copied(), width);
            let back = round_trip(
                &p,
                |v, w| v.serialize_into(w),
                |r| PackedInts::deserialize(r),
            );
            assert_eq!(back.iter().collect::<Vec<_>>(), values);
        }
    }

    #[test]
    fn pfc_round_trip() {
        let mut b = PfcBuilder::new(16);
        let keys: Vec<String> = (0..200).map(|i| format!("http://x/{i:04}")).collect();
        let mut sorted = keys.clone();
        sorted.sort();
        for k in &sorted {
            b.push(k.as_bytes());
        }
        let pfc = b.build();
        let back = round_trip(&pfc, |v, w| v.serialize_into(w), |r| Pfc::deserialize(r));
        assert_eq!(back.len(), pfc.len());
        for (i, k) in sorted.iter().enumerate() {
            assert_eq!(back.get(i).as_deref(), Some(k.as_bytes()));
            assert_eq!(back.locate(k.as_bytes()), Some(i));
        }
    }

    #[test]
    fn wavelet_round_trip() {
        let values: Vec<u64> = (0..500u64).map(|i| i * 31 % 200).collect();
        let wm = WaveletMatrix::new(&values, 8);
        let back = round_trip(
            &wm,
            |v, w| v.serialize_into(w),
            |r| WaveletMatrix::deserialize(r),
        );
        for (i, &v) in values.iter().enumerate() {
            assert_eq!(back.access(i), v);
        }
        assert_eq!(back.rank(31, values.len()), wm.rank(31, values.len()));
    }

    /// Serialize, then view-deserialize over an aligned byte view; the view
    /// path must accept exactly what the `Read` path accepts and agree on
    /// every accessor.
    #[test]
    fn view_deserializers_agree_with_read_path() {
        use crate::mem::{Bytes, Cursor};

        // One buffer holding all four structures back to back — exercises
        // cursor advancement across structure boundaries.
        let bits: Vec<bool> = (0..777).map(|i| i % 5 == 0).collect();
        let bv: BitVector = bits.iter().copied().collect();
        let pi = PackedInts::from_slice(&(0..300u64).map(|i| i * 7 % 97).collect::<Vec<_>>());
        let mut pb = PfcBuilder::new(8);
        let mut keys: Vec<String> = (0..100).map(|i| format!("k{i:03}")).collect();
        keys.sort();
        for k in &keys {
            pb.push(k.as_bytes());
        }
        let pfc = pb.build();
        let values: Vec<u64> = (0..333u64).map(|i| i * 13 % 50).collect();
        let wm = WaveletMatrix::new(&values, 6);

        let mut buf = Vec::new();
        bv.serialize_into(&mut buf).unwrap();
        pi.serialize_into(&mut buf).unwrap();
        pfc.serialize_into(&mut buf).unwrap();
        // PFC data is byte-granular; the writer pads before the next word
        // field in real components (docs/08 §1). Emulate that here.
        while buf.len() % 8 != 0 {
            buf.push(0);
        }
        wm.serialize_into(&mut buf).unwrap();

        let mut c = Cursor::new(Bytes::from_vec_aligned(buf));
        let bv2 = BitVector::deserialize_view(&mut c).unwrap();
        let pi2 = PackedInts::deserialize_view(&mut c).unwrap();
        let pfc2 = Pfc::deserialize_view(&mut c).unwrap();
        c.align8().unwrap();
        let wm2 = WaveletMatrix::deserialize_view(&mut c).unwrap();
        assert!(c.is_empty(), "trailing bytes after view deserialize");

        assert_eq!(bv2.len(), bv.len());
        assert_eq!(bv2.count_ones(), bv.count_ones());
        for (i, &bit) in bits.iter().enumerate() {
            assert_eq!(bv2.get(i), bit);
            assert_eq!(bv2.rank1(i), bv.rank1(i));
        }
        assert_eq!(
            pi2.iter().collect::<Vec<_>>(),
            pi.iter().collect::<Vec<_>>()
        );
        for (i, k) in keys.iter().enumerate() {
            assert_eq!(pfc2.get(i).as_deref(), Some(k.as_bytes()));
            assert_eq!(pfc2.locate(k.as_bytes()), Some(i));
        }
        for (i, &v) in values.iter().enumerate() {
            assert_eq!(wm2.access(i), v);
        }
        assert_eq!(wm2.select(13, 2), wm.select(13, 2));
    }

    /// The view path rejects the same malformed input the `Read` path does.
    #[test]
    fn view_deserializers_reject_malformed() {
        use crate::mem::{Bytes, Cursor};

        let bv: BitVector = std::iter::repeat_n(true, 10).collect();

        // Truncated input.
        let mut buf = Vec::new();
        bv.serialize_into(&mut buf).unwrap();
        buf.truncate(buf.len() - 1);
        let mut c = Cursor::new(Bytes::from_vec_aligned(buf));
        assert!(BitVector::deserialize_view(&mut c).is_err());

        // Dirty padding past len.
        let mut buf = Vec::new();
        bv.serialize_into(&mut buf).unwrap();
        let last = buf.len() - 1;
        buf[last] |= 0x80; // bit 63 of the only word, len = 10
        let mut c = Cursor::new(Bytes::from_vec_aligned(buf));
        assert!(BitVector::deserialize_view(&mut c).is_err());
    }
}
