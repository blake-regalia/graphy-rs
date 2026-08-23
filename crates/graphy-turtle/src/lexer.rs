//! Incremental tokenizer shared by every format driver (doc 03 §3).
//!
//! The lexer owns the carry buffer: `feed` appends bytes, `next` returns the
//! next complete token or `Ok(None)` when the buffer ends mid-token and more
//! input may arrive (the caller retries after the next feed — position is
//! only advanced when a token completes, so a retry rescans just the token in
//! flight). Delimiter hunting uses `memchr`; escape decoding is lazy and
//! per-token into reused scratch buffers.

use graphy_core::Dir;
use memchr::{memchr, memchr2, memchr3};

use crate::tables;
use crate::unescape;
use crate::ParseError;

/// Where a token's text lives; resolved via [`Lexer::text`] immediately after
/// `next` returns (ranges go stale at the following `feed`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Text {
    /// Range into the lexer buffer (no escapes; validated UTF-8).
    Buf { start: usize, end: usize },
    /// Escape-decoded into `scratch_a`.
    ScratchA,
    /// Escape-decoded into `scratch_b` (second slot for PNAME locals).
    ScratchB,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Token {
    /// `<…>` with UCHAR escapes decoded; may be relative (driver resolves).
    Iri(Text),
    /// `prefix:local`; local has PLX escapes decoded, `%XX` kept verbatim.
    Pname {
        prefix: Text,
        local: Text,
    },
    /// `_:label` (surface label; driver maps to an internal label).
    BlankLabel(Text),
    /// Any quote style; text is the decoded content.
    String {
        content: Text,
        long: bool,
        single: bool,
    },
    /// `@tag` / `@tag--dir` — tag as written (driver lowercases).
    LangTag {
        tag: Text,
        dir: Option<Dir>,
    },
    Integer(Text),
    Decimal(Text),
    Double(Text),
    KwA,
    KwTrue,
    KwFalse,
    /// `@prefix` / `@base` (Turtle style, terminated by `.`).
    KwPrefixAt,
    KwBaseAt,
    /// `PREFIX` / `BASE` (SPARQL style, no `.`).
    KwPrefixSparql,
    KwBaseSparql,
    KwGraph,
    /// `@version` / `VERSION` (RDF 1.2).
    KwVersionAt,
    KwVersionSparql,
    Dot,
    Semicolon,
    Comma,
    LParen,
    RParen,
    LBracket,
    RBracket,
    LBrace,
    RBrace,
    /// `^^`
    DoubleCaret,
    /// `<<` (reified triple open, RDF 1.2)
    LtLt,
    /// `>>`
    GtGt,
    /// `<<(` (triple term open, RDF 1.2)
    LtLtParen,
    /// `)>>`
    RParenGtGt,
    /// `{|` (annotation open, RDF 1.2)
    AnnoOpen,
    /// `|}`
    AnnoClose,
    /// `~` (reifier, RDF 1.2)
    Tilde,
    /// True end of input (only after `set_eof`).
    Eof,
}

#[derive(Debug, Default)]
pub(crate) struct Lexer {
    buf: Vec<u8>,
    /// Consume cursor: everything before it is fully tokenized.
    pos: usize,
    eof: bool,
    /// Trusted-input mode (`Options::trusted`): skip forbidden-character
    /// validation inside IRIs and strings. Selected once per token — the
    /// validating scan loops are never touched by this flag.
    pub(crate) trusted: bool,
    /// Global byte offset of `buf[0]`.
    base_off: u64,
    /// 1-based line number at `pos` and global offset of that line's start.
    line: u64,
    line_start: u64,
    pub(crate) scratch_a: Vec<u8>,
    pub(crate) scratch_b: Vec<u8>,
    /// Buffer index where the most recent token began (for driver errors).
    tok_start: usize,
}

/// `Ok(Some(tok))`, `Ok(None)` = need more input, or a positioned error.
type Lexed = Result<Option<Token>, ParseError>;

impl Lexer {
    pub fn new() -> Lexer {
        Lexer {
            line: 1,
            ..Lexer::default()
        }
    }

    /// Append a chunk, discarding the already-consumed prefix.
    pub fn feed(&mut self, chunk: &[u8]) {
        if self.pos > 0 {
            self.buf.drain(..self.pos);
            self.base_off += self.pos as u64;
            self.pos = 0;
        }
        self.buf.extend_from_slice(chunk);
    }

    pub fn set_eof(&mut self) {
        self.eof = true;
    }

    /// Resolve token text. Valid until the next `feed`.
    pub fn text(&self, t: Text) -> &[u8] {
        match t {
            Text::Buf { start, end } => &self.buf[start..end],
            Text::ScratchA => &self.scratch_a,
            Text::ScratchB => &self.scratch_b,
        }
    }

    /// Resolve token text as `str` (all Text contents are validated UTF-8).
    pub fn text_str(&self, t: Text) -> &str {
        std::str::from_utf8(self.text(t)).expect("token text validated during lexing")
    }

    pub fn err_at(&self, at: usize, message: impl Into<String>) -> ParseError {
        let offset = self.base_off + at as u64;
        let span = &self.buf[self.pos.min(at)..at];
        let extra = memchr::memchr_iter(b'\n', span).count() as u64;
        let line_start = match memchr::memrchr(b'\n', span) {
            Some(i) => self.base_off + (self.pos.min(at) + i + 1) as u64,
            None => self.line_start,
        };
        ParseError {
            message: message.into(),
            offset,
            line: self.line + extra,
            column: offset - line_start + 1,
        }
    }

    /// Error at the current token start.
    pub fn err_here(&self, message: impl Into<String>) -> ParseError {
        self.err_at(self.pos, message)
    }

    /// Start of the most recent token from `next` (for driver errors).
    pub fn token_start(&self) -> usize {
        self.tok_start
    }

    /// Consume cursor: the buffer index one past the last completed token
    /// (i.e. the end offset of the token `next` just returned).
    pub fn pos(&self) -> usize {
        self.pos
    }

    /// Highlight-recovery: force the consume cursor forward past a bad byte at
    /// `at`, landing on the next UTF-8 boundary. Guarantees ≥1 byte of forward
    /// progress so a resilient tokenizer over arbitrary input always
    /// terminates (docs/10 §3.3).
    pub fn recover_past(&mut self, at: usize) {
        let mut to = at.max(self.pos) + 1;
        while to < self.buf.len() && (self.buf[to] & 0xC0) == 0x80 {
            to += 1;
        }
        self.accept(to.min(self.buf.len()));
    }

    /// Lenient-mode recovery at token granularity: discard *tokens* through
    /// the next statement terminator `.`. Unlike [`skip_past`](Self::skip_past),
    /// the resync point can never land inside an IRI or string whose content
    /// happens to contain the delimiter byte (`<http://w3.org/x.ttl>`) — the
    /// byte hunt turned one error into a cascade and swallowed the statements
    /// in between. Lex errors during the skip are forced past bytewise.
    /// Returns false when the buffer ran out first (need more input / EOF).
    pub fn skip_to_statement_end(&mut self) -> bool {
        loop {
            match self.next() {
                Ok(Some(Token::Dot)) => return true,
                Ok(Some(Token::Eof)) | Ok(None) => return false,
                Ok(Some(_)) => {}
                Err(e) => {
                    let local = (e.offset - self.base_off) as usize;
                    self.recover_past(local);
                }
            }
        }
    }

    /// Lenient-mode recovery: discard bytes through the next `delim`.
    /// Returns false when the buffer ran out first (need more input / EOF).
    pub fn skip_past(&mut self, delim: u8) -> bool {
        match memchr(delim, &self.buf[self.pos..]) {
            Some(i) => {
                self.accept(self.pos + i + 1);
                true
            }
            None => {
                self.accept(self.buf.len());
                false
            }
        }
    }

    /// Advance the consume cursor, maintaining line/column tracking.
    fn accept(&mut self, to: usize) {
        let span = &self.buf[self.pos..to];
        let mut count = 0u64;
        let mut last = None;
        for i in memchr::memchr_iter(b'\n', span) {
            count += 1;
            last = Some(i);
        }
        if let Some(i) = last {
            self.line += count;
            self.line_start = self.base_off + (self.pos + i + 1) as u64;
        }
        self.pos = to;
    }

    /// Next token; `Ok(None)` = the buffer ends mid-token and `eof` is unset.
    pub fn next(&mut self) -> Lexed {
        loop {
            match self.buf.get(self.pos) {
                None => {
                    return if self.eof {
                        Ok(Some(Token::Eof))
                    } else {
                        Ok(None)
                    }
                }
                Some(b' ' | b'\t' | b'\r' | b'\n') => {
                    let mut i = self.pos + 1;
                    while matches!(self.buf.get(i), Some(b' ' | b'\t' | b'\r' | b'\n')) {
                        i += 1;
                    }
                    self.accept(i);
                }
                Some(b'#') => match memchr(b'\n', &self.buf[self.pos..]) {
                    Some(i) => self.accept(self.pos + i + 1),
                    None if self.eof => self.accept(self.buf.len()),
                    None => return Ok(None),
                },
                Some(_) => break,
            }
        }
        let start = self.pos;
        self.tok_start = start;
        match self.buf[start] {
            b'<' => self.lex_lt(start),
            b'"' | b'\'' => self.lex_string(start),
            b'_' => self.lex_blank(start),
            b'@' => self.lex_at(start),
            b'0'..=b'9' | b'+' | b'-' => self.lex_number(start),
            b'.' => {
                match self.buf.get(start + 1) {
                    Some(b'0'..=b'9') => self.lex_number(start),
                    None if !self.eof => Ok(None), // could be ".5"
                    _ => {
                        self.accept(start + 1);
                        Ok(Some(Token::Dot))
                    }
                }
            }
            b';' => self.punct(start, 1, Token::Semicolon),
            b',' => self.punct(start, 1, Token::Comma),
            b'(' => self.punct(start, 1, Token::LParen),
            b'[' => self.punct(start, 1, Token::LBracket),
            b']' => self.punct(start, 1, Token::RBracket),
            b'}' => self.punct(start, 1, Token::RBrace),
            b'~' => self.punct(start, 1, Token::Tilde),
            b')' => {
                // `)>>` closes a triple term; collections are not allowed
                // directly inside `<< >>`, so the greedy match is safe.
                match (self.buf.get(start + 1), self.buf.get(start + 2)) {
                    (Some(b'>'), Some(b'>')) => self.punct(start, 3, Token::RParenGtGt),
                    (Some(b'>'), None) | (None, _) if !self.eof => Ok(None),
                    _ => self.punct(start, 1, Token::RParen),
                }
            }
            b'{' => match self.buf.get(start + 1) {
                Some(b'|') => self.punct(start, 2, Token::AnnoOpen),
                None if !self.eof => Ok(None),
                _ => self.punct(start, 1, Token::LBrace),
            },
            b'|' => match self.buf.get(start + 1) {
                Some(b'}') => self.punct(start, 2, Token::AnnoClose),
                None if !self.eof => Ok(None),
                _ => Err(self.err_here("unexpected '|'")),
            },
            b'^' => match self.buf.get(start + 1) {
                Some(b'^') => self.punct(start, 2, Token::DoubleCaret),
                None if !self.eof => Ok(None),
                _ => Err(self.err_here("expected '^^'")),
            },
            b'>' => match self.buf.get(start + 1) {
                Some(b'>') => self.punct(start, 2, Token::GtGt),
                None if !self.eof => Ok(None),
                _ => Err(self.err_here("unexpected '>'")),
            },
            _ => self.lex_word(start),
        }
    }

    fn punct(&mut self, start: usize, len: usize, tok: Token) -> Lexed {
        self.accept(start + len);
        Ok(Some(tok))
    }

    /// Decode the char at `i`. `Ok(None)` = truncated multibyte before EOF.
    fn char_at(&self, i: usize) -> Result<Option<(char, usize)>, ParseError> {
        let b = self.buf[i];
        if b < 0x80 {
            return Ok(Some((b as char, 1)));
        }
        let width = match b {
            0xC0..=0xDF => 2,
            0xE0..=0xEF => 3,
            0xF0..=0xF7 => 4,
            _ => return Err(self.err_at(i, "invalid UTF-8")),
        };
        if i + width > self.buf.len() {
            return if self.eof {
                Err(self.err_at(i, "invalid UTF-8"))
            } else {
                Ok(None)
            };
        }
        match std::str::from_utf8(&self.buf[i..i + width]) {
            Ok(s) => Ok(Some((s.chars().next().expect("nonempty"), width))),
            Err(_) => Err(self.err_at(i, "invalid UTF-8")),
        }
    }

    // ------------------------------------------------------------ IRIs

    fn lex_lt(&mut self, start: usize) -> Lexed {
        match self.buf.get(start + 1) {
            None if !self.eof => Ok(None),
            Some(b'<') => match self.buf.get(start + 2) {
                None if !self.eof => Ok(None),
                Some(b'(') => self.punct(start, 3, Token::LtLtParen),
                _ => self.punct(start, 2, Token::LtLt),
            },
            _ => self.lex_iriref(start),
        }
    }

    fn lex_iriref(&mut self, start: usize) -> Lexed {
        let Some(gt) = memchr(b'>', &self.buf[start + 1..]) else {
            return if self.eof {
                Err(self.err_here("unterminated IRI"))
            } else {
                Ok(None)
            };
        };
        let (cs, ce) = (start + 1, start + 1 + gt);
        let has_escape = if self.trusted {
            // Trusted input: the content is assumed free of forbidden
            // characters, so the only per-byte question left is whether any
            // escapes need decoding — one SIMD sweep instead of the scalar
            // table-lookup loop below.
            memchr(b'\\', &self.buf[cs..ce]).is_some()
        } else {
            let mut has_escape = false;
            for i in cs..ce {
                let b = self.buf[i];
                if b == b'\\' {
                    has_escape = true;
                } else if tables::is_forbidden_iri_byte(b) {
                    return Err(
                        self.err_at(i, format!("character {:?} not allowed in IRI", b as char))
                    );
                }
            }
            has_escape
        };
        let text = if has_escape {
            self.decode_iri_escapes(cs, ce)?
        } else {
            std::str::from_utf8(&self.buf[cs..ce])
                .map_err(|_| self.err_at(cs, "invalid UTF-8 in IRI"))?;
            Text::Buf { start: cs, end: ce }
        };
        self.accept(ce + 1);
        Ok(Some(Token::Iri(text)))
    }

    fn decode_iri_escapes(&mut self, cs: usize, ce: usize) -> Result<Text, ParseError> {
        std::str::from_utf8(&self.buf[cs..ce])
            .map_err(|_| self.err_at(cs, "invalid UTF-8 in IRI"))?;
        let mut out = std::mem::take(&mut self.scratch_a);
        out.clear();
        let mut i = cs;
        while let Some(j) = memchr(b'\\', &self.buf[i..ce]) {
            out.extend_from_slice(&self.buf[i..i + j]);
            let at = i + j;
            let Some((c, used)) = unescape::decode_uchar(&self.buf[..ce], at) else {
                self.scratch_a = out;
                return Err(self.err_at(at, "invalid \\u escape in IRI"));
            };
            if (c as u32) < 0x80 && tables::is_forbidden_iri_byte(c as u8) {
                self.scratch_a = out;
                return Err(self.err_at(at, format!("escape encodes {c:?}, not allowed in IRI")));
            }
            let mut enc = [0u8; 4];
            out.extend_from_slice(c.encode_utf8(&mut enc).as_bytes());
            i = at + used;
        }
        out.extend_from_slice(&self.buf[i..ce]);
        self.scratch_a = out;
        Ok(Text::ScratchA)
    }

    // ---------------------------------------------------------- strings

    fn lex_string(&mut self, start: usize) -> Lexed {
        let q = self.buf[start];
        // Disambiguate "", "…", """…""".
        let long = match (self.buf.get(start + 1), self.buf.get(start + 2)) {
            (Some(&b1), Some(&b2)) if b1 == q && b2 == q => true,
            (Some(&b1), None) if b1 == q && !self.eof => return Ok(None),
            (Some(&b1), _) if b1 == q => {
                // Empty short string.
                self.accept(start + 2);
                return Ok(Some(Token::String {
                    content: Text::Buf {
                        start: start + 1,
                        end: start + 1,
                    },
                    long: false,
                    single: q == b'\'',
                }));
            }
            (None, _) if !self.eof => return Ok(None),
            (None, _) => return Err(self.err_here("unterminated string")),
            _ => false,
        };
        if long {
            self.lex_long_string(start, q)
        } else {
            self.lex_short_string(start, q)
        }
    }

    fn lex_short_string(&mut self, start: usize, q: u8) -> Lexed {
        if self.trusted {
            return self.lex_short_string_trusted(start, q);
        }
        let cs = start + 1;
        let mut i = cs;
        let mut has_escape = false;
        let ce = loop {
            let Some(j) = memchr3(q, b'\\', b'\n', &self.buf[i..]) else {
                return if self.eof {
                    Err(self.err_here("unterminated string"))
                } else {
                    Ok(None)
                };
            };
            let k = i + j;
            match self.buf[k] {
                b'\n' => return Err(self.err_at(k, "newline in single-line string")),
                b'\\' => {
                    has_escape = true;
                    if k + 1 >= self.buf.len() {
                        return if self.eof {
                            Err(self.err_at(k, "unterminated escape"))
                        } else {
                            Ok(None)
                        };
                    }
                    i = k + 2;
                }
                _ => break k,
            }
        };
        if let Some(r) = memchr(b'\r', &self.buf[cs..ce]) {
            if !has_escape || !is_escaped_at(&self.buf, cs, cs + r) {
                return Err(self.err_at(cs + r, "carriage return in single-line string"));
            }
        }
        let content = self.finish_string_content(cs, ce, has_escape)?;
        self.accept(ce + 1);
        Ok(Some(Token::String {
            content,
            long: false,
            single: q == b'\'',
        }))
    }

    /// Trusted-input short string: the content is assumed free of raw
    /// newlines/carriage returns, so the scan only hunts the closing quote
    /// and escapes (`memchr2` instead of `memchr3`) and the whole-content
    /// CR sweep is elided. Escape decoding and UTF-8 validation are
    /// unchanged.
    fn lex_short_string_trusted(&mut self, start: usize, q: u8) -> Lexed {
        let cs = start + 1;
        let mut i = cs;
        let mut has_escape = false;
        let ce = loop {
            let Some(j) = memchr2(q, b'\\', &self.buf[i..]) else {
                return if self.eof {
                    Err(self.err_here("unterminated string"))
                } else {
                    Ok(None)
                };
            };
            let k = i + j;
            if self.buf[k] == b'\\' {
                has_escape = true;
                if k + 1 >= self.buf.len() {
                    return if self.eof {
                        Err(self.err_at(k, "unterminated escape"))
                    } else {
                        Ok(None)
                    };
                }
                i = k + 2;
            } else {
                break k;
            }
        };
        let content = self.finish_string_content(cs, ce, has_escape)?;
        self.accept(ce + 1);
        Ok(Some(Token::String {
            content,
            long: false,
            single: q == b'\'',
        }))
    }

    fn lex_long_string(&mut self, start: usize, q: u8) -> Lexed {
        let cs = start + 3;
        let mut i = cs;
        let mut has_escape = false;
        let (ce, te) = loop {
            let Some(j) = memchr2(q, b'\\', &self.buf[i..]) else {
                return if self.eof {
                    Err(self.err_here("unterminated long string"))
                } else {
                    Ok(None)
                };
            };
            let k = i + j;
            if self.buf[k] == b'\\' {
                has_escape = true;
                if k + 1 >= self.buf.len() {
                    return if self.eof {
                        Err(self.err_at(k, "unterminated escape"))
                    } else {
                        Ok(None)
                    };
                }
                i = k + 2;
                continue;
            }
            let mut run = 1;
            while self.buf.get(k + run) == Some(&q) {
                run += 1;
            }
            if k + run == self.buf.len() && !self.eof {
                // The quote run touches the buffer end and may still grow.
                return Ok(None);
            }
            if run >= 3 {
                // Grammar: content quotes must be followed by a non-quote, so
                // an unescaped run is exactly the 3-quote terminator or a
                // syntax error.
                if run > 3 {
                    return Err(
                        self.err_at(k, "long string content may not end with unescaped quotes")
                    );
                }
                break (k, k + 3);
            }
            i = k + run;
        };
        let content = self.finish_string_content(cs, ce, has_escape)?;
        self.accept(te);
        Ok(Some(Token::String {
            content,
            long: true,
            single: q == b'\'',
        }))
    }

    fn finish_string_content(
        &mut self,
        cs: usize,
        ce: usize,
        has_escape: bool,
    ) -> Result<Text, ParseError> {
        std::str::from_utf8(&self.buf[cs..ce])
            .map_err(|_| self.err_at(cs, "invalid UTF-8 in string"))?;
        if !has_escape {
            return Ok(Text::Buf { start: cs, end: ce });
        }
        let mut out = std::mem::take(&mut self.scratch_a);
        out.clear();
        let mut i = cs;
        while let Some(j) = memchr(b'\\', &self.buf[i..ce]) {
            out.extend_from_slice(&self.buf[i..i + j]);
            let at = i + j;
            let decoded = unescape::decode_echar(&self.buf[..ce], at)
                .or_else(|| unescape::decode_uchar(&self.buf[..ce], at));
            let Some((c, used)) = decoded else {
                self.scratch_a = out;
                return Err(self.err_at(at, "invalid string escape"));
            };
            let mut enc = [0u8; 4];
            out.extend_from_slice(c.encode_utf8(&mut enc).as_bytes());
            i = at + used;
        }
        out.extend_from_slice(&self.buf[i..ce]);
        self.scratch_a = out;
        Ok(Text::ScratchA)
    }

    // ------------------------------------------------------ blank nodes

    fn lex_blank(&mut self, start: usize) -> Lexed {
        match self.buf.get(start + 1) {
            Some(b':') => {}
            None if !self.eof => return Ok(None),
            _ => return Err(self.err_here("expected '_:'")),
        }
        let ls = start + 2;
        let mut i = ls;
        // First char: PN_CHARS_U or digit.
        match self.buf.get(i) {
            None if !self.eof => return Ok(None),
            None => return Err(self.err_here("blank node label required")),
            Some(_) => match self.char_at(i)? {
                None => return Ok(None),
                Some((c, w)) if tables::is_pn_chars_u(c) || c.is_ascii_digit() => i += w,
                Some(_) => return Err(self.err_at(i, "invalid blank node label start")),
            },
        }
        // Continue: PN_CHARS or '.', with no trailing '.'.
        let mut end = i; // end after last char that may end the label
        loop {
            match self.buf.get(i) {
                None if !self.eof => return Ok(None),
                None => break,
                Some(b'.') => i += 1,
                Some(_) => match self.char_at(i)? {
                    None => return Ok(None),
                    Some((c, w)) if tables::is_pn_chars(c) => {
                        i += w;
                        end = i;
                    }
                    Some(_) => break,
                },
            }
        }
        let text = Text::Buf { start: ls, end };
        self.accept(end);
        Ok(Some(Token::BlankLabel(text)))
    }

    // ----------------------------------------------- @langtag / @directives

    fn lex_at(&mut self, start: usize) -> Lexed {
        let mut i = start + 1;
        while matches!(self.buf.get(i), Some(b) if b.is_ascii_alphabetic()) {
            i += 1;
        }
        if i == self.buf.len() && !self.eof {
            return Ok(None);
        }
        if i == start + 1 {
            return Err(self.err_here("expected language tag or directive after '@'"));
        }
        let primary = &self.buf[start + 1..i];
        if !matches!(self.buf.get(i), Some(b'-')) {
            let directive = match primary {
                b"prefix" => Some(Token::KwPrefixAt),
                b"base" => Some(Token::KwBaseAt),
                b"version" => Some(Token::KwVersionAt),
                _ => None,
            };
            if let Some(tok) = directive {
                self.accept(i);
                return Ok(Some(tok));
            }
        }
        if primary.len() > 8 {
            return Err(self.err_here("language tag subtag longer than 8 characters"));
        }
        // Subtags, then an optional `--ltr` / `--rtl` direction (RDF 1.2).
        // The direction is carried out-of-band, so the tag text ends before
        // the `--`.
        let mut dir = None;
        let mut tag_end = i;
        loop {
            if self.buf.get(i) != Some(&b'-') {
                break;
            }
            match self.buf.get(i + 1) {
                None if !self.eof => return Ok(None),
                Some(b'-') => {
                    let ds = i + 2;
                    let mut j = ds;
                    while matches!(self.buf.get(j), Some(b) if b.is_ascii_alphabetic()) {
                        j += 1;
                    }
                    if j == self.buf.len() && !self.eof {
                        return Ok(None);
                    }
                    dir = match &self.buf[ds..j] {
                        b"ltr" => Some(Dir::Ltr),
                        b"rtl" => Some(Dir::Rtl),
                        _ => return Err(self.err_at(i, "base direction must be --ltr or --rtl")),
                    };
                    i = j;
                    break;
                }
                Some(b) if b.is_ascii_alphanumeric() => {
                    let sub_start = i + 1;
                    i += 1;
                    while matches!(self.buf.get(i), Some(b) if b.is_ascii_alphanumeric()) {
                        i += 1;
                    }
                    if i == self.buf.len() && !self.eof {
                        return Ok(None);
                    }
                    if i - sub_start > 8 {
                        return Err(
                            self.err_at(sub_start, "language tag subtag longer than 8 characters")
                        );
                    }
                    tag_end = i;
                }
                _ => break, // trailing '-' is not part of the tag
            }
        }
        let text = Text::Buf {
            start: start + 1,
            end: tag_end,
        };
        // Direction end may still grow ("@en--ltr" + more letters?) — no:
        // ltr/rtl matched exactly and the next byte is a boundary by the scan.
        self.accept(i);
        Ok(Some(Token::LangTag { tag: text, dir }))
    }

    // ---------------------------------------------------------- numbers

    fn lex_number(&mut self, start: usize) -> Lexed {
        let n = self.buf.len();
        let mut i = start;
        if matches!(self.buf[i], b'+' | b'-') {
            i += 1;
        }
        let d1s = i;
        while matches!(self.buf.get(i), Some(b) if b.is_ascii_digit()) {
            i += 1;
        }
        if i == n && !self.eof {
            return Ok(None);
        }
        let d1 = i - d1s;
        let accept = |lx: &mut Lexer, end: usize, kind: fn(Text) -> Token| {
            let t = Text::Buf { start, end };
            lx.accept(end);
            Ok(Some(kind(t)))
        };
        match self.buf.get(i) {
            Some(b'.') => {
                let d2s = i + 1;
                let mut j = d2s;
                while matches!(self.buf.get(j), Some(b) if b.is_ascii_digit()) {
                    j += 1;
                }
                if j == n && !self.eof {
                    return Ok(None);
                }
                let d2 = j - d2s;
                match self.buf.get(j) {
                    Some(b'e' | b'E') if d1 + d2 > 0 => {
                        let end = self.scan_exponent(j)?;
                        match end {
                            Some(end) => accept(self, end, Token::Double),
                            None => Ok(None),
                        }
                    }
                    _ if d2 > 0 => accept(self, j, Token::Decimal),
                    // "1." — the dot terminates the statement instead.
                    _ if d1 > 0 => accept(self, i, Token::Integer),
                    _ => Err(self.err_here("invalid number")),
                }
            }
            Some(b'e' | b'E') if d1 > 0 => match self.scan_exponent(i)? {
                Some(end) => accept(self, end, Token::Double),
                None => Ok(None),
            },
            _ if d1 > 0 => accept(self, i, Token::Integer),
            _ => Err(self.err_here("invalid number")),
        }
    }

    /// Scan `[eE] [+-]? [0-9]+` starting at the `e`. `Ok(None)` = need more.
    fn scan_exponent(&mut self, e: usize) -> Result<Option<usize>, ParseError> {
        let mut i = e + 1;
        if matches!(self.buf.get(i), Some(b'+' | b'-')) {
            i += 1;
        }
        let ds = i;
        while matches!(self.buf.get(i), Some(b) if b.is_ascii_digit()) {
            i += 1;
        }
        if i == self.buf.len() && !self.eof {
            return Ok(None);
        }
        if i == ds {
            return Err(self.err_at(e, "invalid exponent"));
        }
        Ok(Some(i))
    }

    // --------------------------------------------- bare words and PNAMEs

    fn lex_word(&mut self, start: usize) -> Lexed {
        let mut i = start;
        // Prefix part (may be empty when the word starts with ':').
        if self.buf[start] != b':' {
            match self.char_at(i)? {
                None => return Ok(None),
                Some((c, w)) if tables::is_pn_chars_base(c) => i += w,
                Some((c, _)) => return Err(self.err_at(i, format!("unexpected character {c:?}"))),
            }
            let mut end = i;
            loop {
                match self.buf.get(i) {
                    None if !self.eof => return Ok(None),
                    None => break,
                    Some(b'.') => i += 1,
                    Some(b':') => break,
                    Some(_) => match self.char_at(i)? {
                        None => return Ok(None),
                        Some((c, w)) if tables::is_pn_chars(c) => {
                            i += w;
                            end = i;
                        }
                        Some(_) => break,
                    },
                }
            }
            // A PNAME requires the colon immediately after a well-formed
            // prefix (in particular: no trailing dots — those return to the
            // stream as statement terminators).
            if self.buf.get(i) != Some(&b':') || i != end {
                let word = &self.buf[start..end];
                let tok = match word {
                    b"a" => Some(Token::KwA),
                    b"true" => Some(Token::KwTrue),
                    b"false" => Some(Token::KwFalse),
                    w if w.eq_ignore_ascii_case(b"prefix") => Some(Token::KwPrefixSparql),
                    w if w.eq_ignore_ascii_case(b"base") => Some(Token::KwBaseSparql),
                    w if w.eq_ignore_ascii_case(b"graph") => Some(Token::KwGraph),
                    w if w.eq_ignore_ascii_case(b"version") => Some(Token::KwVersionSparql),
                    _ => None,
                };
                return match tok {
                    Some(tok) => {
                        self.accept(end);
                        Ok(Some(tok))
                    }
                    None => Err(self.err_at(end, "expected ':' after prefix")),
                };
            }
        }
        let colon = i;
        let prefix = Text::Buf { start, end: colon };
        match self.lex_pn_local_opt(colon + 1)? {
            None => Ok(None),
            Some((local, end)) => {
                self.accept(end);
                Ok(Some(Token::Pname { prefix, local }))
            }
        }
    }

    /// PN_LOCAL from `ls`; returns decoded text and the byte end. `Ok(None)`
    /// = need more input. An empty local is legal (`ex:`).
    fn lex_pn_local_opt(&mut self, ls: usize) -> Result<Option<(Text, usize)>, ParseError> {
        let mut i = ls;
        let mut end = ls; // end after the last char that may end the local
        let mut has_escape = false;
        let mut first = true;
        loop {
            match self.buf.get(i) {
                None if !self.eof => return Ok(None),
                None => break,
                Some(b'%') => {
                    if i + 2 >= self.buf.len() {
                        if !self.eof {
                            return Ok(None);
                        }
                        return Err(self.err_at(i, "invalid %-sequence in local name"));
                    }
                    if !(self.buf[i + 1].is_ascii_hexdigit() && self.buf[i + 2].is_ascii_hexdigit())
                    {
                        return Err(self.err_at(i, "invalid %-sequence in local name"));
                    }
                    i += 3;
                    end = i;
                    first = false;
                }
                Some(b'\\') => match self.buf.get(i + 1) {
                    None if !self.eof => return Ok(None),
                    Some(&b) if tables::is_pn_local_esc(b) => {
                        has_escape = true;
                        i += 2;
                        end = i;
                        first = false;
                    }
                    _ => return Err(self.err_at(i, "invalid escape in local name")),
                },
                Some(b':') => {
                    i += 1;
                    end = i;
                    first = false;
                }
                Some(b'.') if !first => i += 1,
                Some(_) => match self.char_at(i)? {
                    None => return Ok(None),
                    Some((c, w))
                        if (first && (tables::is_pn_chars_u(c) || c.is_ascii_digit()))
                            || (!first && tables::is_pn_chars(c)) =>
                    {
                        i += w;
                        end = i;
                        first = false;
                    }
                    Some(_) => break,
                },
            }
        }
        let text = if has_escape {
            let mut out = std::mem::take(&mut self.scratch_b);
            out.clear();
            let mut j = ls;
            while j < end {
                if self.buf[j] == b'\\' {
                    out.push(self.buf[j + 1]);
                    j += 2;
                } else {
                    out.push(self.buf[j]);
                    j += 1;
                }
            }
            self.scratch_b = out;
            Text::ScratchB
        } else {
            std::str::from_utf8(&self.buf[ls..end])
                .map_err(|_| self.err_at(ls, "invalid UTF-8 in local name"))?;
            Text::Buf { start: ls, end }
        };
        Ok(Some((text, end)))
    }
}

/// Whether the byte at `at` is escaped (odd run of preceding backslashes
/// starting no earlier than `from`).
fn is_escaped_at(buf: &[u8], from: usize, at: usize) -> bool {
    let mut n = 0;
    while at > from + n && buf[at - 1 - n] == b'\\' {
        n += 1;
    }
    n % 2 == 1
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Tokenize a complete input, resolving text into owned strings.
    fn lex_all(input: &str) -> Result<Vec<(Token, Vec<String>)>, ParseError> {
        lex_all_mode(input, false)
    }

    fn lex_all_mode(input: &str, trusted: bool) -> Result<Vec<(Token, Vec<String>)>, ParseError> {
        let mut lx = Lexer::new();
        lx.trusted = trusted;
        lx.feed(input.as_bytes());
        lx.set_eof();
        let mut out = Vec::new();
        loop {
            let tok = lx.next()?.expect("eof set, never NeedMore");
            let texts = token_texts(&lx, &tok);
            if matches!(tok, Token::Eof) {
                return Ok(out);
            }
            out.push((tok, texts));
        }
    }

    fn token_texts(lx: &Lexer, tok: &Token) -> Vec<String> {
        let t = |x: Text| String::from_utf8(lx.text(x).to_vec()).unwrap();
        match *tok {
            Token::Iri(x)
            | Token::BlankLabel(x)
            | Token::Integer(x)
            | Token::Decimal(x)
            | Token::Double(x) => vec![t(x)],
            Token::String { content, .. } => vec![t(content)],
            Token::LangTag { tag, .. } => vec![t(tag)],
            Token::Pname { prefix, local } => vec![t(prefix), t(local)],
            _ => vec![],
        }
    }

    /// Same input lexed with every possible split point must agree.
    fn lex_all_split(input: &str, at: usize) -> Result<Vec<(Token, Vec<String>)>, ParseError> {
        let bytes = input.as_bytes();
        let mut lx = Lexer::new();
        let mut out = Vec::new();
        for (i, part) in [&bytes[..at], &bytes[at..]].into_iter().enumerate() {
            lx.feed(part);
            if i == 1 {
                lx.set_eof();
            }
            loop {
                match lx.next()? {
                    None => break,
                    Some(Token::Eof) => return Ok(out),
                    Some(tok) => {
                        let texts = token_texts(&lx, &tok);
                        out.push((tok, texts));
                    }
                }
            }
        }
        Ok(out)
    }

    /// Token identity minus buffer offsets (splits shift Buf ranges).
    fn normalize(toks: &[(Token, Vec<String>)]) -> Vec<(String, Vec<String>)> {
        toks.iter()
            .map(|(t, texts)| {
                let kind = match t {
                    Token::String { long, single, .. } => format!("String/{long}/{single}"),
                    Token::LangTag { dir, .. } => format!("LangTag/{dir:?}"),
                    Token::Iri(_) => "Iri".to_owned(),
                    Token::Pname { .. } => "Pname".to_owned(),
                    Token::BlankLabel(_) => "BlankLabel".to_owned(),
                    Token::Integer(_) => "Integer".to_owned(),
                    Token::Decimal(_) => "Decimal".to_owned(),
                    Token::Double(_) => "Double".to_owned(),
                    other => format!("{other:?}"),
                };
                (kind, texts.clone())
            })
            .collect()
    }

    #[test]
    fn basic_tokens() {
        let toks = lex_all(
            "<http://x/> _:b0 \"hi\" 'yo' \"\"\"long\n line\"\"\" @en @en--rtl 42 -4.2 .5e0 \
             a true false ex:p : :o ^^ . ; , ( ) [ ] { } << >> <<( )>> {| |} ~",
        )
        .unwrap();
        let kinds: Vec<&Token> = toks.iter().map(|(t, _)| t).collect();
        use Token::*;
        assert!(matches!(kinds[0], Iri(_)));
        assert!(matches!(kinds[1], BlankLabel(_)));
        assert!(matches!(
            kinds[2],
            String {
                long: false,
                single: false,
                ..
            }
        ));
        assert!(matches!(
            kinds[3],
            String {
                long: false,
                single: true,
                ..
            }
        ));
        assert!(matches!(kinds[4], String { long: true, .. }));
        assert!(matches!(kinds[5], LangTag { dir: None, .. }));
        assert!(matches!(
            kinds[6],
            LangTag {
                dir: Some(Dir::Rtl),
                ..
            }
        ));
        assert!(matches!(kinds[7], Integer(_)));
        assert!(matches!(kinds[8], Decimal(_)));
        assert!(matches!(kinds[9], Double(_)));
        assert!(matches!(kinds[10..13], [KwA, KwTrue, KwFalse]));
        assert!(matches!(kinds[13], Pname { .. }));
        assert!(matches!(kinds[14], Pname { .. }));
        assert!(matches!(kinds[15], Pname { .. }));
        assert!(matches!(
            kinds[16..],
            [
                DoubleCaret,
                Dot,
                Semicolon,
                Comma,
                LParen,
                RParen,
                LBracket,
                RBracket,
                LBrace,
                RBrace,
                LtLt,
                GtGt,
                LtLtParen,
                RParenGtGt,
                AnnoOpen,
                AnnoClose,
                Tilde
            ]
        ));
        assert_eq!(toks[0].1, ["http://x/"]);
        assert_eq!(toks[4].1, ["long\n line"]);
        assert_eq!(toks[13].1, ["ex", "p"]);
        assert_eq!(toks[14].1, ["", ""]);
        assert_eq!(toks[15].1, ["", "o"]);
    }

    #[test]
    fn escapes_decode() {
        let toks = lex_all(r#"<http://x/A> "a\tbé\U0001F600" ex:l\%oc\,al"#).unwrap();
        assert_eq!(toks[0].1, ["http://x/A"]);
        assert_eq!(toks[1].1, ["a\tbé😀"]);
        assert_eq!(toks[2].1, ["ex", "l%oc,al"]);
    }

    #[test]
    fn directives_vs_langtags() {
        let toks = lex_all("@prefix @base PREFIX bAsE GRAPH @prefix-x").unwrap();
        use Token::*;
        assert!(matches!(
            toks.iter().map(|(t, _)| t).collect::<Vec<_>>()[..],
            [
                &KwPrefixAt,
                &KwBaseAt,
                &KwPrefixSparql,
                &KwBaseSparql,
                &KwGraph,
                &LangTag { .. }
            ]
        ));
        assert_eq!(toks[5].1, ["prefix-x"]);
    }

    #[test]
    fn numbers_and_dots() {
        // "1." lexes INTEGER then Dot; "1.5" is a decimal; "1.e0" a double.
        let toks = lex_all("1. 1.5 1.e0 .5 -0.5e-3 +7").unwrap();
        use Token::*;
        let kinds: Vec<&Token> = toks.iter().map(|(t, _)| t).collect();
        assert!(matches!(kinds[0], Integer(_)));
        assert!(matches!(kinds[1], Dot));
        assert!(matches!(kinds[2], Decimal(_)));
        assert!(matches!(kinds[3], Double(_)));
        assert!(matches!(kinds[4], Decimal(_)));
        assert!(matches!(kinds[5], Double(_)));
        assert!(matches!(kinds[6], Integer(_)));
        assert_eq!(toks[5].1, ["-0.5e-3"]);
    }

    #[test]
    fn pname_trailing_dots_return_to_stream() {
        let toks = lex_all("ex:o. _:b. true.").unwrap();
        use Token::*;
        let kinds: Vec<&Token> = toks.iter().map(|(t, _)| t).collect();
        assert!(matches!(kinds[0], Pname { .. }));
        assert!(matches!(kinds[1], Dot));
        assert!(matches!(kinds[2], BlankLabel(_)));
        assert!(matches!(kinds[3], Dot));
        assert!(matches!(kinds[4], KwTrue));
        assert!(matches!(kinds[5], Dot));
        assert_eq!(toks[0].1, ["ex", "o"]);
        assert_eq!(toks[2].1, ["b"]);
    }

    #[test]
    fn long_string_quote_runs() {
        // 6 quotes: empty long string.
        assert_eq!(lex_all("\"\"\"\"\"\"").unwrap()[0].1, [""]);
        // Interior 1-2 quote runs are content when followed by a non-quote.
        assert_eq!(lex_all("\"\"\"a\"\"b\"\"\"").unwrap()[0].1, ["a\"\"b"]);
        // Leading quotes right after the opener are content too.
        assert_eq!(lex_all("\"\"\"\"a\"\"\"").unwrap()[0].1, ["\"a"]);
        // Content may not END with unescaped quotes (the grammar requires a
        // non-quote after every 1-2 quote run): 4+ closing quotes = error.
        assert!(lex_all("\"\"\"a\"\"\"\"").is_err());
        // Escaping the trailing quote is the way to write it.
        assert_eq!(lex_all("\"\"\"a\\\"\"\"\"").unwrap()[0].1, ["a\""]);
    }

    #[test]
    fn errors_are_positioned() {
        let e = lex_all("<http://x/>\n  <bad iri>").unwrap_err();
        assert_eq!((e.line, e.column), (2, 7));
        assert!(lex_all("\"unterminated").is_err());
        assert!(lex_all("\"nl\ninside\"").is_err());
        assert!(lex_all("@en--xyz").is_err());
        assert!(lex_all("1e").is_err());
        assert!(lex_all("^x").is_err());
        assert!(lex_all(r"<http://x/ y>").is_err()); // escape → forbidden char
    }

    #[test]
    fn trusted_mode_agrees_on_valid_input() {
        // Same token stream, same decoded text, on inputs covering the
        // trusted fast paths (IRIs with/without escapes, short/long strings
        // with/without escapes, escaped quotes).
        let inputs = [
            "<http://x/> <http://x/\\u0041b> \"plain\" \"esc\\\"aped\" 'y' '''long\n \"\" q''' .",
            "<a:b%41> \"tab\\there\" \"\" '' ex:p 42 .",
        ];
        for input in inputs {
            assert_eq!(
                normalize(&lex_all_mode(input, false).unwrap()),
                normalize(&lex_all_mode(input, true).unwrap()),
                "modes differ on {input:?}"
            );
        }
    }

    #[test]
    fn trusted_mode_skips_validation_only() {
        // Contract: trusted mode may ACCEPT invalid input (no validation),
        // but must never panic or read out of bounds.
        // Forbidden raw character in an IRI: rejected normally, waved
        // through trusted (garbage in, garbage out).
        assert!(lex_all_mode("<http://x/ y>", false).is_err());
        assert!(lex_all_mode("<http://x/ y>", true).is_ok());
        // Raw newline in a short string: likewise.
        assert!(lex_all_mode("\"a\nb\"", false).is_err());
        assert!(lex_all_mode("\"a\nb\"", true).is_ok());
        // Hard errors that stay errors: truncation and bad escapes.
        assert!(lex_all_mode("\"unterminated", true).is_err());
        assert!(lex_all_mode("<unterminated", true).is_err());
        assert!(lex_all_mode("\"bad \\q escape\"", true).is_err());
        // Invalid UTF-8 is still rejected (text_str's safety contract).
        let mut lx = Lexer::new();
        lx.trusted = true;
        lx.feed(b"\"a\xFFb\"");
        lx.set_eof();
        assert!(lx.next().is_err());
    }

    #[test]
    fn every_split_point_agrees() {
        let input = "<http://x/\\u0041> _:b.l \"a\\tb\" '''x''y'''@en--ltr ex:lo\\,c 4.2e-1 <<( )>> {| |} # c\n .";
        let whole = normalize(&lex_all(input).unwrap());
        for at in 0..=input.len() {
            if !input.is_char_boundary(at) {
                continue;
            }
            let split = lex_all_split(input, at).unwrap_or_else(|e| panic!("split at {at}: {e}"));
            assert_eq!(normalize(&split), whole, "split at {at}");
        }
    }
}
