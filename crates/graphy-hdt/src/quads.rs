//! HDTQ graph annex (qEndpoint dialect): a `MultiRoaringBitmap` — one
//! layer per graph, each a chunked sequence of 32-bit roaring bitmaps over
//! triple indices. Layout pinned from qEndpoint's
//! `compact/bitmap/MultiRoaringBitmap.java`:
//!
//! - header (32 bytes, little-endian): cookie `0x6347008534687532` (u64),
//!   layer count (u32), chunk size in bits (u32, default `1 << 29`),
//!   numbits per layer (u64), layer count again (u64);
//! - blocks: `0x41` (u8), serialized size (u64 LE), layer index (u64 LE),
//!   then the RoaringBitmap in the portable format (what `roaring-rs`
//!   reads/writes natively); chunk order within a layer is ascending;
//! - terminator `0x40`.
//!
//! No CRCs (unlike the core HDT structures). Interop caveat: this matches
//! qEndpoint's current sources; the dialect is not part of the HDT spec.

use roaring::RoaringBitmap;

use crate::codec::{Cur, Out};
use crate::HdtError;

const COOKIE: u64 = 0x6347_0085_3468_7532;
const BLOCK_BITMAP: u8 = 0x41;
const BLOCK_END: u8 = 0x40;
pub(crate) const DEFAULT_CHUNK: u32 = 1 << 29;

/// One roaring bitmap per graph over triple indices.
#[derive(Debug)]
pub(crate) struct GraphAnnex {
    /// Per layer: chunked bitmaps (chunk `c` covers bits
    /// `[c·chunk, (c+1)·chunk)`).
    layers: Vec<Vec<RoaringBitmap>>,
    chunk: u32,
}

impl GraphAnnex {
    pub fn read(c: &mut Cur<'_>) -> Result<GraphAnnex, HdtError> {
        let bad = |m: &str| HdtError::Format(m.to_owned());
        let header = c.take(32)?;
        let u64le = |i: usize| u64::from_le_bytes(header[i..i + 8].try_into().expect("8"));
        let u32le = |i: usize| u32::from_le_bytes(header[i..i + 4].try_into().expect("4"));
        if u64le(0) != COOKIE {
            return Err(bad("bad MultiRoaringBitmap cookie"));
        }
        let n_layers = u32le(8) as usize;
        let chunk = u32le(12);
        let _numbits = u64le(16);
        if u64le(24) != n_layers as u64 {
            return Err(bad("inconsistent layer counts"));
        }
        if chunk == 0 {
            return Err(bad("zero chunk size"));
        }
        let mut layers: Vec<Vec<RoaringBitmap>> = (0..n_layers).map(|_| Vec::new()).collect();
        loop {
            match c.byte()? {
                BLOCK_END => break,
                BLOCK_BITMAP => {
                    let size = u64::from_le_bytes(c.take(8)?.try_into().expect("8")) as usize;
                    let layer = u64::from_le_bytes(c.take(8)?.try_into().expect("8")) as usize;
                    let bytes = c.take(size)?;
                    let bm = RoaringBitmap::deserialize_from(bytes)
                        .map_err(|e| HdtError::Format(format!("bad roaring block: {e}")))?;
                    layers
                        .get_mut(layer)
                        .ok_or_else(|| bad("roaring block layer out of range"))?
                        .push(bm);
                }
                other => {
                    return Err(HdtError::Format(format!(
                        "unknown MultiRoaringBitmap block 0x{other:02x}"
                    )))
                }
            }
        }
        Ok(GraphAnnex { layers, chunk })
    }

    pub fn n_layers(&self) -> usize {
        self.layers.len()
    }

    /// Is triple `k` annotated with graph layer `g`?
    #[inline]
    pub fn get(&self, g: usize, k: u64) -> bool {
        let chunk = u64::from(self.chunk);
        self.layers[g]
            .get((k / chunk) as usize)
            .is_some_and(|bm| bm.contains((k % chunk) as u32))
    }

    /// Write an annex from per-layer sorted triple-index iterators.
    /// `numbits` = the triple count (every layer's bit width).
    pub fn write(out: &mut Out, layers: &[Vec<u64>], numbits: u64) {
        let chunk = u64::from(DEFAULT_CHUNK);
        let n_chunks = numbits.div_ceil(chunk).max(1) as usize;
        out.buf.extend_from_slice(&COOKIE.to_le_bytes());
        out.buf
            .extend_from_slice(&(layers.len() as u32).to_le_bytes());
        out.buf.extend_from_slice(&DEFAULT_CHUNK.to_le_bytes());
        out.buf.extend_from_slice(&numbits.to_le_bytes());
        out.buf
            .extend_from_slice(&(layers.len() as u64).to_le_bytes());
        for (i, bits) in layers.iter().enumerate() {
            let mut chunks: Vec<RoaringBitmap> = vec![RoaringBitmap::new(); n_chunks];
            for &k in bits {
                chunks[(k / chunk) as usize].insert((k % chunk) as u32);
            }
            for bm in &chunks {
                out.buf.push(BLOCK_BITMAP);
                out.buf
                    .extend_from_slice(&(bm.serialized_size() as u64).to_le_bytes());
                out.buf.extend_from_slice(&(i as u64).to_le_bytes());
                bm.serialize_into(&mut out.buf).expect("vec write");
            }
        }
        out.buf.push(BLOCK_END);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn annex_round_trip() {
        let layers: Vec<Vec<u64>> = vec![
            (0..100).filter(|k| k % 3 == 0).collect(),
            (0..100).filter(|k| k % 7 == 1).collect(),
            Vec::new(),
        ];
        let mut out = Out::new();
        GraphAnnex::write(&mut out, &layers, 100);
        let mut c = Cur::new(&out.buf);
        let a = GraphAnnex::read(&mut c).unwrap();
        assert_eq!(c.pos, out.buf.len());
        assert_eq!(a.n_layers(), 3);
        for (g, bits) in layers.iter().enumerate() {
            for k in 0..100u64 {
                assert_eq!(a.get(g, k), bits.contains(&k), "g={g} k={k}");
            }
        }
    }
}
