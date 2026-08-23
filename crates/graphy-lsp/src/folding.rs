//! Folding ranges (docs/10 §10, tier 1): collapsible multi-line bracket pairs.
//! Purely lexical — brackets inside strings and comments are never seen because
//! the events come from each language's tokenizer, not a raw byte scan. Folds
//! are computed leniently so a partial/unbalanced document still folds what it
//! can.

use graphy_sparql_syntax::{tokenize_resilient, TokenKind};
use graphy_turtle::{highlight_tokens, HlKind};

use crate::line_index::LineIndex;

/// A collapsible region, `start_line`..=`end_line` (0-based). Maps to an LSP
/// `FoldingRange`; only pairs spanning more than one line are emitted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FoldRange {
    pub start_line: u32,
    pub end_line: u32,
}

/// A bracket occurrence: `open` distinguishes `(`/`[`/`{` from their closers,
/// `group` pairs a closer to its opener (`{}`=0, `[]`=1, `()`=2), `offset` is
/// the byte position.
struct Bracket {
    open: bool,
    group: u8,
    offset: u32,
}

/// Pair brackets into multi-line fold ranges. A closer matches the nearest
/// same-group opener on the stack; openers left unclosed above it are dropped
/// (lenient recovery). Unmatched closers are ignored.
fn fold(src: &str, events: impl Iterator<Item = Bracket>) -> Vec<FoldRange> {
    let li = LineIndex::new(src);
    let mut stack: Vec<(u8, u32)> = Vec::new(); // (group, open line)
    let mut out = Vec::new();
    for ev in events {
        let (line, _) = li.position(src, ev.offset);
        if ev.open {
            stack.push((ev.group, line));
        } else if let Some(pos) = stack.iter().rposition(|&(g, _)| g == ev.group) {
            let (_, open_line) = stack[pos];
            stack.truncate(pos);
            if line > open_line {
                out.push(FoldRange {
                    start_line: open_line,
                    end_line: line,
                });
            }
        }
    }
    out
}

/// Byte → bracket group for `{ } [ ] ( )`, else `None`.
fn bracket_of(byte: u8) -> Option<(bool, u8)> {
    Some(match byte {
        b'{' => (true, 0),
        b'}' => (false, 0),
        b'[' => (true, 1),
        b']' => (false, 1),
        b'(' => (true, 2),
        b')' => (false, 2),
        _ => return None,
    })
}

/// Folding ranges for a Turtle/TriG/N-Triples/N-Quads document.
pub fn turtle_folds(src: &str) -> Vec<FoldRange> {
    let bytes = src.as_bytes();
    let events = highlight_tokens(bytes).into_iter().filter_map(|t| {
        if t.kind != HlKind::Punct {
            return None;
        }
        let (open, group) = bracket_of(bytes[t.start as usize])?;
        Some(Bracket {
            open,
            group,
            offset: t.start,
        })
    });
    fold(src, events)
}

/// Folding ranges for a SPARQL query or update (`{}` groups, `()`, `[]`).
pub fn sparql_folds(src: &str) -> Vec<FoldRange> {
    let events = tokenize_resilient(src).into_iter().filter_map(|t| {
        let (open, group) = match t.kind {
            TokenKind::LBrace | TokenKind::LBraceBar => (true, 0),
            TokenKind::RBrace => (false, 0),
            TokenKind::LBracket => (true, 1),
            TokenKind::RBracket => (false, 1),
            TokenKind::LParen => (true, 2),
            TokenKind::RParen => (false, 2),
            _ => return None,
        };
        Some(Bracket {
            open,
            group,
            offset: t.span.start,
        })
    });
    fold(src, events)
}

/// Folding ranges for a JSON-LD document (`{}` objects, `[]` arrays). Brackets
/// inside strings are skipped by scanning strings whole.
pub fn jsonld_folds(src: &str) -> Vec<FoldRange> {
    let b = src.as_bytes();
    let mut events = Vec::new();
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'"' => i = scan_string_end(b, i),
            byte => {
                if let Some((open, group)) = bracket_of(byte) {
                    if group != 2 {
                        // JSON has no `()`; only fold objects/arrays.
                        events.push(Bracket {
                            open,
                            group,
                            offset: i as u32,
                        });
                    }
                }
                i += 1;
            }
        }
    }
    fold(src, events.into_iter())
}

/// Index one past a JSON string's closing quote (or `b.len()` if unterminated).
fn scan_string_end(b: &[u8], start: usize) -> usize {
    let mut j = start + 1;
    while j < b.len() {
        match b[j] {
            b'\\' => j += 2,
            b'"' => return j + 1,
            _ => j += 1,
        }
    }
    b.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn turtle_folds_blocks_and_lists() {
        let src = "ex:s ex:p [\n  ex:a ex:b\n] ;\n  ex:q (\n  1 2\n) .";
        let folds = turtle_folds(src);
        assert_eq!(
            folds,
            vec![
                FoldRange {
                    start_line: 0,
                    end_line: 2
                }, // [ … ]
                FoldRange {
                    start_line: 3,
                    end_line: 5
                }, // ( … )
            ]
        );
    }

    #[test]
    fn single_line_pairs_do_not_fold() {
        assert!(turtle_folds("ex:s ex:p ( 1 2 3 ) .").is_empty());
    }

    #[test]
    fn brackets_in_strings_are_ignored() {
        // The `}` lives inside a literal and must not close anything.
        let src = "{\nex:s ex:p \"a } b\" .\n}";
        let folds = turtle_folds(src);
        assert_eq!(
            folds,
            vec![FoldRange {
                start_line: 0,
                end_line: 2
            }]
        );
    }

    #[test]
    fn sparql_folds_group_and_call() {
        let src = "SELECT * WHERE {\n  ?s ?p ?o .\n  FILTER(\n    ?o > 1\n  )\n}";
        let folds = sparql_folds(src);
        assert!(folds.contains(&FoldRange {
            start_line: 0,
            end_line: 5
        })); // { … }
        assert!(folds.contains(&FoldRange {
            start_line: 2,
            end_line: 4
        })); // ( … )
    }

    #[test]
    fn jsonld_folds_object_and_array() {
        let src = "{\n  \"@context\": {\n    \"x\": \"y\"\n  },\n  \"list\": [\n    1\n  ]\n}";
        let folds = jsonld_folds(src);
        assert!(folds.contains(&FoldRange {
            start_line: 0,
            end_line: 7
        })); // outer {}
        assert!(folds.contains(&FoldRange {
            start_line: 1,
            end_line: 3
        })); // @context {}
        assert!(folds.contains(&FoldRange {
            start_line: 4,
            end_line: 6
        })); // list []
    }

    #[test]
    fn unbalanced_never_panics() {
        for src in ["{{{", "}}}", "([{", "ex:s [ (", "{\n\"a\":[}"] {
            let _ = turtle_folds(src);
            let _ = sparql_folds(src);
            let _ = jsonld_folds(src);
        }
    }
}
