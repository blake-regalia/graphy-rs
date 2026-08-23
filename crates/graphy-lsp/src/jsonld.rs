//! JSON-LD lexical layer (docs/10 §11).
//!
//! JSON-LD is JSON, so tier-1 highlighting needs only a small resilient JSON
//! scanner plus a keyword-aware classifier — no dependency on `graphy-jsonld`
//! (whose `toRdf`/`@context` semantics arrive with M9 and drive the M11c
//! semantic layer). The scanner never fails: unexpected bytes are skipped and a
//! string left open at EOF still colours as a string.
//!
//! Classification: any string whose content starts with `@` is a JSON-LD
//! keyword (`@context`, `@id`, `@type`, … — and keyword *values* like
//! `"@type": "@id"`); a string in key position (followed by `:`) is a term
//! (`property`); any other string is a value. Structural punctuation gets no
//! token (the editor styles braces); `true`/`false`/`null` are keywords.

use crate::legend::SemKind;
use crate::semantic::{SemBuilder, SemToken};

/// Resolved semantic tokens for a JSON-LD document.
pub fn jsonld_semantic_tokens(src: &str) -> Vec<SemToken> {
    let b = src.as_bytes();
    let mut sb = SemBuilder::new(src);
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b' ' | b'\t' | b'\r' | b'\n' | b'{' | b'}' | b'[' | b']' | b':' | b',' => {
                // Trivia and structural punctuation: no semantic token.
                i += 1;
            }
            b'"' => {
                let (end, terminated) = scan_string(b, i);
                let is_keyword = b.get(i + 1) == Some(&b'@');
                let kind = if is_keyword {
                    SemKind::Keyword
                } else if terminated && next_nonws_is_colon(b, end) {
                    SemKind::Property
                } else {
                    SemKind::String
                };
                sb.push(i as u32, end as u32, kind);
                i = end;
            }
            b'-' | b'0'..=b'9' => {
                let end = scan_number(b, i);
                sb.push(i as u32, end as u32, SemKind::Number);
                i = end;
            }
            b't' | b'f' | b'n' => {
                let end = scan_word(b, i);
                if matches!(&src[i..end], "true" | "false" | "null") {
                    sb.push(i as u32, end as u32, SemKind::Keyword);
                }
                // A non-literal bare word is invalid JSON — emit no token.
                i = end;
            }
            // Any other byte is unexpected; skip it (no token, no panic).
            _ => i += 1,
        }
    }
    sb.finish()
}

/// Scan a `"…"` string starting at the opening quote. Returns the index one
/// past the closing quote (or `b.len()` if unterminated) and whether it closed.
pub(crate) fn scan_string(b: &[u8], start: usize) -> (usize, bool) {
    let mut j = start + 1;
    while j < b.len() {
        match b[j] {
            b'\\' => j += 2, // skip the escaped byte (a `\"` cannot close)
            b'"' => return (j + 1, true),
            _ => j += 1,
        }
    }
    (b.len(), false)
}

/// True if the next non-whitespace byte at/after `from` is `:` (string is a
/// JSON object key).
pub(crate) fn next_nonws_is_colon(b: &[u8], from: usize) -> bool {
    let mut j = from;
    while matches!(b.get(j), Some(b' ' | b'\t' | b'\r' | b'\n')) {
        j += 1;
    }
    b.get(j) == Some(&b':')
}

/// Scan a JSON number starting at `start`. Always consumes ≥1 byte.
fn scan_number(b: &[u8], start: usize) -> usize {
    let mut j = start;
    if b.get(j) == Some(&b'-') {
        j += 1;
    }
    while j < b.len() && b[j].is_ascii_digit() {
        j += 1;
    }
    if b.get(j) == Some(&b'.') {
        j += 1;
        while j < b.len() && b[j].is_ascii_digit() {
            j += 1;
        }
    }
    if matches!(b.get(j), Some(b'e' | b'E')) {
        j += 1;
        if matches!(b.get(j), Some(b'+' | b'-')) {
            j += 1;
        }
        while j < b.len() && b[j].is_ascii_digit() {
            j += 1;
        }
    }
    j.max(start + 1)
}

/// Scan an ASCII-alphabetic word starting at `start`. Always consumes ≥1 byte.
fn scan_word(b: &[u8], start: usize) -> usize {
    let mut j = start;
    while j < b.len() && b[j].is_ascii_alphabetic() {
        j += 1;
    }
    j.max(start + 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn toks(src: &str) -> Vec<(u32, u32, u32, SemKind)> {
        jsonld_semantic_tokens(src)
            .into_iter()
            .map(|t| (t.line, t.start, t.len, t.kind))
            .collect()
    }

    #[test]
    fn keywords_terms_and_values() {
        let src = r#"{"@id": "http://a/x", "name": "Alice", "age": 30}"#;
        let got = toks(src);
        assert_eq!(
            got,
            vec![
                (0, 1, 5, SemKind::Keyword),   // "@id"
                (0, 8, 12, SemKind::String),   // "http://a/x"
                (0, 22, 6, SemKind::Property), // "name"
                (0, 30, 7, SemKind::String),   // "Alice"
                (0, 39, 5, SemKind::Property), // "age"
                (0, 46, 2, SemKind::Number),   // 30
            ]
        );
    }

    #[test]
    fn keyword_valued_context_definition() {
        // "@type": "@id" — both the key and the keyword value colour as Keyword.
        let got = toks(r#"{"@type": "@id"}"#);
        let kw = got.iter().filter(|t| t.3 == SemKind::Keyword).count();
        assert_eq!(kw, 2);
    }

    #[test]
    fn booleans_and_null() {
        let got = toks(r#"{"a": true, "b": false, "c": null}"#);
        let kinds: Vec<_> = got.iter().map(|t| t.3).collect();
        assert!(kinds.iter().filter(|k| **k == SemKind::Keyword).count() == 3);
    }

    #[test]
    fn unterminated_string_stays_a_string() {
        let got = jsonld_semantic_tokens(r#"{"name": "half a val"#);
        assert_eq!(got.last().unwrap().kind, SemKind::String);
    }

    #[test]
    fn garbage_never_panics() {
        for src in [
            "",
            "}}}",
            r#"{"a": @#$%, "b": 1}"#,
            "\u{0}\u{1}\u{2}",
            r#"{"escaped \" quote": "x\\"}"#,
        ] {
            let _ = jsonld_semantic_tokens(src);
        }
    }
}
