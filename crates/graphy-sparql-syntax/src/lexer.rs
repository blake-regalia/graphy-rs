//! Single-pass SPARQL lexer (doc 04 §2): queries are small, so the whole
//! source is in memory and the output is a token vector with byte spans.
//! Keywords are case-insensitive; a bare word followed by `:` is always a
//! prefixed name, never a keyword. SPARQL 1.2 terminals (`<<(`, `)>>`,
//! `<<`, `>>`, `~`, `{|`, `|}`, directional language tags, the new builtin
//! keywords) are always lexed — the parser gates them by spec mode.

use graphy_core::text::{
    is_forbidden_iri_byte, is_pn_chars, is_pn_chars_base, is_pn_chars_u, is_pn_local_esc,
    is_varname_char,
};

use crate::token::{Dir, Kw, Span, StringForm, Token, TokenKind};

/// A span-carrying lexical error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LexError {
    pub span: Span,
    pub message: String,
}

impl std::fmt::Display for LexError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} at byte {}", self.message, self.span.start)
    }
}

impl std::error::Error for LexError {}

/// Tokenize a complete query/update string.
pub fn tokenize(src: &str) -> Result<Vec<Token>, LexError> {
    Lexer {
        src,
        buf: src.as_bytes(),
        at: 0,
    }
    .run()
}

/// Tokenize for editor highlighting: never fails. An unlexable lexeme becomes a
/// [`TokenKind::Error`] span and the scan resynchronizes past it, so every byte
/// belongs to exactly one token or to inter-token trivia, and the pass always
/// terminates (docs/10 §3.2–3.3). The strict [`tokenize`] is unchanged.
pub fn tokenize_resilient(src: &str) -> Vec<Token> {
    Lexer {
        src,
        buf: src.as_bytes(),
        at: 0,
    }
    .run_resilient()
}

struct Lexer<'a> {
    src: &'a str,
    buf: &'a [u8],
    at: usize,
}

impl<'a> Lexer<'a> {
    fn run(mut self) -> Result<Vec<Token>, LexError> {
        let mut out = Vec::new();
        loop {
            self.skip_trivia();
            let start = self.at;
            let Some(&b) = self.buf.get(self.at) else {
                return Ok(out);
            };
            let kind = self.next_kind(b, start)?;
            out.push(Token {
                kind,
                span: self.span_from(start),
            });
        }
    }

    /// Like [`run`](Self::run) but never fails: a lexer error becomes a
    /// [`TokenKind::Error`] token and the scan forces past the offending lexeme
    /// (docs/10 §3.3). Guarantees forward progress of ≥1 byte per step, landing
    /// on UTF-8 boundaries.
    fn run_resilient(mut self) -> Vec<Token> {
        let mut out = Vec::new();
        loop {
            self.skip_trivia();
            let start = self.at;
            let Some(&b) = self.buf.get(self.at) else {
                return out;
            };
            let kind = match self.next_kind(b, start) {
                Ok(kind) => kind,
                Err(_) => {
                    // If `next_kind` already consumed the malformed lexeme, take
                    // that as the error span; otherwise force one char forward.
                    if self.at <= start {
                        self.at = start + 1;
                        while self.at < self.buf.len() && (self.buf[self.at] & 0xC0) == 0x80 {
                            self.at += 1;
                        }
                    }
                    TokenKind::Error
                }
            };
            out.push(Token {
                kind,
                span: self.span_from(start),
            });
        }
    }

    fn next_kind(&mut self, b: u8, start: usize) -> Result<TokenKind, LexError> {
        match b {
            b'<' => self.lt(start),
            b'>' => Ok(self.take2(b'=', TokenKind::Ge, TokenKind::Gt, TokenKind::GtGt, b'>')),
            b'(' => Ok(self.bracket_pair(b')', TokenKind::Nil, TokenKind::LParen)),
            b')' => {
                if self.buf[self.at..].starts_with(b")>>") {
                    self.at += 3;
                    Ok(TokenKind::RParenGtGt)
                } else {
                    self.at += 1;
                    Ok(TokenKind::RParen)
                }
            }
            b'[' => Ok(self.bracket_pair(b']', TokenKind::Anon, TokenKind::LBracket)),
            b']' => Ok(self.punct1(TokenKind::RBracket)),
            b'{' => {
                self.at += 1;
                if self.buf.get(self.at) == Some(&b'|') {
                    self.at += 1;
                    Ok(TokenKind::LBraceBar)
                } else {
                    Ok(TokenKind::LBrace)
                }
            }
            b'}' => Ok(self.punct1(TokenKind::RBrace)),
            b'|' => {
                self.at += 1;
                match self.buf.get(self.at) {
                    Some(b'|') => {
                        self.at += 1;
                        Ok(TokenKind::OrOr)
                    }
                    Some(b'}') => {
                        self.at += 1;
                        Ok(TokenKind::RBarBrace)
                    }
                    _ => Ok(TokenKind::Pipe),
                }
            }
            b'&' => {
                if self.buf[self.at..].starts_with(b"&&") {
                    self.at += 2;
                    Ok(TokenKind::AndAnd)
                } else {
                    Err(self.err_at(start, "expected `&&`"))
                }
            }
            b'^' => {
                self.at += 1;
                if self.buf.get(self.at) == Some(&b'^') {
                    self.at += 1;
                    Ok(TokenKind::CaretCaret)
                } else {
                    Ok(TokenKind::Caret)
                }
            }
            b'=' => Ok(self.punct1(TokenKind::Eq)),
            b'!' => {
                self.at += 1;
                if self.buf.get(self.at) == Some(&b'=') {
                    self.at += 1;
                    Ok(TokenKind::Ne)
                } else {
                    Ok(TokenKind::Bang)
                }
            }
            b';' => Ok(self.punct1(TokenKind::Semicolon)),
            b',' => Ok(self.punct1(TokenKind::Comma)),
            b'*' => Ok(self.punct1(TokenKind::Star)),
            b'/' => Ok(self.punct1(TokenKind::Slash)),
            b'~' => Ok(self.punct1(TokenKind::Tilde)),
            b'?' | b'$' => self.var_or_question(start),
            b'"' | b'\'' => self.string(start, b),
            b'@' => self.langtag(start),
            b'+' | b'-' => {
                // A directly attached sign is part of the numeric token
                // (the grammar's NumericLiteralPositive/Negative — the
                // AdditiveExpression production accounts for it).
                match self.buf.get(self.at + 1) {
                    Some(d) if d.is_ascii_digit() => {
                        self.at += 1;
                        self.number(start)
                    }
                    Some(b'.')
                        if self
                            .buf
                            .get(self.at + 2)
                            .is_some_and(|d| d.is_ascii_digit()) =>
                    {
                        self.at += 1;
                        self.number(start)
                    }
                    _ => Ok(self.punct1(if b == b'+' {
                        TokenKind::Plus
                    } else {
                        TokenKind::Minus
                    })),
                }
            }
            b'.' => {
                if self
                    .buf
                    .get(self.at + 1)
                    .is_some_and(|d| d.is_ascii_digit())
                {
                    self.number(start)
                } else {
                    Ok(self.punct1(TokenKind::Dot))
                }
            }
            b'0'..=b'9' => self.number(start),
            b'_' => self.blank_node(start),
            b':' => self.pname(start),
            _ => self.word_or_pname(start),
        }
    }

    // ------------------------------------------------------------ trivia

    fn skip_trivia(&mut self) {
        loop {
            match self.buf.get(self.at) {
                Some(b' ' | b'\t' | b'\r' | b'\n') => self.at += 1,
                Some(b'#') => {
                    while !matches!(self.buf.get(self.at), None | Some(b'\n')) {
                        self.at += 1;
                    }
                }
                _ => return,
            }
        }
    }

    // ------------------------------------------------------------ helpers

    fn span_from(&self, start: usize) -> Span {
        Span {
            start: start as u32,
            end: self.at as u32,
        }
    }

    fn err_at(&self, at: usize, message: impl Into<String>) -> LexError {
        LexError {
            span: Span {
                start: at as u32,
                end: (at + 1).min(self.buf.len()) as u32,
            },
            message: message.into(),
        }
    }

    fn punct1(&mut self, kind: TokenKind) -> TokenKind {
        self.at += 1;
        kind
    }

    /// `X=` → `eq_kind`, `XX` → `double_kind` (where the second byte is
    /// `double`), else `single_kind`.
    fn take2(
        &mut self,
        eq: u8,
        eq_kind: TokenKind,
        single_kind: TokenKind,
        double_kind: TokenKind,
        double: u8,
    ) -> TokenKind {
        self.at += 1;
        match self.buf.get(self.at) {
            Some(&b) if b == eq => {
                self.at += 1;
                eq_kind
            }
            Some(&b) if b == double => {
                self.at += 1;
                double_kind
            }
            _ => single_kind,
        }
    }

    /// `(` + ws + `)` → NIL (and the `[ ]` ANON analogue), else the open
    /// bracket alone. Only whitespace may sit inside per the grammar.
    fn bracket_pair(&mut self, close: u8, pair: TokenKind, open: TokenKind) -> TokenKind {
        let mut j = self.at + 1;
        while matches!(self.buf.get(j), Some(b' ' | b'\t' | b'\r' | b'\n')) {
            j += 1;
        }
        if self.buf.get(j) == Some(&close) {
            self.at = j + 1;
            pair
        } else {
            self.at += 1;
            open
        }
    }

    fn char_at(&self, i: usize) -> Option<char> {
        self.src[i..].chars().next()
    }

    // ------------------------------------------------------------ tokens

    /// `<`: IRIREF if a well-formed one starts here (SPARQL IRIREFs have
    /// no escapes; any forbidden byte before `>` disqualifies), else the
    /// `<=` / `<<(` / `<<` / `<` operators.
    fn lt(&mut self, start: usize) -> Result<TokenKind, LexError> {
        let mut j = self.at + 1;
        loop {
            match self.buf.get(j) {
                Some(b'>') => {
                    self.at = j + 1;
                    return Ok(TokenKind::IriRef);
                }
                Some(b'\\') => {
                    let Some(&kind @ (b'u' | b'U')) = self.buf.get(j + 1) else {
                        break;
                    };
                    let n = if kind == b'u' { 4 } else { 8 };
                    let Some(hex) = self
                        .buf
                        .get(j + 2..j + 2 + n)
                        .filter(|h| h.iter().all(u8::is_ascii_hexdigit))
                    else {
                        return Err(self.err_at(j, "malformed Unicode escape in IRI"));
                    };
                    let value =
                        u32::from_str_radix(std::str::from_utf8(hex).expect("ASCII hex"), 16)
                            .expect("checked hex");
                    if char::from_u32(value).is_none() {
                        return Err(self.err_at(j, "IRI escape is not a Unicode scalar value"));
                    }
                    j += n + 2;
                }
                Some(&b) if !is_forbidden_iri_byte(b) || b >= 0x80 => j += 1,
                _ => break,
            }
        }
        let _ = start;
        if self.buf[self.at..].starts_with(b"<<(") {
            self.at += 3;
            Ok(TokenKind::LtLtParen)
        } else if self.buf[self.at..].starts_with(b"<<") {
            self.at += 2;
            Ok(TokenKind::LtLt)
        } else if self.buf[self.at..].starts_with(b"<=") {
            self.at += 2;
            Ok(TokenKind::Le)
        } else {
            self.at += 1;
            Ok(TokenKind::Lt)
        }
    }

    /// `?name` / `$name`, or the path operator `?`.
    fn var_or_question(&mut self, start: usize) -> Result<TokenKind, LexError> {
        let sigil = self.buf[self.at];
        let name_at = self.at + 1;
        match self.char_at(name_at) {
            Some(c) if is_pn_chars_u(c) || c.is_ascii_digit() => {
                self.at = name_at + c.len_utf8();
                while let Some(c) = self.char_at(self.at) {
                    if is_varname_char(c) {
                        self.at += c.len_utf8();
                    } else {
                        break;
                    }
                }
                Ok(TokenKind::Var)
            }
            _ if sigil == b'?' => {
                self.at += 1;
                Ok(TokenKind::Question)
            }
            _ => Err(self.err_at(start, "expected a variable name after `$`")),
        }
    }

    /// All four string forms; escapes are validated (shape only — decoding
    /// is the parser's job) and long strings may hold unescaped newlines
    /// and up to two consecutive quote characters.
    fn string(&mut self, start: usize, q: u8) -> Result<TokenKind, LexError> {
        let long = self.buf[self.at..].starts_with(&[q, q, q]);
        let form = match (q, long) {
            (b'"', false) => StringForm::Quote,
            (b'"', true) => StringForm::LongQuote,
            (b'\'', false) => StringForm::Apos,
            (_, false) => StringForm::Apos,
            (_, true) => StringForm::LongApos,
        };
        self.at += if long { 3 } else { 1 };
        loop {
            match self.buf.get(self.at) {
                None => return Err(self.err_at(start, "unterminated string literal")),
                Some(&b) if b == q => {
                    if !long {
                        self.at += 1;
                        return Ok(TokenKind::String(form));
                    }
                    if self.buf[self.at..].starts_with(&[q, q, q]) {
                        self.at += 3;
                        return Ok(TokenKind::String(form));
                    }
                    self.at += 1;
                }
                Some(b'\\') => {
                    let esc_at = self.at;
                    self.at += 1;
                    match self.buf.get(self.at) {
                        Some(b't' | b'b' | b'n' | b'r' | b'f' | b'"' | b'\'' | b'\\') => {
                            self.at += 1;
                        }
                        Some(b'u') => self.hex_escape(esc_at, 4)?,
                        Some(b'U') => self.hex_escape(esc_at, 8)?,
                        _ => return Err(self.err_at(esc_at, "invalid string escape")),
                    }
                }
                Some(b'\n' | b'\r') if !long => {
                    return Err(self.err_at(start, "newline in single-line string"));
                }
                Some(_) => self.at += 1,
            }
        }
    }

    /// `\uXXXX` / `\UXXXXXXXX` after the backslash; `self.at` sits on the
    /// `u`. Validates hex count and that the scalar is a valid char.
    fn hex_escape(&mut self, esc_at: usize, n: usize) -> Result<(), LexError> {
        let ds = self.at + 1;
        let hex = self
            .buf
            .get(ds..ds + n)
            .filter(|h| h.iter().all(u8::is_ascii_hexdigit))
            .ok_or_else(|| self.err_at(esc_at, "malformed \\u escape"))?;
        let v = u32::from_str_radix(std::str::from_utf8(hex).expect("ascii hex"), 16)
            .expect("checked hex");
        if char::from_u32(v).is_none() {
            return Err(self.err_at(esc_at, "escape is not a Unicode scalar value"));
        }
        self.at = ds + n;
        Ok(())
    }

    /// `@tag(-sub)*(--ltr|--rtl)?` — SPARQL has no `@prefix`/`@base`, so
    /// `@` always opens a language tag.
    fn langtag(&mut self, start: usize) -> Result<TokenKind, LexError> {
        self.at += 1;
        let tag_start = self.at;
        while matches!(self.buf.get(self.at), Some(b) if b.is_ascii_alphabetic()) {
            self.at += 1;
        }
        if self.at == tag_start {
            return Err(self.err_at(start, "expected a language tag after `@`"));
        }
        let mut dir = None;
        while self.buf.get(self.at) == Some(&b'-') {
            if self.buf.get(self.at + 1) == Some(&b'-') {
                let ds = self.at + 2;
                let mut j = ds;
                while matches!(self.buf.get(j), Some(b) if b.is_ascii_alphabetic()) {
                    j += 1;
                }
                dir = match &self.buf[ds..j] {
                    b"ltr" => Some(Dir::Ltr),
                    b"rtl" => Some(Dir::Rtl),
                    _ => return Err(self.err_at(self.at, "base direction must be --ltr or --rtl")),
                };
                self.at = j;
                break;
            }
            let sub_start = self.at + 1;
            let mut j = sub_start;
            while matches!(self.buf.get(j), Some(b) if b.is_ascii_alphanumeric()) {
                j += 1;
            }
            if j == sub_start {
                return Err(self.err_at(self.at, "empty language subtag"));
            }
            self.at = j;
        }
        Ok(TokenKind::LangTag(dir))
    }

    /// INTEGER / DECIMAL / DOUBLE (sign, if any, already consumed).
    fn number(&mut self, start: usize) -> Result<TokenKind, LexError> {
        while matches!(self.buf.get(self.at), Some(b) if b.is_ascii_digit()) {
            self.at += 1;
        }
        let mut decimal = false;
        // DECIMAL requires digits after the dot; `1.` is INTEGER then Dot.
        if self.buf.get(self.at) == Some(&b'.')
            && self
                .buf
                .get(self.at + 1)
                .is_some_and(|d| d.is_ascii_digit())
        {
            decimal = true;
            self.at += 1;
            while matches!(self.buf.get(self.at), Some(b) if b.is_ascii_digit()) {
                self.at += 1;
            }
        }
        if matches!(self.buf.get(self.at), Some(b'e' | b'E')) {
            let mut j = self.at + 1;
            if matches!(self.buf.get(j), Some(b'+' | b'-')) {
                j += 1;
            }
            if !matches!(self.buf.get(j), Some(d) if d.is_ascii_digit()) {
                return Err(self.err_at(start, "exponent needs at least one digit"));
            }
            self.at = j;
            while matches!(self.buf.get(self.at), Some(b) if b.is_ascii_digit()) {
                self.at += 1;
            }
            return Ok(TokenKind::Double);
        }
        Ok(if decimal {
            TokenKind::Decimal
        } else {
            TokenKind::Integer
        })
    }

    /// `_:label` (first char `PN_CHARS_U` or digit; interior dots allowed,
    /// no trailing dot).
    fn blank_node(&mut self, start: usize) -> Result<TokenKind, LexError> {
        if self.buf.get(self.at + 1) != Some(&b':') {
            return Err(self.err_at(start, "expected `_:` to open a blank node label"));
        }
        self.at += 2;
        match self.char_at(self.at) {
            Some(c) if is_pn_chars_u(c) || c.is_ascii_digit() => self.at += c.len_utf8(),
            _ => return Err(self.err_at(start, "blank node label needs at least one character")),
        }
        let mut last_dot = None;
        while let Some(c) = self.char_at(self.at) {
            if is_pn_chars(c) {
                last_dot = None;
                self.at += c.len_utf8();
            } else if c == '.' {
                last_dot = Some(self.at);
                self.at += 1;
            } else {
                break;
            }
        }
        if let Some(d) = last_dot {
            // Trailing dots belong to the statement, not the label.
            self.at = d;
        }
        Ok(TokenKind::BlankNode)
    }

    /// From a `:` — a prefixed name with an empty prefix.
    fn pname(&mut self, start: usize) -> Result<TokenKind, LexError> {
        self.at += 1;
        self.pn_local(start)
    }

    /// A bare word: `PN_PREFIX` then `:` makes it a PNAME; otherwise it
    /// must be a keyword, `a`, or a boolean literal.
    fn word_or_pname(&mut self, start: usize) -> Result<TokenKind, LexError> {
        match self.char_at(self.at) {
            Some(c) if is_pn_chars_base(c) => self.at += c.len_utf8(),
            _ => return Err(self.err_at(start, "unexpected character")),
        }
        let mut last_dot = None;
        while let Some(c) = self.char_at(self.at) {
            if is_pn_chars(c) {
                last_dot = None;
                self.at += c.len_utf8();
            } else if c == '.' {
                last_dot = Some(self.at);
                self.at += 1;
            } else {
                break;
            }
        }
        if let Some(d) = last_dot {
            self.at = d;
        }
        if self.buf.get(self.at) == Some(&b':') {
            self.at += 1;
            return self.pn_local(start);
        }
        let word = &self.src[start..self.at];
        if word == "a" {
            return Ok(TokenKind::A);
        }
        // Keywords are ASCII and bounded; skip allocation for long words.
        if word.len() <= 14 && word.is_ascii() {
            let upper = word.to_ascii_uppercase();
            match upper.as_str() {
                "TRUE" => return Ok(TokenKind::True),
                "FALSE" => return Ok(TokenKind::False),
                u => {
                    if let Some(kw) = Kw::from_upper(u) {
                        return Ok(TokenKind::Keyword(kw));
                    }
                }
            }
        }
        Err(self.err_at(start, format!("unknown keyword `{word}`")))
    }

    /// The optional local part after the `:` of a prefixed name
    /// (`PN_LOCAL`: leading `PN_CHARS_U`/digit/`:`/PLX, interior dots, no
    /// trailing dot, `%XX` and `\`-escapes).
    fn pn_local(&mut self, start: usize) -> Result<TokenKind, LexError> {
        let mut any = false;
        let mut last_dot = None;
        let mut first = true;
        while let Some(c) = self.char_at(self.at) {
            let ok = match c {
                ':' => true,
                '%' => {
                    if !self
                        .buf
                        .get(self.at + 1..self.at + 3)
                        .is_some_and(|h| h.iter().all(u8::is_ascii_hexdigit))
                    {
                        return Err(self.err_at(self.at, "malformed %-escape in local name"));
                    }
                    self.at += 2; // the loop tail adds the '%' itself
                    true
                }
                '\\' => {
                    let Some(&e) = self.buf.get(self.at + 1) else {
                        return Err(self.err_at(self.at, "dangling escape in local name"));
                    };
                    if !is_pn_local_esc(e) {
                        return Err(self.err_at(self.at, "invalid character escape in local name"));
                    }
                    self.at += 1;
                    true
                }
                '.' if !first => {
                    last_dot = Some(self.at);
                    self.at += 1;
                    continue;
                }
                c if first => is_pn_chars_u(c) || c.is_ascii_digit(),
                c => is_pn_chars(c),
            };
            if !ok {
                break;
            }
            last_dot = None;
            self.at += c.len_utf8();
            any = true;
            first = false;
        }
        if let Some(d) = last_dot {
            self.at = d;
        }
        let _ = start;
        Ok(if any {
            TokenKind::PNameLn
        } else {
            TokenKind::PNameNs
        })
    }
}
