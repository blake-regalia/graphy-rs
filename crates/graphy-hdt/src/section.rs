//! HDT structural pieces above the codec: LogSequence2, Bitmap375, and the
//! plain-front-coded dictionary section — read and write, layouts pinned
//! against hdt-cpp output (see codec.rs header).

use crate::codec::{bits_for, bits_get, bits_pack, Cur, Out};
use crate::HdtError;

/// LogSequence2: `[type=1][bits u8][vbyte n][crc8][bitstream][crc32c]`.
#[derive(Debug)]
pub(crate) struct LogSeq<'a> {
    bits: u32,
    pub n: u64,
    data: &'a [u8],
}

impl<'a> LogSeq<'a> {
    pub fn read(c: &mut Cur<'a>) -> Result<LogSeq<'a>, HdtError> {
        let (bits, n) = c.crc8_region(|c| {
            if c.byte()? != 1 {
                return Err(HdtError::Format("bad sequence type".into()));
            }
            let bits = u32::from(c.byte()?);
            let n = c.vbyte()?;
            Ok((bits, n))
        })?;
        if bits > 64 {
            return Err(HdtError::Format(format!("sequence bits {bits} > 64")));
        }
        let data = c.crc32_payload(((n * u64::from(bits)).div_ceil(8)) as usize)?;
        Ok(LogSeq { bits, n, data })
    }

    #[inline]
    pub fn get(&self, k: u64) -> u64 {
        bits_get(self.data, self.bits, k)
    }

    pub fn write(out: &mut Out, values: &[u64]) {
        let bits = bits_for(values.iter().copied().max().unwrap_or(0));
        out.crc8_region(|o| {
            o.buf.push(1);
            o.buf.push(bits as u8);
            o.vbyte(values.len() as u64);
        });
        let payload = bits_pack(values.iter().copied(), bits, values.len() as u64);
        out.crc32_payload(&payload);
    }
}

/// Bitmap375: `[type=1][vbyte nbits][crc8][bitstream][crc32c]`.
#[derive(Debug)]
pub(crate) struct Bitmap<'a> {
    #[allow(dead_code)] // format completeness; readers derive counts elsewhere
    pub nbits: u64,
    data: &'a [u8],
}

impl<'a> Bitmap<'a> {
    pub fn read(c: &mut Cur<'a>) -> Result<Bitmap<'a>, HdtError> {
        let nbits = c.crc8_region(|c| {
            if c.byte()? != 1 {
                return Err(HdtError::Format("bad bitmap type".into()));
            }
            c.vbyte()
        })?;
        let data = c.crc32_payload((nbits.div_ceil(8)) as usize)?;
        Ok(Bitmap { nbits, data })
    }

    #[inline]
    pub fn get(&self, k: u64) -> bool {
        self.data[(k / 8) as usize] >> (k % 8) & 1 != 0
    }

    pub fn write(out: &mut Out, bits: &[bool]) {
        out.crc8_region(|o| {
            o.buf.push(1);
            o.vbyte(bits.len() as u64);
        });
        let mut payload = vec![0u8; bits.len().div_ceil(8)];
        for (k, &b) in bits.iter().enumerate() {
            if b {
                payload[k / 8] |= 1 << (k % 8);
            }
        }
        out.crc32_payload(&payload);
    }
}

/// One plain-front-coded dictionary section:
/// `[type=2][vbyte n][vbyte databytes][vbyte blocksize][crc8]`
/// `[blocks: LogSequence2 of block offsets + end offset][data][crc32c]`.
/// Block content: head string NUL-terminated, then per entry
/// `vbyte(common-prefix-len) + suffix + NUL`.
#[derive(Debug)]
pub(crate) struct PfcSection<'a> {
    pub n: u64,
    blocksize: u64,
    blocks: LogSeq<'a>,
    data: &'a [u8],
}

pub(crate) const BLOCK_SIZE: u64 = 16;

impl<'a> PfcSection<'a> {
    pub fn read(c: &mut Cur<'a>) -> Result<PfcSection<'a>, HdtError> {
        let (n, databytes, blocksize) = c.crc8_region(|c| {
            if c.byte()? != 2 {
                return Err(HdtError::Format(
                    "unsupported dictionary section type".into(),
                ));
            }
            Ok((c.vbyte()?, c.vbyte()?, c.vbyte()?))
        })?;
        let blocks = LogSeq::read(c)?;
        let data = c.crc32_payload(databytes as usize)?;
        Ok(PfcSection {
            n,
            blocksize,
            blocks,
            data,
        })
    }

    /// Decode entry `i` (0-based). Sequential access decodes the block up
    /// to `i`; the reader's iterator walks blocks without re-decoding.
    pub fn get(&self, i: u64) -> Result<Vec<u8>, HdtError> {
        if i >= self.n {
            return Err(HdtError::Format(format!("section index {i} ≥ {}", self.n)));
        }
        let block = i / self.blocksize;
        let mut at = self.blocks.get(block) as usize;
        let bad = || HdtError::Format("corrupt PFC block".into());
        let mut cur: Vec<u8> = Vec::new();
        for k in 0..=(i % self.blocksize) {
            if k == 0 {
                let end = self.data[at..]
                    .iter()
                    .position(|&b| b == 0)
                    .ok_or_else(bad)?;
                cur = self.data[at..at + end].to_vec();
                at += end + 1;
            } else {
                // vbyte common-prefix length, then suffix.
                let mut c = Cur::new(self.data);
                c.pos = at;
                let lcp = c.vbyte()? as usize;
                at = c.pos;
                let end = self.data[at..]
                    .iter()
                    .position(|&b| b == 0)
                    .ok_or_else(bad)?;
                if lcp > cur.len() {
                    return Err(bad());
                }
                cur.truncate(lcp);
                cur.extend_from_slice(&self.data[at..at + end]);
                at += end + 1;
            }
        }
        Ok(cur)
    }

    /// Iterate every entry in order (block-sequential, no re-decoding).
    pub fn iter(&self) -> PfcIter<'a, '_> {
        PfcIter {
            sec: self,
            next: 0,
            at: 0,
            cur: Vec::new(),
        }
    }

    /// Write a section from sorted entries. Returns nothing; the layout is
    /// appended to `out`.
    pub fn write(out: &mut Out, entries: &[Vec<u8>]) {
        let mut data = Vec::new();
        let mut offsets = Vec::new();
        let mut prev: &[u8] = &[];
        for (i, e) in entries.iter().enumerate() {
            if i as u64 % BLOCK_SIZE == 0 {
                offsets.push(data.len() as u64);
                data.extend_from_slice(e);
                data.push(0);
            } else {
                let lcp = prev
                    .iter()
                    .zip(e.iter())
                    .take_while(|(a, b)| a == b)
                    .count();
                let mut o = Out::new();
                o.vbyte(lcp as u64);
                data.extend_from_slice(&o.buf);
                data.extend_from_slice(&e[lcp..]);
                data.push(0);
            }
            prev = e;
        }
        offsets.push(data.len() as u64);

        out.crc8_region(|o| {
            o.buf.push(2);
            o.vbyte(entries.len() as u64);
            o.vbyte(data.len() as u64);
            o.vbyte(BLOCK_SIZE);
        });
        LogSeq::write(out, &offsets);
        out.crc32_payload(&data);
    }
}

pub(crate) struct PfcIter<'a, 's> {
    sec: &'s PfcSection<'a>,
    next: u64,
    at: usize,
    cur: Vec<u8>,
}

impl Iterator for PfcIter<'_, '_> {
    type Item = Result<Vec<u8>, HdtError>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.next >= self.sec.n {
            return None;
        }
        let bad = || HdtError::Format("corrupt PFC block".into());
        let data = self.sec.data;
        let r = (|| {
            if self.next % self.sec.blocksize == 0 {
                self.at = self.sec.blocks.get(self.next / self.sec.blocksize) as usize;
                let end = data[self.at..]
                    .iter()
                    .position(|&b| b == 0)
                    .ok_or_else(bad)?;
                self.cur = data[self.at..self.at + end].to_vec();
                self.at += end + 1;
            } else {
                let mut c = Cur::new(data);
                c.pos = self.at;
                let lcp = c.vbyte()? as usize;
                self.at = c.pos;
                let end = data[self.at..]
                    .iter()
                    .position(|&b| b == 0)
                    .ok_or_else(bad)?;
                if lcp > self.cur.len() {
                    return Err(bad());
                }
                self.cur.truncate(lcp);
                self.cur.extend_from_slice(&data[self.at..self.at + end]);
                self.at += end + 1;
            }
            Ok(self.cur.clone())
        })();
        self.next += 1;
        Some(r)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pfc_round_trip() {
        let entries: Vec<Vec<u8>> = (0..100u32)
            .map(|i| format!("http://example.org/thing/{i:04}").into_bytes())
            .collect();
        let mut sorted = entries.clone();
        sorted.sort();
        let mut out = Out::new();
        PfcSection::write(&mut out, &sorted);
        let mut c = Cur::new(&out.buf);
        let sec = PfcSection::read(&mut c).unwrap();
        assert_eq!(c.pos, out.buf.len());
        assert_eq!(sec.n, 100);
        for (i, e) in sorted.iter().enumerate() {
            assert_eq!(&sec.get(i as u64).unwrap(), e, "entry {i}");
        }
        let walked: Vec<Vec<u8>> = sec.iter().map(|r| r.unwrap()).collect();
        assert_eq!(walked, sorted);
    }

    #[test]
    fn seq_and_bitmap_round_trip() {
        let vals: Vec<u64> = (0..500u64).map(|i| i * 37 % 100_000).collect();
        let mut out = Out::new();
        LogSeq::write(&mut out, &vals);
        let bits: Vec<bool> = (0..500).map(|i| i % 3 == 0).collect();
        Bitmap::write(&mut out, &bits);

        let mut c = Cur::new(&out.buf);
        let seq = LogSeq::read(&mut c).unwrap();
        for (k, &v) in vals.iter().enumerate() {
            assert_eq!(seq.get(k as u64), v);
        }
        let bm = Bitmap::read(&mut c).unwrap();
        for (k, &b) in bits.iter().enumerate() {
            assert_eq!(bm.get(k as u64), b);
        }
        assert_eq!(c.pos, out.buf.len());
    }
}
