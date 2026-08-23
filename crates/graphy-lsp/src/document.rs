//! Open-document state and incremental sync (docs/10 §6).
//!
//! Text is held in a [`ropey::Rope`] for O(log n) edits and UTF-16 ↔ char
//! conversion. LSP positions are `(line, UTF-16 character)`; all of that
//! conversion lives here so the rest of the server works in whole-document
//! strings.

use std::collections::HashMap;

use lsp_types::{Position, TextDocumentContentChangeEvent, Uri};
use ropey::Rope;

/// Which grammar a document uses. N-Triples/N-Quads/Turtle/TriG all share the
/// Turtle-family tokenizer, so they collapse to one variant here (docs/10 §8,
/// Q4: distinct editor ids, one internal handler).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lang {
    Turtle,
    Sparql,
    JsonLd,
}

impl Lang {
    /// Resolve a language from the LSP `languageId`, falling back to the URI's
    /// file extension, then to Turtle (the most permissive text format).
    pub fn detect(language_id: &str, uri: &Uri) -> Lang {
        match language_id {
            "turtle" | "ntriples" | "n-triples" | "nquads" | "n-quads" | "trig" => {
                return Lang::Turtle
            }
            "sparql" => return Lang::Sparql,
            // Plain "json" too: an editor without a JSON-LD grammar installed
            // reports .jsonld files as json, and JSON-LD is the only JSON
            // dialect this server speaks.
            "jsonld" | "json-ld" | "json" => return Lang::JsonLd,
            _ => {}
        }
        match extension(uri).as_deref() {
            Some("ttl" | "nt" | "nq" | "trig") => Lang::Turtle,
            Some("rq" | "ru" | "sparql") => Lang::Sparql,
            Some("jsonld" | "json") => Lang::JsonLd,
            _ => Lang::Turtle,
        }
    }
}

/// Lowercased file extension of a document URI, if any.
fn extension(uri: &Uri) -> Option<String> {
    let name = uri.path().as_str().rsplit('/').next()?;
    let dot = name.rfind('.')?;
    Some(name[dot + 1..].to_ascii_lowercase())
}

/// One open document.
#[derive(Debug)]
pub struct Doc {
    pub rope: Rope,
    pub version: i32,
    pub lang: Lang,
}

impl Doc {
    pub fn new(text: &str, version: i32, lang: Lang) -> Doc {
        Doc {
            rope: Rope::from_str(text),
            version,
            lang,
        }
    }

    /// The whole document as a string (input to the tier-1 analyzers).
    pub fn text(&self) -> String {
        self.rope.to_string()
    }

    /// Apply one LSP content change. A change with no range is a full replace;
    /// otherwise the range is spliced out and the new text inserted.
    pub fn apply(&mut self, change: TextDocumentContentChangeEvent) {
        match change.range {
            None => self.rope = Rope::from_str(&change.text),
            Some(range) => {
                let start = self.position_to_char(range.start);
                let end = self.position_to_char(range.end);
                if start <= end && end <= self.rope.len_chars() {
                    self.rope.remove(start..end);
                    self.rope.insert(start, &change.text);
                }
            }
        }
    }

    /// The byte offset of an LSP position in [`Self::text`] (same clamping
    /// rules as [`Self::apply`]); for feature requests carrying a cursor.
    pub fn byte_of(&self, pos: Position) -> usize {
        self.rope.char_to_byte(self.position_to_char(pos))
    }

    /// Convert an LSP `(line, UTF-16 character)` position to a rope char index.
    /// Out-of-range positions clamp per the spec: a line past the document maps
    /// to the document end, and a character past the line end defaults to the
    /// line *length* — before the line terminator, so an exaggerated end column
    /// never swallows the newline.
    fn position_to_char(&self, pos: Position) -> usize {
        let rope = &self.rope;
        let line = pos.line as usize;
        if line >= rope.len_lines() {
            return rope.len_chars();
        }
        let line_start = rope.line_to_char(line);
        let line_end = if line + 1 < rope.len_lines() {
            // Exclude the terminator: the '\n', plus the '\r' of a CRLF.
            let mut e = rope.line_to_char(line + 1) - 1;
            if e > line_start && rope.char(e - 1) == '\r' {
                e -= 1;
            }
            e
        } else {
            rope.len_chars()
        };
        let start_u16 = rope.char_to_utf16_cu(line_start);
        let end_u16 = rope.char_to_utf16_cu(line_end);
        let target = (start_u16 + pos.character as usize).min(end_u16);
        rope.utf16_cu_to_char(target)
    }
}

/// The set of open documents, keyed by URI.
#[derive(Debug, Default)]
pub struct DocStore {
    docs: HashMap<Uri, Doc>,
}

impl DocStore {
    pub fn open(&mut self, uri: Uri, doc: Doc) {
        self.docs.insert(uri, doc);
    }

    pub fn close(&mut self, uri: &Uri) {
        self.docs.remove(uri);
    }

    pub fn get(&self, uri: &Uri) -> Option<&Doc> {
        self.docs.get(uri)
    }

    pub fn get_mut(&mut self, uri: &Uri) -> Option<&mut Doc> {
        self.docs.get_mut(uri)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lsp_types::Range;
    use std::str::FromStr;

    fn url(s: &str) -> Uri {
        Uri::from_str(s).unwrap()
    }

    fn pos(line: u32, ch: u32) -> Position {
        Position::new(line, ch)
    }

    #[test]
    fn language_detection() {
        let u = url("file:///x.unknown");
        assert_eq!(Lang::detect("sparql", &u), Lang::Sparql);
        assert_eq!(Lang::detect("nquads", &u), Lang::Turtle);
        assert_eq!(Lang::detect("", &url("file:///q.rq")), Lang::Sparql);
        assert_eq!(Lang::detect("", &url("file:///d.jsonld")), Lang::JsonLd);
        assert_eq!(Lang::detect("json", &u), Lang::JsonLd);
        assert_eq!(Lang::detect("", &url("file:///d.json")), Lang::JsonLd);
        assert_eq!(Lang::detect("", &url("file:///d.ttl")), Lang::Turtle);
        assert_eq!(Lang::detect("", &url("file:///d.weird")), Lang::Turtle);
    }

    fn change(range: Option<Range>, text: &str) -> TextDocumentContentChangeEvent {
        TextDocumentContentChangeEvent {
            range,
            range_length: None,
            text: text.to_string(),
        }
    }

    #[test]
    fn incremental_insert_and_delete() {
        let mut doc = Doc::new("ex:s ex:p ex:o .", 1, Lang::Turtle);
        // Insert "X" after "ex:s" (line 0, char 4).
        doc.apply(change(Some(Range::new(pos(0, 4), pos(0, 4))), "X"));
        assert_eq!(doc.text(), "ex:sX ex:p ex:o .");
        // Delete the "X" back out.
        doc.apply(change(Some(Range::new(pos(0, 4), pos(0, 5))), ""));
        assert_eq!(doc.text(), "ex:s ex:p ex:o .");
    }

    #[test]
    fn multiline_replace() {
        let mut doc = Doc::new("line0\nline1\nline2", 1, Lang::Turtle);
        // Replace from (0,4) through (2,4) with "Z".
        doc.apply(change(Some(Range::new(pos(0, 4), pos(2, 4))), "Z"));
        assert_eq!(doc.text(), "lineZ2");
    }

    #[test]
    fn utf16_aware_edit_after_astral_char() {
        // "😀" is one char / two UTF-16 code units. An edit at UTF-16 column 3
        // lands right after the "😀x", i.e. before the space.
        let mut doc = Doc::new("😀x y", 1, Lang::Turtle);
        doc.apply(change(Some(Range::new(pos(0, 3), pos(0, 3))), "!"));
        assert_eq!(doc.text(), "😀x! y");
    }

    #[test]
    fn overflow_character_clamps_before_the_newline() {
        // Deleting (0,0)..(0,999) must clear line 0's content but keep the
        // line break (LSP: character past line length = line length).
        let mut doc = Doc::new("ab\ncd", 1, Lang::Turtle);
        doc.apply(change(Some(Range::new(pos(0, 0), pos(0, 999))), ""));
        assert_eq!(doc.text(), "\ncd");
        // Insert at an exaggerated column: end of line content, not next line.
        let mut doc = Doc::new("ab\r\ncd", 1, Lang::Turtle);
        doc.apply(change(Some(Range::new(pos(0, 999), pos(0, 999))), "X"));
        assert_eq!(doc.text(), "abX\r\ncd");
        // A line past the document still clamps to the document end.
        let mut doc = Doc::new("ab", 1, Lang::Turtle);
        doc.apply(change(Some(Range::new(pos(9, 0), pos(9, 5))), "!"));
        assert_eq!(doc.text(), "ab!");
    }

    #[test]
    fn full_replace_when_no_range() {
        let mut doc = Doc::new("old", 1, Lang::Turtle);
        doc.apply(change(None, "brand new"));
        assert_eq!(doc.text(), "brand new");
    }
}
