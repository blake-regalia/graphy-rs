//! HDT binary primitives, pinned empirically against an hdt-cpp-produced
//! file (dbpedia.hdt from the hdt-java test suite; every value below was
//! verified byte-for-byte — see BENCHMARKS.md M5 hdt addendum):
//!
//! - **vbyte**: 7-bit little-endian groups, HIGH bit set on the LAST byte.
//! - **CRC8**: poly 0x07, init 0, not reflected (plain CRC-8).
//! - **CRC16**: ARC — poly 0x8005 reflected (0xA001), init 0.
//! - **CRC32**: Castagnoli — poly 0x1EDC6F41 reflected (0x82F63B78),
//!   init/xorout 0xFFFFFFFF.
//! - **ControlInformation**: `$HDT` cookie, type byte, NUL-terminated
//!   format string, NUL-terminated `k=v;` properties, CRC16 (LE) over
//!   everything preceding it.
//! - Bit sequences are byte-aligned little-endian bitstreams, LSB first.

use crate::HdtError;

// ---------------------------------------------------------------- CRCs

fn crc8(data: &[u8]) -> u8 {
    let mut crc = 0u8;
    for &b in data {
        crc ^= b;
        for _ in 0..8 {
            crc = if crc & 0x80 != 0 {
                (crc << 1) ^ 0x07
            } else {
                crc << 1
            };
        }
    }
    crc
}

fn crc16(data: &[u8]) -> u16 {
    let mut crc = 0u16;
    for &b in data {
        crc ^= u16::from(b);
        for _ in 0..8 {
            crc = if crc & 1 != 0 {
                (crc >> 1) ^ 0xA001
            } else {
                crc >> 1
            };
        }
    }
    crc
}

fn crc32c(data: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for &b in data {
        crc ^= u32::from(b);
        for _ in 0..8 {
            crc = if crc & 1 != 0 {
                (crc >> 1) ^ 0x82F6_3B78
            } else {
                crc >> 1
            };
        }
    }
    crc ^ 0xFFFF_FFFF
}

// ------------------------------------------------------------- reading

/// Cursor over an HDT byte buffer with CRC-checked region reads.
pub(crate) struct Cur<'a> {
    pub data: &'a [u8],
    pub pos: usize,
}

impl<'a> Cur<'a> {
    pub fn new(data: &'a [u8]) -> Cur<'a> {
        Cur { data, pos: 0 }
    }

    fn bad(&self, m: &str) -> HdtError {
        HdtError::Format(format!("{m} at byte {}", self.pos))
    }

    pub fn byte(&mut self) -> Result<u8, HdtError> {
        let b = *self
            .data
            .get(self.pos)
            .ok_or_else(|| self.bad("unexpected EOF"))?;
        self.pos += 1;
        Ok(b)
    }

    pub fn take(&mut self, n: usize) -> Result<&'a [u8], HdtError> {
        let end = self
            .pos
            .checked_add(n)
            .filter(|&e| e <= self.data.len())
            .ok_or_else(|| self.bad("unexpected EOF"))?;
        let s = &self.data[self.pos..end];
        self.pos = end;
        Ok(s)
    }

    pub fn vbyte(&mut self) -> Result<u64, HdtError> {
        let mut v = 0u64;
        let mut shift = 0u32;
        loop {
            let b = self.byte()?;
            if shift >= 63 {
                return Err(self.bad("vbyte overflow"));
            }
            v |= u64::from(b & 0x7F) << shift;
            shift += 7;
            if b & 0x80 != 0 {
                return Ok(v);
            }
        }
    }

    fn cstring(&mut self) -> Result<&'a str, HdtError> {
        let start = self.pos;
        let end = self.data[start..]
            .iter()
            .position(|&b| b == 0)
            .map(|i| start + i)
            .ok_or_else(|| self.bad("unterminated string"))?;
        let s = std::str::from_utf8(&self.data[start..end])
            .map_err(|_| self.bad("non-UTF-8 string"))?;
        self.pos = end + 1;
        Ok(s)
    }

    /// Read a ControlInformation block; returns (type, format, properties).
    pub fn control_info(&mut self) -> Result<(u8, &'a str, Props<'a>), HdtError> {
        let start = self.pos;
        if self.take(4)? != b"$HDT" {
            return Err(HdtError::Format(format!(
                "missing $HDT cookie at byte {start}"
            )));
        }
        let ty = self.byte()?;
        let fmt = self.cstring()?;
        let props = self.cstring()?;
        let crc_region = &self.data[start..self.pos];
        let want = u16::from_le_bytes(self.take(2)?.try_into().expect("2 bytes"));
        if crc16(crc_region) != want {
            return Err(self.bad("control-information CRC16 mismatch"));
        }
        Ok((ty, fmt, Props(props)))
    }

    /// Header region: type byte + vbyte fields + CRC8 check. Returns the
    /// header fields' raw region for the caller to parse via a fresh read;
    /// simplest is the closure form below.
    pub fn crc8_region<T>(
        &mut self,
        read: impl FnOnce(&mut Cur<'a>) -> Result<T, HdtError>,
    ) -> Result<T, HdtError> {
        let start = self.pos;
        let out = read(self)?;
        let region = &self.data[start..self.pos];
        let want = self.byte()?;
        if crc8(region) != want {
            return Err(self.bad("header CRC8 mismatch"));
        }
        Ok(out)
    }

    /// Take `n` payload bytes followed by their CRC32C.
    pub fn crc32_payload(&mut self, n: usize) -> Result<&'a [u8], HdtError> {
        let payload = self.take(n)?;
        let want = u32::from_le_bytes(self.take(4)?.try_into().expect("4 bytes"));
        if crc32c(payload) != want {
            return Err(self.bad("payload CRC32C mismatch"));
        }
        Ok(payload)
    }
}

/// `key=value;` properties of a control-information block.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Props<'a>(&'a str);

impl<'a> Props<'a> {
    pub fn get(&self, key: &str) -> Option<&'a str> {
        self.0
            .split(';')
            .filter_map(|kv| kv.split_once('='))
            .find(|(k, _)| *k == key)
            .map(|(_, v)| v)
    }
}

// ------------------------------------------------------------- writing

/// Append-only writer mirroring [`Cur`]'s framing.
pub(crate) struct Out {
    pub buf: Vec<u8>,
}

impl Out {
    pub fn new() -> Out {
        Out { buf: Vec::new() }
    }

    pub fn vbyte(&mut self, mut v: u64) {
        loop {
            let b = (v & 0x7F) as u8;
            v >>= 7;
            if v == 0 {
                self.buf.push(b | 0x80);
                return;
            }
            self.buf.push(b);
        }
    }

    pub fn control_info(&mut self, ty: u8, format: &str, props: &str) {
        let start = self.buf.len();
        self.buf.extend_from_slice(b"$HDT");
        self.buf.push(ty);
        self.buf.extend_from_slice(format.as_bytes());
        self.buf.push(0);
        self.buf.extend_from_slice(props.as_bytes());
        self.buf.push(0);
        let crc = crc16(&self.buf[start..]);
        self.buf.extend_from_slice(&crc.to_le_bytes());
    }

    /// Write a header region via `write`, then its CRC8.
    pub fn crc8_region(&mut self, write: impl FnOnce(&mut Out)) {
        let start = self.buf.len();
        write(self);
        let crc = crc8(&self.buf[start..]);
        self.buf.push(crc);
    }

    /// Append `payload` followed by its CRC32C.
    pub fn crc32_payload(&mut self, payload: &[u8]) {
        self.buf.extend_from_slice(payload);
        self.buf.extend_from_slice(&crc32c(payload).to_le_bytes());
    }
}

// --------------------------------------------------- packed bitstreams

/// Read the `k`-th `w`-bit value of a little-endian LSB-first bitstream.
#[inline]
pub(crate) fn bits_get(buf: &[u8], w: u32, k: u64) -> u64 {
    let mut v = 0u64;
    let base = k * u64::from(w);
    for j in 0..u64::from(w) {
        let bit = base + j;
        if buf[(bit / 8) as usize] >> (bit % 8) & 1 != 0 {
            v |= 1 << j;
        }
    }
    v
}

/// Append `w`-bit values into a little-endian LSB-first bitstream.
pub(crate) fn bits_pack(values: impl Iterator<Item = u64>, w: u32, n: u64) -> Vec<u8> {
    let mut buf = vec![0u8; ((n * u64::from(w)).div_ceil(8)) as usize];
    let mut base = 0u64;
    for v in values {
        for j in 0..u64::from(w) {
            if v >> j & 1 != 0 {
                let bit = base + j;
                buf[(bit / 8) as usize] |= 1 << (bit % 8);
            }
        }
        base += u64::from(w);
    }
    buf
}

/// Bits needed for `max` (≥ 1 so zero-width sequences never occur).
pub(crate) fn bits_for(max: u64) -> u32 {
    (64 - max.leading_zeros()).max(1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc_vectors() {
        // "123456789" check values for the three algorithms.
        let d = b"123456789";
        assert_eq!(crc8(d), 0xF4); // CRC-8 (poly 0x07)
        assert_eq!(crc16(d), 0xBB3D); // CRC-16/ARC
        assert_eq!(crc32c(d), 0xE306_9283); // CRC-32C
    }

    #[test]
    fn vbyte_round_trip() {
        for v in [0u64, 1, 127, 128, 300, 4011, u32::MAX as u64, 1 << 55] {
            let mut o = Out::new();
            o.vbyte(v);
            let mut c = Cur::new(&o.buf);
            assert_eq!(c.vbyte().unwrap(), v);
            assert_eq!(c.pos, o.buf.len());
        }
    }

    #[test]
    fn bitstream_round_trip() {
        for w in [1u32, 2, 7, 19, 33, 64] {
            let vals: Vec<u64> = (0..50u64)
                .map(|i| i.wrapping_mul(0x9E37_79B9_7F4A_7C15) & (((1u128 << w) - 1) as u64))
                .collect();
            let buf = bits_pack(vals.iter().copied(), w, vals.len() as u64);
            for (k, &v) in vals.iter().enumerate() {
                assert_eq!(bits_get(&buf, w, k as u64), v, "w={w} k={k}");
            }
        }
    }

    #[test]
    fn control_info_round_trip() {
        let mut o = Out::new();
        o.control_info(3, "<http://purl.org/HDT/hdt#dictionaryFour>", "mapping=1;");
        let mut c = Cur::new(&o.buf);
        let (ty, fmt, props) = c.control_info().unwrap();
        assert_eq!(ty, 3);
        assert_eq!(fmt, "<http://purl.org/HDT/hdt#dictionaryFour>");
        assert_eq!(props.get("mapping"), Some("1"));
    }
}
