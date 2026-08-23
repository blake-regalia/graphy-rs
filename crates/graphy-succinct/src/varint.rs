//! LEB128 unsigned varints, used inside PFC blocks (graphy-core has its own
//! private copy; the encoding must stay byte-compatible with nothing — each
//! crate's varints are internal to its own containers).

pub fn write(buf: &mut Vec<u8>, mut v: u64) {
    loop {
        let byte = (v & 0x7F) as u8;
        v >>= 7;
        if v == 0 {
            buf.push(byte);
            return;
        }
        buf.push(byte | 0x80);
    }
}

/// Returns (value, bytes consumed), or `None` on truncation/overflow.
pub fn read(buf: &[u8]) -> Option<(u64, usize)> {
    let mut v: u64 = 0;
    for (i, &b) in buf.iter().enumerate().take(10) {
        v |= u64::from(b & 0x7F) << (7 * i);
        if b & 0x80 == 0 {
            // Reject a 10th byte overflowing 64 bits.
            if i == 9 && b > 1 {
                return None;
            }
            return Some((v, i + 1));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        let mut buf = Vec::new();
        for v in [0u64, 1, 127, 128, 300, u32::MAX as u64, u64::MAX] {
            buf.clear();
            write(&mut buf, v);
            assert_eq!(read(&buf), Some((v, buf.len())));
        }
    }

    #[test]
    fn rejects_bad_input() {
        assert_eq!(read(&[]), None);
        assert_eq!(read(&[0x80]), None); // truncated
        assert_eq!(read(&[0xFF; 10]), None); // continuation forever
        let mut buf = vec![0xFF; 9];
        buf.push(0x02); // 10th byte overflows 64 bits
        assert_eq!(read(&buf), None);
    }
}
