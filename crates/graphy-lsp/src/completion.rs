//! Completion (docs/10 §10, M11b tier 2) — lexical-context candidates:
//!
//! - **Prefix names**: the document's own declarations first, then a vendored
//!   well-known table (a prefix.cc-style snapshot — never the network).
//! - **Local names**: distinct locals already used under the typed prefix.
//! - **Keywords**: the full SPARQL reserved-word list (from the lexer's own
//!   table, so it can't drift), Turtle directives, JSON-LD `@`-keywords.
//! - **Variables**: every `?var`/`$var` already present in a SPARQL doc.
//!
//! The client filters candidates against the typed word, so these functions
//! return the full candidate set for the detected position.

use std::collections::BTreeSet;

use graphy_sparql_syntax::{tokenize_resilient, Kw, TokenKind};
use graphy_turtle::{highlight_tokens, HlKind};

use crate::diagnostics::JSONLD_KEYWORDS;

/// What a completion candidate is (maps to an LSP `CompletionItemKind`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompKind {
    Prefix,
    LocalName,
    Keyword,
    Variable,
}

/// One completion candidate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Completion {
    pub label: String,
    pub kind: CompKind,
    /// Shown dimmed next to the label (the namespace IRI for prefixes).
    pub detail: Option<String>,
}

fn item(label: impl Into<String>, kind: CompKind, detail: Option<String>) -> Completion {
    Completion {
        label: label.into(),
        kind,
        detail,
    }
}

/// A vendored well-known-prefix snapshot (top prefix.cc entries plus the
/// vocabularies this project touches). Offline by design — docs/10 §10.
pub const WELL_KNOWN_PREFIXES: &[(&str, &str)] = &[
    ("as", "https://www.w3.org/ns/activitystreams#"),
    ("cc", "http://creativecommons.org/ns#"),
    ("csvw", "http://www.w3.org/ns/csvw#"),
    ("dc", "http://purl.org/dc/terms/"),
    ("dcat", "http://www.w3.org/ns/dcat#"),
    ("dcterms", "http://purl.org/dc/terms/"),
    ("doap", "http://usefulinc.com/ns/doap#"),
    ("earl", "http://www.w3.org/ns/earl#"),
    ("foaf", "http://xmlns.com/foaf/0.1/"),
    ("geo", "http://www.opengis.net/ont/geosparql#"),
    ("gr", "http://purl.org/goodrelations/v1#"),
    ("hydra", "http://www.w3.org/ns/hydra/core#"),
    ("ldp", "http://www.w3.org/ns/ldp#"),
    (
        "mf",
        "http://www.w3.org/2001/sw/DataAccess/tests/test-manifest#",
    ),
    ("oa", "http://www.w3.org/ns/oa#"),
    ("odrl", "http://www.w3.org/ns/odrl/2/"),
    ("org", "http://www.w3.org/ns/org#"),
    ("owl", "http://www.w3.org/2002/07/owl#"),
    ("prov", "http://www.w3.org/ns/prov#"),
    ("qudt", "http://qudt.org/schema/qudt/"),
    ("rdf", "http://www.w3.org/1999/02/22-rdf-syntax-ns#"),
    ("rdfs", "http://www.w3.org/2000/01/rdf-schema#"),
    ("schema", "https://schema.org/"),
    ("sd", "http://www.w3.org/ns/sparql-service-description#"),
    ("sh", "http://www.w3.org/ns/shacl#"),
    ("sioc", "http://rdfs.org/sioc/ns#"),
    ("skos", "http://www.w3.org/2004/02/skos/core#"),
    ("sosa", "http://www.w3.org/ns/sosa/"),
    ("ssn", "http://www.w3.org/ns/ssn/"),
    ("time", "http://www.w3.org/2006/time#"),
    ("vann", "http://purl.org/vocab/vann/"),
    ("vcard", "http://www.w3.org/2006/vcard/ns#"),
    ("void", "http://rdfs.org/ns/void#"),
    ("wd", "http://www.wikidata.org/entity/"),
    ("wdt", "http://www.wikidata.org/prop/direct/"),
    ("xsd", "http://www.w3.org/2001/XMLSchema#"),
];

/// The word being typed at byte `at`: from the last break byte to `at`.
/// Word bytes cover pnames, vars, directives, and keywords; quotes, brackets,
/// and whitespace break.
fn word_before(src: &str, at: usize) -> &str {
    let mut at = at.min(src.len());
    while !src.is_char_boundary(at) {
        at -= 1;
    }
    let b = src.as_bytes();
    let mut start = at;
    while start > 0 {
        let c = b[start - 1];
        let word_byte = c.is_ascii_alphanumeric()
            || matches!(c, b'_' | b'-' | b':' | b'?' | b'$' | b'@' | b'.')
            || c >= 0x80;
        if !word_byte {
            break;
        }
        start -= 1;
    }
    &src[start..at]
}

// ------------------------------------------------------------ Turtle family

/// `@prefix`/`PREFIX` declarations as (name-with-colon, iri) pairs, read from
/// the highlight tokens (keyword → PrefixName → Iri). Shared with hover.
pub(crate) fn turtle_decls(src: &str) -> Vec<(String, String)> {
    let toks = highlight_tokens(src.as_bytes());
    let mut out = Vec::new();
    for (k, t) in toks.iter().enumerate() {
        if t.kind != HlKind::Keyword {
            continue;
        }
        let word = src[t.start as usize..t.end as usize].to_ascii_lowercase();
        if word != "@prefix" && word != "prefix" {
            continue;
        }
        if let Some(p) = toks.get(k + 1).filter(|n| n.kind == HlKind::PrefixName) {
            let name = src[p.start as usize..p.end as usize].to_string();
            let iri = toks
                .get(k + 2)
                .filter(|n| n.kind == HlKind::Iri)
                .map(|n| src[n.start as usize + 1..n.end as usize - 1].to_string())
                .unwrap_or_default();
            out.push((name, iri));
        }
    }
    out
}

/// Completions for a Turtle-family document at byte offset `at`.
pub fn turtle_completions(src: &str, at: usize) -> Vec<Completion> {
    let word = word_before(src, at);
    // Local-name position: `ex:…` typed (but not a blank node `_:…`).
    if let Some(colon) = word.find(':') {
        if !word.starts_with("_:") {
            return local_names_turtle(src, &word[..colon + 1]);
        }
    }
    let mut out = Vec::new();
    if word.starts_with('@') {
        for d in ["@prefix", "@base", "@version"] {
            out.push(item(d, CompKind::Keyword, None));
        }
        return out;
    }
    prefix_candidates(turtle_decls(src), &mut out);
    for kw in ["PREFIX", "BASE", "GRAPH", "a", "true", "false"] {
        out.push(item(kw, CompKind::Keyword, None));
    }
    out
}

/// Distinct local names already used under `prefix` (Turtle: the highlighter
/// splits a pname into adjacent PrefixName + LocalName tokens).
fn local_names_turtle(src: &str, prefix: &str) -> Vec<Completion> {
    let toks = highlight_tokens(src.as_bytes());
    let mut names = BTreeSet::new();
    for (k, t) in toks.iter().enumerate() {
        if t.kind != HlKind::PrefixName || &src[t.start as usize..t.end as usize] != prefix {
            continue;
        }
        if let Some(l) = toks.get(k + 1).filter(|n| n.kind == HlKind::LocalName) {
            names.insert(&src[l.start as usize..l.end as usize]);
        }
    }
    names
        .into_iter()
        .map(|n| item(n, CompKind::LocalName, Some(prefix.to_string())))
        .collect()
}

/// Declared prefixes first, then undeclared well-known ones.
fn prefix_candidates(decls: Vec<(String, String)>, out: &mut Vec<Completion>) {
    let declared: BTreeSet<String> = decls.iter().map(|(n, _)| n.clone()).collect();
    for (name, iri) in &decls {
        out.push(item(name.clone(), CompKind::Prefix, Some(iri.clone())));
    }
    for (name, iri) in WELL_KNOWN_PREFIXES {
        let label = format!("{name}:");
        if !declared.contains(&label) {
            out.push(item(
                label,
                CompKind::Prefix,
                Some(format!("{iri} (well-known, undeclared)")),
            ));
        }
    }
}

// ------------------------------------------------------------------- SPARQL

/// Completions for a SPARQL document at byte offset `at`.
pub fn sparql_completions(src: &str, at: usize) -> Vec<Completion> {
    let word = word_before(src, at);
    let toks = tokenize_resilient(src);

    // Variable position.
    if word.starts_with('?') || word.starts_with('$') {
        let mut vars = BTreeSet::new();
        for t in &toks {
            if t.kind == TokenKind::Var {
                vars.insert(&src[t.span.start as usize..t.span.end as usize]);
            }
        }
        return vars
            .into_iter()
            .map(|v| item(v, CompKind::Variable, None))
            .collect();
    }

    // Local-name position.
    if let Some(colon) = word.find(':') {
        if !word.starts_with("_:") {
            let prefix = &word[..colon + 1];
            let mut names = BTreeSet::new();
            for t in &toks {
                if t.kind == TokenKind::PNameLn {
                    let text = &src[t.span.start as usize..t.span.end as usize];
                    if let Some(rest) = text.strip_prefix(prefix) {
                        names.insert(rest);
                    }
                }
            }
            return names
                .into_iter()
                .map(|n| item(n, CompKind::LocalName, Some(prefix.to_string())))
                .collect();
        }
    }

    // Prefixes (declared, from PREFIX decls) + well-known + every keyword.
    let decls = sparql_decls(src, &toks);
    let mut out = Vec::new();
    prefix_candidates(decls, &mut out);
    for kw in Kw::ALL {
        out.push(item(*kw, CompKind::Keyword, None));
    }
    out.push(item("a", CompKind::Keyword, None));
    out
}

/// `PREFIX` declarations as (name-with-colon, iri) pairs. Shared with hover.
pub(crate) fn sparql_decls(
    src: &str,
    toks: &[graphy_sparql_syntax::Token],
) -> Vec<(String, String)> {
    let mut decls = Vec::new();
    for (k, t) in toks.iter().enumerate() {
        if t.kind != TokenKind::Keyword(Kw::Prefix) {
            continue;
        }
        if let Some(ns) = toks.get(k + 1).filter(|n| n.kind == TokenKind::PNameNs) {
            let name = src[ns.span.start as usize..ns.span.end as usize].to_string();
            let iri = toks
                .get(k + 2)
                .filter(|n| n.kind == TokenKind::IriRef)
                .map(|n| src[n.span.start as usize + 1..n.span.end as usize - 1].to_string())
                .unwrap_or_default();
            decls.push((name, iri));
        }
    }
    decls
}

// ------------------------------------------------------------------ JSON-LD

/// Completions for a JSON-LD document at byte offset `at`: the keyword set,
/// offered when the cursor sits in a string that starts with `@`.
pub fn jsonld_completions(src: &str, at: usize) -> Vec<Completion> {
    let mut at = at.min(src.len());
    while !src.is_char_boundary(at) {
        at -= 1;
    }
    let b = src.as_bytes();
    // Walk back over keyword-ish bytes; we must land just past a `"`.
    let mut start = at;
    while start > 0 && (b[start - 1].is_ascii_alphanumeric() || b[start - 1] == b'@') {
        start -= 1;
    }
    if start == 0 || b[start - 1] != b'"' || !src[start..at].starts_with('@') {
        return Vec::new();
    }
    JSONLD_KEYWORDS
        .iter()
        .map(|k| item(*k, CompKind::Keyword, None))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn labels(items: &[Completion]) -> Vec<&str> {
        items.iter().map(|i| i.label.as_str()).collect()
    }

    #[test]
    fn turtle_prefix_position_lists_declared_then_well_known() {
        let src = "@prefix ex: <http://e/> .\nex:s a ex:T .\n";
        let items = turtle_completions(src, src.len());
        assert_eq!(items[0].label, "ex:");
        assert_eq!(items[0].kind, CompKind::Prefix);
        assert_eq!(items[0].detail.as_deref(), Some("http://e/"));
        assert!(labels(&items).contains(&"foaf:"));
        assert!(labels(&items).contains(&"a"));
    }

    #[test]
    fn turtle_local_names_under_prefix() {
        let src = "@prefix ex: <http://e/> .\nex:alpha ex:beta ex:gamma .\nex:s ex:p ex:";
        let items = turtle_completions(src, src.len());
        let ls = labels(&items);
        assert!(ls.contains(&"alpha") && ls.contains(&"beta") && ls.contains(&"gamma"));
        assert!(items.iter().all(|i| i.kind == CompKind::LocalName));
    }

    #[test]
    fn turtle_directive_position() {
        let src = "@pre";
        let items = turtle_completions(src, src.len());
        assert_eq!(labels(&items), vec!["@prefix", "@base", "@version"]);
    }

    #[test]
    fn sparql_variables_complete() {
        let src = "SELECT ?subject WHERE { ?subject ?pred ?obj . FILTER(?";
        let items = sparql_completions(src, src.len());
        let ls = labels(&items);
        assert!(ls.contains(&"?subject") && ls.contains(&"?pred") && ls.contains(&"?obj"));
        assert!(items.iter().all(|i| i.kind == CompKind::Variable));
    }

    #[test]
    fn sparql_keywords_and_prefixes() {
        let src = "PREFIX ex: <http://e/>\nSELECT * WHERE { ?s ex:p ?o } ";
        let items = sparql_completions(src, src.len());
        let ls = labels(&items);
        assert!(ls.contains(&"ex:"));
        assert!(ls.contains(&"OPTIONAL") && ls.contains(&"FILTER") && ls.contains(&"GROUP_CONCAT"));
        // Local names under a typed prefix.
        let src2 = "PREFIX ex: <http://e/>\nASK { ?s ex:knows ?o . ?o ex:";
        let items = sparql_completions(src2, src2.len());
        assert_eq!(labels(&items), vec!["knows"]);
    }

    #[test]
    fn jsonld_keyword_position_only_inside_at_string() {
        let src = r#"{"@con"#;
        let items = jsonld_completions(src, src.len());
        assert!(labels(&items).contains(&"@context"));
        // Not in a string → nothing.
        assert!(jsonld_completions("{}", 1).is_empty());
        // String not starting with @ → nothing.
        let src = r#"{"name"#;
        assert!(jsonld_completions(src, src.len()).is_empty());
    }

    #[test]
    fn never_panics_on_odd_offsets() {
        for src in ["", "ex:", "?", "\u{1F600}:x", "@", "\"@\""] {
            for at in 0..=src.len() + 2 {
                let _ = turtle_completions(src, at);
                let _ = sparql_completions(src, at);
                let _ = jsonld_completions(src, at);
            }
        }
    }
}
