//! Diagnostics (docs/10 §9, M11b tier 2).
//!
//! Byte-ranged, language-specific diagnostics; the server maps them to LSP
//! `Diagnostic`s via [`LineIndex`](crate::LineIndex). Sources per language:
//!
//! - **Turtle family**: a lenient (recovering) TriG parse — TriG is the
//!   superset of NT/NQ/Turtle — surfaces every accumulated `ParseError`, so a
//!   doc with one bad statement gets one squiggle, not zero features. Plus an
//!   unused-prefix lint from the lexer tier.
//! - **SPARQL**: the fail-fast parser (query vs update chosen by the first
//!   non-prologue keyword) localizes the first syntax error; on a clean parse
//!   the `graphy-algebra` translation adds semantic scope errors. The
//!   *recovering* SPARQL parser (multiple syntax errors) is the next M11b
//!   increment. Same unused-prefix lint.
//! - **JSON-LD**: JSON well-formedness (first structural error) plus an
//!   unknown-`@`-keyword lint on object keys.

use std::collections::HashSet;

use graphy_algebra::{translate_query, translate_update};
use graphy_sparql_syntax::{
    parse_query_recovering, parse_update_recovering, tokenize_resilient, Kw, ParseError, Token,
    TokenKind,
};
use graphy_turtle::{highlight_tokens, HlKind, Options, TriGParser};

use crate::jsonld::scan_string;

/// A prefix declaration found in the token stream: (name-with-colon, name
/// span start/end, whole-statement span when its shape was intact).
type PrefixDecl<'a> = (&'a str, u32, u32, Option<(u32, u32)>);

/// Diagnostic severity (maps 1:1 onto the LSP severities we emit).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sev {
    Error,
    Warning,
}

/// A language-level diagnostic over a byte span of the source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diag {
    pub start: u32,
    pub end: u32,
    pub sev: Sev,
    pub message: String,
    /// An auto-applicable fix (LSP quick fix / code action), when one exists.
    pub fix: Option<Fix>,
}

/// What a fix does — lets aggregate actions (`source.fixAll`,
/// `source.removeUnusedImports`) select by category.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FixKind {
    RemoveUnusedPrefix,
    DeclareWellKnownPrefix,
}

/// A quick fix: a title and the byte-span edits that implement it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fix {
    pub title: String,
    pub kind: FixKind,
    pub edits: Vec<FixEdit>,
}

/// One replacement: substitute `text` for the byte range `[start, end)`
/// (empty `text` = deletion; `start == end` = insertion).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FixEdit {
    pub start: u32,
    pub end: u32,
    pub text: String,
}

fn diag(start: u32, end: u32, sev: Sev, message: impl Into<String>) -> Diag {
    Diag {
        start,
        end,
        sev,
        message: message.into(),
        fix: None,
    }
}

/// Extend a statement's byte span to a whole-line deletion: leading
/// indentation is included when the statement starts its line, and one
/// trailing line break goes with it so no blank line is left behind.
fn full_line_span(src: &str, start: u32, end: u32) -> (u32, u32) {
    let b = src.as_bytes();
    let mut s = start as usize;
    while s > 0 && matches!(b[s - 1], b' ' | b'\t') {
        s -= 1;
    }
    if !(s == 0 || b[s - 1] == b'\n') {
        s = start as usize; // mid-line: delete only the statement itself
    } else {
        let mut e = end as usize;
        if b.get(e) == Some(&b'\r') && b.get(e + 1) == Some(&b'\n') {
            return (s as u32, (e + 2) as u32);
        }
        if b.get(e) == Some(&b'\n') {
            e += 1;
        }
        return (s as u32, e as u32);
    }
    (s as u32, end)
}

/// The word-ish range starting at byte `at`: through the next whitespace (≥1
/// byte when not at EOF), clamped to char boundaries so position mapping and
/// slicing stay safe even for offsets inside multi-byte garbage.
fn word_range(src: &str, at: usize) -> (u32, u32) {
    let b = src.as_bytes();
    let mut start = at.min(src.len());
    while start > 0 && !src.is_char_boundary(start) {
        start -= 1;
    }
    let mut end = start;
    while end < b.len() && !b[end].is_ascii_whitespace() {
        end += 1;
    }
    if end == start && end < b.len() {
        end += 1;
        while end < b.len() && !src.is_char_boundary(end) {
            end += 1;
        }
    }
    (start as u32, end as u32)
}

// ------------------------------------------------------------ Turtle family

/// Diagnostics for a Turtle/TriG/N-Triples/N-Quads document. `base` should be
/// the document's own URI (RFC 3986 §5.1.3 — that's what relative references
/// in the file resolve against in an editor); without it, valid documents
/// that use `<>`/relative IRIs would flag spurious "without a base" errors.
pub fn turtle_diagnostics(src: &str, base: Option<&str>) -> Vec<Diag> {
    let mut out = Vec::new();
    let opts = Options {
        lenient: true,
        base: base.map(String::from),
        ..Options::default()
    };
    match TriGParser::new(opts) {
        Ok(mut parser) => {
            // Lenient mode accumulates into `errors()`; a returned Err (a
            // failure lenient recovery cannot absorb) is surfaced the same way.
            let fatal = parser
                .feed(src.as_bytes())
                .err()
                .or_else(|| parser.finish().err());
            for e in parser.errors() {
                let (start, end) = word_range(src, e.offset as usize);
                out.push(diag(start, end, Sev::Error, &e.message));
            }
            if let Some(e) = fatal {
                let (start, end) = word_range(src, e.offset as usize);
                let d = diag(start, end, Sev::Error, &e.message);
                if out.last() != Some(&d) {
                    out.push(d);
                }
            }
        }
        Err(e) => {
            let (start, end) = word_range(src, e.offset as usize);
            out.push(diag(start, end, Sev::Error, &e.message));
        }
    }
    turtle_unused_prefixes(src, &mut out);
    attach_missing_prefix_fixes(
        &mut out,
        ('"', '"'),
        turtle_decl_insert_at(src),
        |n, iri| format!("@prefix {n}: <{iri}> .\n"),
    );
    out
}

/// Warn on `@prefix`/`PREFIX` declarations whose name is never used, with a
/// quick fix that deletes the whole declaration statement.
fn turtle_unused_prefixes(src: &str, out: &mut Vec<Diag>) {
    let toks = highlight_tokens(src.as_bytes());
    // (name, name span, whole-declaration span when its shape is intact)
    let mut declared: Vec<PrefixDecl> = Vec::new();
    let mut decl_idx: HashSet<usize> = HashSet::new();
    for (k, t) in toks.iter().enumerate() {
        if t.kind != HlKind::Keyword {
            continue;
        }
        let word = src[t.start as usize..t.end as usize].to_ascii_lowercase();
        if word != "@prefix" && word != "prefix" {
            continue;
        }
        if let Some(p) = toks.get(k + 1).filter(|n| n.kind == HlKind::PrefixName) {
            let stmt = match (toks.get(k + 2), toks.get(k + 3)) {
                (Some(iri), Some(dot))
                    if iri.kind == HlKind::Iri
                        && dot.kind == HlKind::Punct
                        && &src[dot.start as usize..dot.end as usize] == "." =>
                {
                    Some((t.start, dot.end))
                }
                // SPARQL-style `PREFIX ex: <iri>` has no terminating dot.
                (Some(iri), _) if iri.kind == HlKind::Iri && word == "prefix" => {
                    Some((t.start, iri.end))
                }
                _ => None,
            };
            declared.push((&src[p.start as usize..p.end as usize], p.start, p.end, stmt));
            decl_idx.insert(k + 1);
        }
    }
    if declared.is_empty() {
        return;
    }
    let used: HashSet<&str> = toks
        .iter()
        .enumerate()
        .filter(|(k, t)| t.kind == HlKind::PrefixName && !decl_idx.contains(k))
        .map(|(_, t)| &src[t.start as usize..t.end as usize])
        .collect();
    for (name, start, end, stmt) in declared {
        if !used.contains(name) {
            let mut d = diag(
                start,
                end,
                Sev::Warning,
                format!("unused prefix declaration `{name}`"),
            );
            if let Some((s, e)) = stmt {
                let (s, e) = full_line_span(src, s, e);
                d.fix = Some(Fix {
                    title: format!("Remove unused prefix declaration `{name}`"),
                    kind: FixKind::RemoveUnusedPrefix,
                    edits: vec![FixEdit {
                        start: s,
                        end: e,
                        text: String::new(),
                    }],
                });
            }
            out.push(d);
        }
    }
}

/// Attach "insert a declaration" fixes to `undeclared prefix` errors whose
/// name is in the well-known table. `template` renders the declaration;
/// `insert_at` is where new declarations go (after the last existing one).
fn attach_missing_prefix_fixes(
    out: &mut [Diag],
    quote: (char, char),
    insert_at: u32,
    template: impl Fn(&str, &str) -> String,
) {
    let mut fixed: HashSet<String> = HashSet::new();
    for d in out.iter_mut() {
        if d.sev != Sev::Error || d.fix.is_some() {
            continue;
        }
        let Some(rest) = d.message.strip_prefix("undeclared prefix ") else {
            continue;
        };
        let name = rest
            .trim_start_matches(quote.0)
            .trim_end_matches(quote.1)
            .trim_end_matches(':');
        let Some((_, iri)) = crate::completion::WELL_KNOWN_PREFIXES
            .iter()
            .find(|(n, _)| *n == name)
        else {
            continue;
        };
        if !fixed.insert(name.to_string()) {
            continue; // one insert action per prefix is enough
        }
        d.fix = Some(Fix {
            title: format!("Declare well-known prefix `{name}:`"),
            kind: FixKind::DeclareWellKnownPrefix,
            edits: vec![FixEdit {
                start: insert_at,
                end: insert_at,
                text: template(name, iri),
            }],
        });
    }
}

/// Insertion point for a new declaration: just past the last intact one
/// (whole line), else the start of the document.
fn turtle_decl_insert_at(src: &str) -> u32 {
    let toks = highlight_tokens(src.as_bytes());
    let mut at = 0;
    for (k, t) in toks.iter().enumerate() {
        if t.kind != HlKind::Keyword {
            continue;
        }
        let word = src[t.start as usize..t.end as usize].to_ascii_lowercase();
        if word != "@prefix" && word != "prefix" {
            continue;
        }
        if let (Some(iri), Some(dot)) = (toks.get(k + 2), toks.get(k + 3)) {
            if iri.kind == HlKind::Iri
                && dot.kind == HlKind::Punct
                && &src[dot.start as usize..dot.end as usize] == "."
            {
                at = at.max(full_line_span(src, t.start, dot.end).1);
            }
        }
    }
    at
}

// ------------------------------------------------------------------- SPARQL

/// Diagnostics for a SPARQL query or update. The recovering parser reports
/// *every* localized syntax error (group-anchor / `;`-boundary resync); the
/// algebra translation runs only on a syntactically clean parse, adding
/// semantic scope errors.
pub fn sparql_diagnostics(src: &str) -> Vec<Diag> {
    let mut out = Vec::new();
    let toks = tokenize_resilient(src);
    match sparql_form(&toks) {
        SparqlForm::Query => sparql_query_diags(src, &mut out),
        SparqlForm::Update => sparql_update_diags(src, &mut out),
        // No recognizable form keyword: report whichever reading produced
        // fewer complaints (it understood more of the document).
        SparqlForm::Unknown => {
            let mut as_query = Vec::new();
            sparql_query_diags(src, &mut as_query);
            if as_query.is_empty() {
                // Clean as a query — done.
            } else {
                let mut as_update = Vec::new();
                sparql_update_diags(src, &mut as_update);
                if as_update.len() < as_query.len() {
                    as_query = as_update;
                }
            }
            out.append(&mut as_query);
        }
    }
    sparql_unused_prefixes(src, &toks, &mut out);
    let insert_at = sparql_decl_insert_at(src, &toks);
    attach_missing_prefix_fixes(&mut out, ('`', '`'), insert_at, |n, iri| {
        format!("PREFIX {n}: <{iri}>\n")
    });
    out
}

/// Insertion point for a new `PREFIX` declaration: just past the last intact
/// one (whole line), else the start of the document.
fn sparql_decl_insert_at(src: &str, toks: &[Token]) -> u32 {
    let mut at = 0;
    for (k, t) in toks.iter().enumerate() {
        if t.kind != TokenKind::Keyword(Kw::Prefix) {
            continue;
        }
        if let (Some(ns), Some(iri)) = (toks.get(k + 1), toks.get(k + 2)) {
            if ns.kind == TokenKind::PNameNs && iri.kind == TokenKind::IriRef {
                at = at.max(full_line_span(src, t.span.start, iri.span.end).1);
            }
        }
    }
    at
}

fn push_parse_errors(src: &str, errors: &[ParseError], out: &mut Vec<Diag>) {
    for e in errors {
        out.push(span_diag(src, e.span.start, e.span.end, &e.message));
    }
}

fn sparql_query_diags(src: &str, out: &mut Vec<Diag>) {
    let (tree, errors) = parse_query_recovering(src);
    push_parse_errors(src, &errors, out);
    if errors.is_empty() {
        if let Some(q) = tree {
            if let Err(e) = translate_query(&q) {
                out.push(span_diag(src, e.span.start, e.span.end, &e.message));
            }
        }
    }
}

fn sparql_update_diags(src: &str, out: &mut Vec<Diag>) {
    let (tree, errors) = parse_update_recovering(src);
    push_parse_errors(src, &errors, out);
    if errors.is_empty() {
        if let Some(u) = tree {
            if let Err(e) = translate_update(&u) {
                out.push(span_diag(src, e.span.start, e.span.end, &e.message));
            }
        }
    }
}

/// A span-based Error diagnostic, widened to a word when the span is empty.
fn span_diag(src: &str, start: u32, end: u32, message: &str) -> Diag {
    if end > start {
        diag(start, end.min(src.len() as u32), Sev::Error, message)
    } else {
        let (s, e) = word_range(src, start as usize);
        diag(s, e, Sev::Error, message)
    }
}

enum SparqlForm {
    Query,
    Update,
    Unknown,
}

/// Classify by the first non-prologue keyword (prologue = PREFIX/BASE).
fn sparql_form(toks: &[Token]) -> SparqlForm {
    for t in toks {
        if let TokenKind::Keyword(kw) = t.kind {
            match kw {
                Kw::Prefix | Kw::Base => continue,
                Kw::Select | Kw::Construct | Kw::Describe | Kw::Ask => return SparqlForm::Query,
                Kw::Insert
                | Kw::Delete
                | Kw::Load
                | Kw::Clear
                | Kw::Create
                | Kw::Drop
                | Kw::Copy
                | Kw::Move
                | Kw::Add
                | Kw::With => return SparqlForm::Update,
                _ => return SparqlForm::Unknown,
            }
        }
    }
    SparqlForm::Unknown
}

/// Warn on `PREFIX` declarations whose name is never used (bare `PNameNs`
/// references and the namespace part of `PNameLn` both count as uses), with
/// a quick fix that deletes the declaration.
fn sparql_unused_prefixes(src: &str, toks: &[Token], out: &mut Vec<Diag>) {
    let mut declared: Vec<PrefixDecl> = Vec::new();
    let mut decl_idx: HashSet<usize> = HashSet::new();
    for (k, t) in toks.iter().enumerate() {
        if t.kind != TokenKind::Keyword(Kw::Prefix) {
            continue;
        }
        if let Some(ns) = toks.get(k + 1).filter(|n| n.kind == TokenKind::PNameNs) {
            let stmt = toks
                .get(k + 2)
                .filter(|n| n.kind == TokenKind::IriRef)
                .map(|iri| (t.span.start, iri.span.end));
            declared.push((
                &src[ns.span.start as usize..ns.span.end as usize],
                ns.span.start,
                ns.span.end,
                stmt,
            ));
            decl_idx.insert(k + 1);
        }
    }
    if declared.is_empty() {
        return;
    }
    let mut used: HashSet<&str> = HashSet::new();
    for (k, t) in toks.iter().enumerate() {
        match t.kind {
            TokenKind::PNameNs if !decl_idx.contains(&k) => {
                used.insert(&src[t.span.start as usize..t.span.end as usize]);
            }
            TokenKind::PNameLn => {
                let text = &src[t.span.start as usize..t.span.end as usize];
                if let Some(colon) = text.find(':') {
                    used.insert(&text[..colon + 1]);
                }
            }
            _ => {}
        }
    }
    for (name, start, end, stmt) in declared {
        if !used.contains(name) {
            let mut d = diag(
                start,
                end,
                Sev::Warning,
                format!("unused prefix declaration `{name}`"),
            );
            if let Some((s, e)) = stmt {
                let (s, e) = full_line_span(src, s, e);
                d.fix = Some(Fix {
                    title: format!("Remove unused prefix declaration `{name}`"),
                    kind: FixKind::RemoveUnusedPrefix,
                    edits: vec![FixEdit {
                        start: s,
                        end: e,
                        text: String::new(),
                    }],
                });
            }
            out.push(d);
        }
    }
}

// ------------------------------------------------------------------ JSON-LD

/// The JSON-LD 1.1 keyword set (plus `@annotation` from JSON-LD-star), for
/// the unknown-keyword lint on object keys and for completion.
pub(crate) const JSONLD_KEYWORDS: &[&str] = &[
    "@annotation",
    "@base",
    "@container",
    "@context",
    "@direction",
    "@graph",
    "@id",
    "@import",
    "@included",
    "@index",
    "@json",
    "@language",
    "@list",
    "@nest",
    "@none",
    "@prefix",
    "@propagate",
    "@protected",
    "@reverse",
    "@set",
    "@type",
    "@value",
    "@version",
    "@vocab",
];

/// Diagnostics for a JSON-LD document: the first structural JSON error (JSON
/// has no useful resync anchor, so one at a time) plus keyword-typo lints.
pub fn jsonld_diagnostics(src: &str) -> Vec<Diag> {
    let mut out = Vec::new();
    if let Err((at, msg)) = validate_json(src.as_bytes()) {
        let (start, end) = word_range(src, at);
        out.push(diag(start, end, Sev::Error, msg));
    }
    jsonld_keyword_lints(src, &mut out);
    out
}

/// Warn on object keys that look like JSON-LD keywords but aren't.
fn jsonld_keyword_lints(src: &str, out: &mut Vec<Diag>) {
    let b = src.as_bytes();
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'"' => {
                let (end, terminated) = scan_string(b, i);
                if terminated
                    && b.get(i + 1) == Some(&b'@')
                    && crate::jsonld::next_nonws_is_colon(b, end)
                {
                    let key = &src[i + 1..end - 1];
                    if !JSONLD_KEYWORDS.contains(&key) {
                        out.push(diag(
                            i as u32,
                            end as u32,
                            Sev::Warning,
                            format!("unknown JSON-LD keyword `{key}`"),
                        ));
                    }
                }
                i = end;
            }
            _ => i += 1,
        }
    }
}

const JSON_MAX_DEPTH: u32 = 256;

/// Structural JSON validation; `Err((byte, message))` on the first problem.
fn validate_json(b: &[u8]) -> Result<(), (usize, String)> {
    let mut i = skip_ws(b, 0);
    if i >= b.len() {
        return Err((0, "expected a JSON value".to_string()));
    }
    i = json_value(b, i, 0)?;
    i = skip_ws(b, i);
    if i < b.len() {
        return Err((i, "unexpected content after the JSON value".to_string()));
    }
    Ok(())
}

fn skip_ws(b: &[u8], mut i: usize) -> usize {
    while matches!(b.get(i), Some(b' ' | b'\t' | b'\r' | b'\n')) {
        i += 1;
    }
    i
}

/// Parse one JSON value starting at non-ws `i`; returns the index just past it.
fn json_value(b: &[u8], i: usize, depth: u32) -> Result<usize, (usize, String)> {
    if depth > JSON_MAX_DEPTH {
        return Err((i, "JSON nesting too deep".to_string()));
    }
    match b.get(i) {
        None => Err((i, "unexpected end of input".to_string())),
        Some(b'{') => json_container(b, i, depth, b'}', true),
        Some(b'[') => json_container(b, i, depth, b']', false),
        Some(b'"') => json_string(b, i),
        Some(b'-' | b'0'..=b'9') => json_number(b, i),
        Some(b't') => json_literal(b, i, "true"),
        Some(b'f') => json_literal(b, i, "false"),
        Some(b'n') => json_literal(b, i, "null"),
        Some(&c) => Err((i, format!("unexpected character {:?}", c as char))),
    }
}

/// Object (`want_keys`) or array body, starting at the opener.
fn json_container(
    b: &[u8],
    open: usize,
    depth: u32,
    close: u8,
    want_keys: bool,
) -> Result<usize, (usize, String)> {
    let mut i = skip_ws(b, open + 1);
    if b.get(i) == Some(&close) {
        return Ok(i + 1);
    }
    loop {
        if want_keys {
            if b.get(i) != Some(&b'"') {
                return Err((i, "expected an object key string".to_string()));
            }
            i = json_string(b, i)?;
            i = skip_ws(b, i);
            if b.get(i) != Some(&b':') {
                return Err((i, "expected `:` after object key".to_string()));
            }
            i = skip_ws(b, i + 1);
        }
        i = json_value(b, i, depth + 1)?;
        i = skip_ws(b, i);
        match b.get(i) {
            Some(b',') => i = skip_ws(b, i + 1),
            Some(&c) if c == close => return Ok(i + 1),
            _ => {
                let what = if close == b'}' {
                    "`,` or `}`"
                } else {
                    "`,` or `]`"
                };
                return Err((i, format!("expected {what}")));
            }
        }
    }
}

fn json_string(b: &[u8], open: usize) -> Result<usize, (usize, String)> {
    let mut j = open + 1;
    while j < b.len() {
        match b[j] {
            b'"' => return Ok(j + 1),
            b'\\' => match b.get(j + 1) {
                Some(b'"' | b'\\' | b'/' | b'b' | b'f' | b'n' | b'r' | b't') => j += 2,
                Some(b'u') => {
                    let hex = b.get(j + 2..j + 6);
                    let ok = hex.is_some_and(|h| h.iter().all(u8::is_ascii_hexdigit));
                    if !ok {
                        return Err((j, "invalid \\u escape".to_string()));
                    }
                    j += 6;
                }
                _ => return Err((j, "invalid escape".to_string())),
            },
            0x00..=0x1f => return Err((j, "raw control character in string".to_string())),
            _ => j += 1,
        }
    }
    Err((open, "unterminated string".to_string()))
}

fn json_number(b: &[u8], start: usize) -> Result<usize, (usize, String)> {
    let mut i = start;
    if b.get(i) == Some(&b'-') {
        i += 1;
    }
    let int_start = i;
    while matches!(b.get(i), Some(b'0'..=b'9')) {
        i += 1;
    }
    if i == int_start {
        return Err((start, "invalid number".to_string()));
    }
    if b[int_start] == b'0' && i - int_start > 1 {
        return Err((start, "leading zero in number".to_string()));
    }
    if b.get(i) == Some(&b'.') {
        i += 1;
        let frac = i;
        while matches!(b.get(i), Some(b'0'..=b'9')) {
            i += 1;
        }
        if i == frac {
            return Err((start, "digits required after decimal point".to_string()));
        }
    }
    if matches!(b.get(i), Some(b'e' | b'E')) {
        i += 1;
        if matches!(b.get(i), Some(b'+' | b'-')) {
            i += 1;
        }
        let exp = i;
        while matches!(b.get(i), Some(b'0'..=b'9')) {
            i += 1;
        }
        if i == exp {
            return Err((start, "digits required in exponent".to_string()));
        }
    }
    Ok(i)
}

fn json_literal(b: &[u8], start: usize, word: &str) -> Result<usize, (usize, String)> {
    if b[start..].starts_with(word.as_bytes()) {
        Ok(start + word.len())
    } else {
        Err((start, format!("expected `{word}`")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn msgs(diags: &[Diag]) -> Vec<&str> {
        diags.iter().map(|d| d.message.as_str()).collect()
    }

    #[test]
    fn turtle_recovering_parse_reports_every_bad_statement() {
        let src = "@prefix ex: <http://x/> .\n\
                   ex:s ex:p BROKEN .\n\
                   ex:s2 GARBAGE!!! more .\n\
                   ex:ok ex:p ex:o .";
        let diags = turtle_diagnostics(src, None);
        let errors: Vec<_> = diags.iter().filter(|d| d.sev == Sev::Error).collect();
        assert!(errors.len() >= 2, "want ≥2 errors, got {diags:?}");
        // Errors land on the broken lines, not the valid ones.
        let line2 = src.find("BROKEN").unwrap() as u32;
        let line3 = src.find("GARBAGE").unwrap() as u32;
        let line4 = src.find("ex:ok").unwrap() as u32;
        assert!(errors.iter().any(|d| d.start >= line2 && d.start < line3));
        assert!(errors.iter().any(|d| d.start >= line3 && d.start < line4));
        assert!(errors.iter().all(|d| d.start < line4));
    }

    #[test]
    fn turtle_valid_doc_is_clean_and_unused_prefix_warns() {
        let src = "@prefix ex: <http://x/> .\n\
                   @prefix unused: <http://u/> .\n\
                   ex:s ex:p ex:o .";
        let diags = turtle_diagnostics(src, None);
        assert!(diags.iter().all(|d| d.sev == Sev::Warning), "{diags:?}");
        assert_eq!(diags.len(), 1);
        assert!(diags[0].message.contains("unused:"));
        // The warning points at the declared name.
        assert_eq!(
            &src[diags[0].start as usize..diags[0].end as usize],
            "unused:"
        );
    }

    #[test]
    fn earl_report_shape_is_clean_with_doc_base() {
        // Regression (2026-07-19, W3C Turtle EARL report): a valid document
        // using `<>` plus many IRIs with `.` in their content flagged one
        // "without a base" error and then an "expected ':' after prefix"
        // cascade — byte-level lenient resync landed inside the dotted IRIs.
        let src = "@prefix doap: <http://usefulinc.com/ns/doap#> .\n\
                   @prefix earl: <http://www.w3.org/ns/earl#> .\n\
                   <> a doap:Project,\n\
                        earl:Software;\n\
                      doap:name \"Turtle\";\n\
                      earl:assertions <http://example.org/earl-eye-2013-08-19.ttl>,\n\
                        <http://example.org/raptor2012-earl-turtle.ttl>,\n\
                        <http://example.org/serd_turtle_tests_earl-2017-01-07.ttl>;\n\
                      earl:generatedBy <http://rubygems.org/gems/earl-report> .\n";
        let diags =
            turtle_diagnostics(src, Some("http://www.w3.org/2013/TurtleTests/manifest.ttl"));
        assert!(diags.is_empty(), "valid doc must be clean: {diags:?}");
        // Without a base the relative `<>` is a genuine (single!) error.
        let diags = turtle_diagnostics(src, None);
        let errors: Vec<_> = diags.iter().filter(|d| d.sev == Sev::Error).collect();
        assert_eq!(errors.len(), 1, "no cascade: {diags:?}");
        assert!(errors[0].message.contains("base"), "{diags:?}");
    }

    #[test]
    fn turtle_error_before_dotted_iris_does_not_cascade() {
        let src = "@prefix ex: <http://x/> .\n\
                   ex:s ex:p BROKEN .\n\
                   ex:a ex:b <http://w.example.org/x.ttl> .\n\
                   ex:c ex:d <http://w3.org/y.ttl> .";
        let diags = turtle_diagnostics(src, None);
        let errors: Vec<_> = diags.iter().filter(|d| d.sev == Sev::Error).collect();
        assert_eq!(
            errors.len(),
            1,
            "one broken statement = one error: {diags:?}"
        );
        assert!(errors[0].start >= src.find("BROKEN").unwrap() as u32 - 12);
        assert!(errors[0].end <= src.find("ex:a").unwrap() as u32);
    }

    #[test]
    fn sparql_syntax_error_is_localized() {
        let src = "SELECT ?s WHERE { ?s ?p }";
        let diags = sparql_diagnostics(src);
        assert_eq!(diags.len(), 1, "{diags:?}");
        assert_eq!(diags[0].sev, Sev::Error);
        // The error points inside the group, not at byte 0.
        assert!(diags[0].start >= src.find('{').unwrap() as u32);
    }

    #[test]
    fn sparql_update_form_is_detected() {
        let diags = sparql_diagnostics("INSERT DATA { <http://a/s> <http://a/p> }");
        assert_eq!(diags.len(), 1);
        assert_eq!(diags[0].sev, Sev::Error);
        let clean = sparql_diagnostics("INSERT DATA { <http://a/s> <http://a/p> \"o\" }");
        assert!(clean.is_empty(), "{clean:?}");
    }

    #[test]
    fn sparql_valid_query_clean_and_unused_prefix_warns() {
        assert!(sparql_diagnostics("SELECT * WHERE { ?s ?p ?o }").is_empty());
        let diags = sparql_diagnostics(
            "PREFIX ex: <http://e/>\nPREFIX u: <http://u/>\nSELECT * WHERE { ?s ex:p ?o }",
        );
        assert_eq!(diags.len(), 1, "{diags:?}");
        assert_eq!(diags[0].sev, Sev::Warning);
        assert!(diags[0].message.contains("`u:`"));
    }

    #[test]
    fn sparql_multiple_errors_all_report() {
        // Two broken group elements → two localized errors, not one.
        let src =
            "SELECT * WHERE {\n?s ?p ?o .\nFILTER() .\n?a ?b ?c .\nBIND(AS ?x) .\n?d ?e ?f\n}";
        let diags = sparql_diagnostics(src);
        let errors: Vec<_> = diags.iter().filter(|d| d.sev == Sev::Error).collect();
        assert_eq!(errors.len(), 2, "{diags:?}");
        let filter_at = src.find("FILTER").unwrap() as u32;
        let bind_at = src.find("BIND").unwrap() as u32;
        assert!(errors[0].start >= filter_at && errors[0].start < bind_at);
        assert!(errors[1].start >= bind_at);
    }

    #[test]
    fn sparql_translate_errors_surface() {
        // Parses, but §18.2 translation rejects the aggregate in a WHERE
        // filter — a semantic diagnostic from the algebra tier.
        let src = "SELECT ?s { ?s ?p ?o FILTER(SUM(?o) > 1) }";
        let diags = sparql_diagnostics(src);
        assert_eq!(diags.len(), 1, "{diags:?}");
        assert_eq!(diags[0].sev, Sev::Error);
        assert!(
            diags[0].message.contains("aggregate"),
            "expected the translate-tier message: {diags:?}"
        );
    }

    #[test]
    fn jsonld_structural_errors() {
        let d = jsonld_diagnostics(r#"{"a": "unterminated"#);
        assert_eq!(d.len(), 1);
        assert_eq!(d[0].sev, Sev::Error);

        let d = jsonld_diagnostics(r#"{"a": 1} trailing"#);
        assert!(d[0].message.contains("after the JSON value"), "{d:?}");

        let d = jsonld_diagnostics(r#"{"a": 01}"#);
        assert!(d[0].message.contains("leading zero"), "{d:?}");

        let d = jsonld_diagnostics("");
        assert!(d[0].message.contains("expected a JSON value"));

        assert!(jsonld_diagnostics(r#"{"a": [1, 2.5e3, true, null, "x"]}"#).is_empty());
    }

    #[test]
    fn jsonld_keyword_typo_lint() {
        let d = jsonld_diagnostics(r#"{"@contxt": {"x": "y"}, "@id": "a"}"#);
        assert_eq!(d.len(), 1, "{d:?}");
        assert_eq!(d[0].sev, Sev::Warning);
        assert!(d[0].message.contains("@contxt"));
        // Keyword-shaped *values* are not linted (e.g. "@type": "@id").
        assert!(jsonld_diagnostics(r#"{"@type": "@nonsense"}"#).is_empty());
    }

    /// Apply fix edits to a source string (back to front so spans stay valid).
    fn apply(src: &str, fix: &Fix) -> String {
        let mut edits = fix.edits.clone();
        edits.sort_by_key(|e| e.start);
        let mut out = src.to_string();
        for e in edits.iter().rev() {
            out.replace_range(e.start as usize..e.end as usize, &e.text);
        }
        out
    }

    #[test]
    fn unused_prefix_fix_deletes_the_declaration() {
        let src = "@prefix ex: <http://x/> .\n@prefix unused: <http://u/> .\nex:s ex:p ex:o .";
        let diags = turtle_diagnostics(src, None);
        let fix = diags[0].fix.as_ref().expect("deletion fix");
        assert!(fix.title.contains("unused:"));
        let fixed = apply(src, fix);
        assert_eq!(fixed, "@prefix ex: <http://x/> .\nex:s ex:p ex:o .");
        assert!(turtle_diagnostics(&fixed, None).is_empty());

        // SPARQL declaration (no trailing dot).
        let src = "PREFIX ex: <http://e/>\nPREFIX u: <http://u/>\nSELECT * WHERE { ?s ex:p ?o }";
        let diags = sparql_diagnostics(src);
        let fixed = apply(src, diags[0].fix.as_ref().expect("deletion fix"));
        assert_eq!(
            fixed,
            "PREFIX ex: <http://e/>\nSELECT * WHERE { ?s ex:p ?o }"
        );
        assert!(sparql_diagnostics(&fixed).is_empty());
    }

    #[test]
    fn undeclared_well_known_prefix_fix_inserts_declaration() {
        // `foaf:` is well-known → the error carries an insert fix, placed
        // after the last existing declaration; applying it makes the doc
        // clean (the inserted prefix is immediately used).
        let src = "@prefix ex: <http://x/> .\nex:s foaf:knows ex:o .";
        let diags = turtle_diagnostics(src, None);
        let with_fix: Vec<_> = diags.iter().filter(|d| d.fix.is_some()).collect();
        assert_eq!(with_fix.len(), 1, "{diags:?}");
        let fix = with_fix[0].fix.as_ref().unwrap();
        assert!(fix.title.contains("foaf:"), "{fix:?}");
        let fixed = apply(src, fix);
        assert!(
            fixed.starts_with(
                "@prefix ex: <http://x/> .\n@prefix foaf: <http://xmlns.com/foaf/0.1/> .\n"
            ),
            "{fixed}"
        );
        assert!(turtle_diagnostics(&fixed, None).is_empty(), "{fixed}");

        // SPARQL flavor.
        let src = "PREFIX ex: <http://e/>\nSELECT * WHERE { ?s foaf:knows ?o . ?s ex:p ?o }";
        let diags = sparql_diagnostics(src);
        let with_fix: Vec<_> = diags.iter().filter(|d| d.fix.is_some()).collect();
        assert_eq!(with_fix.len(), 1, "{diags:?}");
        let fixed = apply(src, with_fix[0].fix.as_ref().unwrap());
        assert!(
            fixed.contains("PREFIX ex: <http://e/>\nPREFIX foaf: <http://xmlns.com/foaf/0.1/>\n"),
            "{fixed}"
        );
        assert!(sparql_diagnostics(&fixed).is_empty(), "{fixed}");
    }

    #[test]
    fn unknown_prefix_gets_no_insert_fix() {
        let diags = turtle_diagnostics("ex:s zzz:p ex:o .", None);
        assert!(
            diags
                .iter()
                .filter(|d| d.message.contains("zzz"))
                .all(|d| d.fix.is_none()),
            "{diags:?}"
        );
    }

    #[test]
    fn diagnostics_never_panic_on_garbage() {
        for src in [
            "",
            "\u{0}\u{1}",
            "@prefix",
            "PREFIX ex:",
            "SELECT",
            "{{{",
            "\"\\u12",
            "ex:s ex:p \u{1F600} .",
            "<<<>>>",
        ] {
            let _ = turtle_diagnostics(src, None);
            let _ = sparql_diagnostics(src);
            let _ = jsonld_diagnostics(src);
        }
    }

    #[test]
    fn messages_are_present() {
        for d in turtle_diagnostics("ex:s ex:p BROKEN .", None) {
            assert!(!d.message.is_empty());
            assert!(d.end >= d.start);
        }
        assert!(!msgs(&sparql_diagnostics("SELECT WHERE")).is_empty());
    }
}
