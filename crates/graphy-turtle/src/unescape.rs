//! Escape-sequence decoding: `\uXXXX` / `\UXXXXXXXX` (UCHAR, used in strings
//! and IRIREFs) and the single-character string escapes (ECHAR). Decoding is
//! lazy — the lexer only calls in here after spotting a backslash.

/// Decode the UCHAR starting at `bytes[at]` (which must be `\`). Returns the
/// scalar value and total bytes consumed, or `None` for malformed input
/// (bad hex, surrogate, out of range, truncation).
pub(crate) fn decode_uchar(bytes: &[u8], at: usize) -> Option<(char, usize)> {
    let n = match bytes.get(at + 1)? {
        b'u' => 4,
        b'U' => 8,
        _ => return None,
    };
    let hex = bytes.get(at + 2..at + 2 + n)?;
    let mut v: u32 = 0;
    for &b in hex {
        v = v * 16 + (b as char).to_digit(16)?;
    }
    char::from_u32(v).map(|c| (c, 2 + n))
}

/// Decode the ECHAR (`\t \b \n \r \f \" \' \\`) at `bytes[at]` (a `\`).
pub(crate) fn decode_echar(bytes: &[u8], at: usize) -> Option<(char, usize)> {
    let c = match bytes.get(at + 1)? {
        b't' => '\t',
        b'b' => '\u{08}',
        b'n' => '\n',
        b'r' => '\r',
        b'f' => '\u{0C}',
        b'"' => '"',
        b'\'' => '\'',
        b'\\' => '\\',
        _ => return None,
    };
    Some((c, 2))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uchars() {
        assert_eq!(decode_uchar(b"\\u0041", 0), Some(('A', 6)));
        assert_eq!(decode_uchar(br"\U0001F600", 0), Some(('😀', 10)));
        assert_eq!(decode_uchar(br"\uD800", 0), None); // surrogate
        assert_eq!(decode_uchar(br"\uZZZZ", 0), None);
        assert_eq!(decode_uchar(br"\u004", 0), None); // truncated
        assert_eq!(decode_uchar(br"\x41", 0), None);
    }

    #[test]
    fn echars() {
        assert_eq!(decode_echar(br"\n", 0), Some(('\n', 2)));
        assert_eq!(decode_echar(br"\'", 0), Some(('\'', 2)));
        assert_eq!(decode_echar(br"\z", 0), None);
    }
}
