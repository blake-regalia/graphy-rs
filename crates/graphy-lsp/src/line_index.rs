//! Byte-offset ↔ LSP position mapping (docs/10 §6.2).
//!
//! LSP positions are `(line, character)` where `line` is 0-based and
//! `character` counts **UTF-16 code units** within the line — not bytes, not
//! Unicode scalar values. Getting this wrong shifts every downstream span, so
//! the conversion lives in one tested place. The index is built once per
//! document version.

/// Line-start byte offsets for a document, enabling O(log n) byte → position.
#[derive(Debug, Clone)]
pub struct LineIndex {
    /// Byte offset of the start of each line. Always begins with `0`.
    starts: Vec<u32>,
}

impl LineIndex {
    /// Build the index for `src`.
    pub fn new(src: &str) -> LineIndex {
        let mut starts = vec![0u32];
        for (i, b) in src.bytes().enumerate() {
            if b == b'\n' {
                starts.push((i + 1) as u32);
            }
        }
        LineIndex { starts }
    }

    /// 0-based line containing `byte` (a byte at a line start belongs to that
    /// line; `byte == src.len()` belongs to the last line).
    fn line_of(&self, byte: u32) -> usize {
        match self.starts.binary_search(&byte) {
            Ok(i) => i,
            Err(i) => i - 1,
        }
    }

    /// The `(line, character)` LSP position for a byte offset. `byte` must be a
    /// char boundary within `src` (token spans always are).
    pub fn position(&self, src: &str, byte: u32) -> (u32, u32) {
        let line = self.line_of(byte);
        let line_start = self.starts[line] as usize;
        let col = utf16_len(&src[line_start..byte as usize]);
        (line as u32, col)
    }
}

/// Length of `s` in UTF-16 code units (astral-plane scalars count as 2).
pub fn utf16_len(s: &str) -> u32 {
    s.chars().map(|c| c.len_utf16() as u32).sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ascii_positions() {
        let src = "abc\ndef\n\ngh";
        let li = LineIndex::new(src);
        assert_eq!(li.position(src, 0), (0, 0));
        assert_eq!(li.position(src, 2), (0, 2));
        assert_eq!(li.position(src, 4), (1, 0)); // 'd'
        assert_eq!(li.position(src, 8), (2, 0)); // blank line
        assert_eq!(li.position(src, 9), (3, 0)); // 'g'
    }

    #[test]
    fn utf16_columns_count_code_units() {
        // "é" is 2 bytes / 1 UTF-16 unit; "😀" is 4 bytes / 2 UTF-16 units.
        let src = "é😀x";
        let li = LineIndex::new(src);
        let x_byte = src.find('x').unwrap() as u32;
        assert_eq!(li.position(src, x_byte), (0, 3)); // 1 + 2
        assert_eq!(utf16_len("😀"), 2);
        assert_eq!(utf16_len("é"), 1);
    }
}
