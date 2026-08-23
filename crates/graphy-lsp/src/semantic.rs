//! Semantic tokens: turn a language's lexical highlight stream into the LSP
//! relative-delta encoding (docs/10 §7).
//!
//! Tier 1 of the two-tier design — it consults only the resilient lexer, never
//! the parser, so highlighting never regresses on a broken buffer. Tokens are
//! split at line boundaries (a single LSP semantic token may not span lines)
//! and positions are UTF-16, via [`LineIndex`].

use graphy_turtle::{highlight_tokens, HlKind};

use crate::legend::SemKind;
use crate::line_index::{utf16_len, LineIndex};

/// A resolved semantic token in LSP coordinates (0-based line, UTF-16
/// character, UTF-16 length). Never spans a newline.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SemToken {
    pub line: u32,
    pub start: u32,
    pub len: u32,
    pub kind: SemKind,
    pub mods: u32,
}

/// Map a Turtle/TriG/N-Triples/N-Quads highlight class to a semantic-token
/// type. `None` = emit no token (punctuation is left to the editor's default;
/// error runs get a diagnostic squiggle, not a colour). These are position-free
/// lexical defaults — the parser tier refines predicate-vs-object later
/// (docs/10 §7.1).
pub fn turtle_sem_kind(k: HlKind) -> Option<SemKind> {
    Some(match k {
        HlKind::Iri => SemKind::Class,
        HlKind::PrefixName => SemKind::Namespace,
        HlKind::LocalName => SemKind::Property,
        HlKind::BlankNode => SemKind::Variable,
        HlKind::String => SemKind::String,
        HlKind::LangTag => SemKind::Decorator,
        HlKind::Number => SemKind::Number,
        HlKind::Boolean | HlKind::Keyword => SemKind::Keyword,
        HlKind::Operator => SemKind::Operator,
        HlKind::Punct | HlKind::Error => return None,
    })
}

/// Accumulates resolved [`SemToken`]s from byte spans, handling the two
/// coordinate concerns every language shares: line-boundary splitting (an LSP
/// semantic token may not span a newline) and byte → UTF-16 conversion. Shared
/// by the per-language adapters.
pub(crate) struct SemBuilder<'a> {
    src: &'a str,
    li: LineIndex,
    out: Vec<SemToken>,
}

impl<'a> SemBuilder<'a> {
    pub(crate) fn new(src: &'a str) -> SemBuilder<'a> {
        SemBuilder {
            src,
            li: LineIndex::new(src),
            out: Vec::new(),
        }
    }

    /// Add the byte span `[start, end)` as `kind`, splitting at any newline it
    /// covers (newlines are char boundaries, so every split point is valid).
    /// The `\r` of a CRLF is part of the terminator, not the segment.
    pub(crate) fn push(&mut self, start: u32, end: u32, kind: SemKind) {
        let bytes = self.src.as_bytes();
        let mut seg = start;
        for (i, &b) in bytes[start as usize..end as usize].iter().enumerate() {
            if b == b'\n' {
                let nl = start + i as u32;
                let stop = if nl > seg && bytes[nl as usize - 1] == b'\r' {
                    nl - 1
                } else {
                    nl
                };
                self.emit(seg, stop, kind);
                seg = nl + 1;
            }
        }
        self.emit(seg, end, kind);
    }

    fn emit(&mut self, start: u32, end: u32, kind: SemKind) {
        if end <= start {
            return;
        }
        let (line, col) = self.li.position(self.src, start);
        let len = utf16_len(&self.src[start as usize..end as usize]);
        if len == 0 {
            return;
        }
        self.out.push(SemToken {
            line,
            start: col,
            len,
            kind,
            mods: 0,
        });
    }

    pub(crate) fn finish(self) -> Vec<SemToken> {
        self.out
    }
}

/// Split a `prefix:local` span at the first `:` — the namespace/local boundary
/// (the prefix part carries no colon) — into a namespace span and, if the local
/// part is non-empty, a local span. Shared by the Turtle and SPARQL adapters.
pub(crate) fn push_prefixed_name(b: &mut SemBuilder, src: &str, start: u32, end: u32) {
    let slice = &src.as_bytes()[start as usize..end as usize];
    let colon = slice
        .iter()
        .position(|&c| c == b':')
        .map(|i| start + i as u32)
        .unwrap_or(end);
    b.push(start, colon + 1, SemKind::Namespace);
    if colon + 1 < end {
        b.push(colon + 1, end, SemKind::Property);
    }
}

/// Resolved semantic tokens for a Turtle-family document.
pub fn turtle_semantic_tokens(src: &str) -> Vec<SemToken> {
    let mut b = SemBuilder::new(src);
    for t in highlight_tokens(src.as_bytes()) {
        if let Some(kind) = turtle_sem_kind(t.kind) {
            b.push(t.start, t.end, kind);
        }
    }
    b.finish()
}

/// Encode resolved tokens as the LSP `SemanticTokens.data` flat array — five
/// `u32`s per token, each field relative to the previous token (docs/10 §7.2).
/// Input must be ordered by (line, start), which the builders guarantee.
pub fn encode(toks: &[SemToken]) -> Vec<u32> {
    let mut data = Vec::with_capacity(toks.len() * 5);
    let (mut pline, mut pstart) = (0u32, 0u32);
    for t in toks {
        // The relative encoding underflows on out-of-order input; the builders
        // emit in span order, so this only fires on a broken adapter.
        debug_assert!(
            (t.line, t.start) >= (pline, pstart) || data.is_empty(),
            "semantic tokens out of order at {}:{}",
            t.line,
            t.start
        );
        let dline = t.line - pline;
        let dstart = if dline == 0 {
            t.start - pstart
        } else {
            t.start
        };
        data.extend_from_slice(&[dline, dstart, t.len, t.kind.index(), t.mods]);
        pline = t.line;
        pstart = t.start;
    }
    data
}

#[cfg(test)]
mod tests {
    use super::*;

    fn toks(src: &str) -> Vec<(u32, u32, u32, SemKind)> {
        turtle_semantic_tokens(src)
            .into_iter()
            .map(|t| (t.line, t.start, t.len, t.kind))
            .collect()
    }

    #[test]
    fn classifies_and_positions() {
        let got = toks("@prefix ex: <http://e/> .\nex:s ex:p 42 .");
        assert_eq!(
            got,
            vec![
                (0, 0, 7, SemKind::Keyword),   // @prefix
                (0, 8, 3, SemKind::Namespace), // ex:
                (0, 12, 11, SemKind::Class),   // <http://e/>
                // '.' is punctuation -> no token
                (1, 0, 3, SemKind::Namespace), // ex:
                (1, 3, 1, SemKind::Property),  // s
                (1, 5, 3, SemKind::Namespace), // ex:
                (1, 8, 1, SemKind::Property),  // p
                (1, 10, 2, SemKind::Number),   // 42
            ]
        );
    }

    #[test]
    fn long_string_splits_across_lines() {
        let src = ":s :p \"\"\"one\ntwo\"\"\" .";
        let got = toks(src);
        // The triple-quoted literal spans two lines -> two String tokens.
        let strings: Vec<_> = got.iter().filter(|t| t.3 == SemKind::String).collect();
        assert_eq!(strings.len(), 2);
        assert_eq!(strings[0].0, 0); // first line
        assert_eq!(strings[1].0, 1); // second line
    }

    #[test]
    fn crlf_split_excludes_the_carriage_return() {
        // Same literal with CRLF endings: the first segment stops before the
        // `\r` instead of highlighting a stray extra column.
        let got = toks(":s :p \"\"\"one\r\ntwo\"\"\" .");
        let strings: Vec<_> = got.iter().filter(|t| t.3 == SemKind::String).collect();
        assert_eq!(strings[0].2, 6); // `"""one` — no \r
        assert_eq!(strings[1].0, 1);
    }

    #[test]
    fn utf16_positions_in_literals() {
        // Emoji before the object shifts the object's UTF-16 column by 2.
        let src = ":s :p \"😀x\" .";
        let got = turtle_semantic_tokens(src);
        let string = got.iter().find(|t| t.kind == SemKind::String).unwrap();
        // "😀x" = quote(1) + 😀(2) + x(1) + quote(1) = 5 UTF-16 units.
        assert_eq!(string.len, 5);
    }

    #[test]
    fn delta_encoding_is_relative() {
        let src = "ex:s ex:p ex:o .";
        let data = encode(&turtle_semantic_tokens(src));
        // First token (ex:) : deltaLine 0, deltaStart 0, len 3, namespace(0).
        assert_eq!(&data[0..5], &[0, 0, 3, SemKind::Namespace.index(), 0]);
        // Second (s): same line, deltaStart from col 0 -> 3.
        assert_eq!(&data[5..10], &[0, 3, 1, SemKind::Property.index(), 0]);
    }

    #[test]
    fn valid_docs_never_panic_and_stay_in_bounds() {
        for doc in [
            "<http://a/s> <http://a/p> <http://a/o> .",
            "@prefix : <http://e/> .\n:s :p :o , :o2 ; :q [ :r :s ] .",
            "PREFIX ex: <http://e/>\nex:s ex:p 1, 2.0, true .",
            ":s :p \"chat\"@fr-be , \"dir\"@en--ltr .",
        ] {
            let ts = turtle_semantic_tokens(doc);
            for t in &ts {
                assert!(t.len > 0);
            }
            let _ = encode(&ts);
        }
    }
}
