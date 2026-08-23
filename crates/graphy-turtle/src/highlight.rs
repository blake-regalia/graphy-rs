//! Resilient tokenizer for editor highlighting (docs/10 §3, §7).
//!
//! Unlike the streaming drivers, this pass never fails: for any input byte
//! string it terminates and emits a sequence of classified spans that never
//! overlap and are strictly increasing, covering every lexically meaningful
//! byte (whitespace and comments are the only gaps). Garbage runs become
//! [`HlKind::Error`] tokens; a construct left open at end of input becomes a
//! *provisional* token of its intended kind, so an IRI or string being typed
//! keeps its colour instead of flickering to an error (docs/10 §7.3).
//!
//! The whole document is fed at once — the LSP holds the full buffer, so the
//! driver's chunk-boundary incrementality (for ingest) is not needed here.
//! Classification is purely lexical: predicate-vs-object and other
//! position-dependent colours are enriched later from the parse (docs/10 §7.1).

use crate::lexer::{Lexer, Token};

/// A lexical highlight class. Mapped to LSP semantic-token types in
/// `graphy-lsp`; kept grammar-facing (not LSP-facing) so this crate carries no
/// protocol dependency.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HlKind {
    /// `<…>` IRI reference (absolute or relative).
    Iri,
    /// The `prefix:` portion of a prefixed name (namespace), colon included.
    PrefixName,
    /// The local portion of a prefixed name.
    LocalName,
    /// `_:label` blank node.
    BlankNode,
    /// String literal of any quote form.
    String,
    /// `@tag` / `@tag--dir` language tag.
    LangTag,
    /// Integer / decimal / double numeric literal.
    Number,
    /// `true` / `false`.
    Boolean,
    /// `a`, `@prefix`, `@base`, `PREFIX`, `BASE`, `GRAPH`, `@version`,
    /// `VERSION`.
    Keyword,
    /// Structural punctuation: `. ; , ( ) [ ] { }`.
    Punct,
    /// `^^`, `<<`, `>>`, `<<(`, `)>>`, `{|`, `|}`, `~`.
    Operator,
    /// An unlexable run (or an unterminated construct not covered by a
    /// provisional token).
    Error,
}

/// A classified byte span. `start`/`end` are absolute byte offsets into the
/// source (`start < end` for every emitted token).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HlToken {
    pub start: u32,
    pub end: u32,
    pub kind: HlKind,
}

impl HlToken {
    fn new(start: usize, end: usize, kind: HlKind) -> HlToken {
        HlToken {
            start: start as u32,
            end: end as u32,
            kind,
        }
    }
}

/// Tokenize `src` for highlighting. Never fails; see the module docs for the
/// invariants. `src.len()` must fit in `u32` (documents past 4 GiB are not an
/// editor concern).
pub fn tokenize(src: &[u8]) -> Vec<HlToken> {
    let mut lx = Lexer::new();
    lx.feed(src);
    lx.set_eof();

    let mut out = Vec::new();
    // Start of an in-flight error run, if one is open.
    let mut err_start: Option<usize> = None;

    loop {
        match lx.next() {
            Ok(Some(Token::Eof)) => break,
            Ok(Some(tok)) => {
                let start = lx.token_start();
                let end = lx.pos();
                flush_err(&mut out, &mut err_start, start);
                push(&mut out, tok, start, end, src);
            }
            // `set_eof` was called, so `Ok(None)` cannot occur here: every
            // "need more input" branch in the lexer is gated on `!eof`.
            Ok(None) => break,
            Err(e) => {
                let at = e.offset as usize;
                let ts = lx.token_start();
                // An "unterminated" error reports its offset at the token
                // start (the cursor has not advanced) and — because we set EOF
                // before scanning — only ever fires when the construct runs to
                // the end of the buffer. When that construct opens an IRI or a
                // string, emit a *provisional* token of the intended kind so an
                // edit in progress keeps its colour instead of flickering to an
                // error (docs/10 §7.3). Single-char errors (`|`, `^`, `>`) also
                // report at the start but carry a different opener byte, so the
                // guard below excludes them.
                let opener = matches!(src.get(ts), Some(b'<' | b'"' | b'\''));
                if at == ts && opener {
                    flush_err(&mut out, &mut err_start, ts);
                    let kind = match src[ts] {
                        b'<' => HlKind::Iri,
                        _ => HlKind::String,
                    };
                    out.push(HlToken::new(ts, src.len(), kind));
                    break;
                }
                // Garbage mid-buffer: open (or extend) an error run and force
                // the cursor forward so the scan always makes progress.
                err_start.get_or_insert(ts.min(at));
                lx.recover_past(at);
            }
        }
    }
    // A trailing error run (garbage right up to EOF) never got flushed above.
    if let Some(s) = err_start {
        if s < src.len() {
            out.push(HlToken::new(s, src.len(), HlKind::Error));
        }
    }
    out
}

/// Emit any open error run as a single [`HlKind::Error`] token ending where the
/// next real token begins, then clear it.
fn flush_err(out: &mut Vec<HlToken>, err_start: &mut Option<usize>, upto: usize) {
    if let Some(s) = err_start.take() {
        if s < upto {
            out.push(HlToken::new(s, upto, HlKind::Error));
        }
    }
}

/// Classify one completed token spanning `[start, end)` and push its span(s).
fn push(out: &mut Vec<HlToken>, tok: Token, start: usize, end: usize, src: &[u8]) {
    let one = |k| HlToken::new(start, end, k);
    match tok {
        Token::Iri(_) => out.push(one(HlKind::Iri)),
        Token::Pname { .. } => {
            // Split at the first `:` — the namespace/local boundary. The
            // prefix part (PN_PREFIX) contains no colon, so the first one is
            // always the separator, even when the local part carries `\:`.
            let colon = memchr::memchr(b':', &src[start..end])
                .map(|i| start + i)
                .unwrap_or(end);
            out.push(HlToken::new(start, colon + 1, HlKind::PrefixName));
            if colon + 1 < end {
                out.push(HlToken::new(colon + 1, end, HlKind::LocalName));
            }
        }
        Token::BlankLabel(_) => out.push(one(HlKind::BlankNode)),
        Token::String { .. } => out.push(one(HlKind::String)),
        Token::LangTag { .. } => out.push(one(HlKind::LangTag)),
        Token::Integer(_) | Token::Decimal(_) | Token::Double(_) => out.push(one(HlKind::Number)),
        Token::KwTrue | Token::KwFalse => out.push(one(HlKind::Boolean)),
        Token::KwA
        | Token::KwPrefixAt
        | Token::KwBaseAt
        | Token::KwPrefixSparql
        | Token::KwBaseSparql
        | Token::KwGraph
        | Token::KwVersionAt
        | Token::KwVersionSparql => out.push(one(HlKind::Keyword)),
        Token::Dot
        | Token::Semicolon
        | Token::Comma
        | Token::LParen
        | Token::RParen
        | Token::LBracket
        | Token::RBracket
        | Token::LBrace
        | Token::RBrace => out.push(one(HlKind::Punct)),
        Token::DoubleCaret
        | Token::LtLt
        | Token::GtGt
        | Token::LtLtParen
        | Token::RParenGtGt
        | Token::AnnoOpen
        | Token::AnnoClose
        | Token::Tilde => out.push(one(HlKind::Operator)),
        Token::Eof => {}
    }
}
