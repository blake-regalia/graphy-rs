//! Document symbols / outline (docs/10 §10, tier 1).
//!
//! Lexer-tier symbols only — everything derivable accurately from the token
//! stream: prefix declarations (all formats), the SPARQL query form, and
//! top-level JSON-LD keys. Turtle **subjects** need exact statement boundaries
//! and land with the recovering parser in M11b; emitting them from a lexical
//! heuristic would mislabel directives and nested nodes, so they are omitted
//! here rather than shipped wrong.

use graphy_sparql_syntax::{tokenize_resilient, Kw, TokenKind};
use graphy_turtle::{highlight_tokens, HlKind};

use crate::jsonld::{next_nonws_is_colon, scan_string};
use crate::line_index::{utf16_len, LineIndex};

/// What an outline entry denotes. Maps to an LSP `SymbolKind` in the server
/// layer (e.g. Namespace → `Namespace`, Query → `Function`, Key → `Field`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SymKind {
    /// A prefix declaration (`@prefix ex:` / `PREFIX ex:`).
    Namespace,
    /// The top-level SPARQL query form (SELECT/CONSTRUCT/DESCRIBE/ASK).
    Query,
    /// A top-level JSON-LD object key.
    Key,
}

/// An outline entry. `line`/`start`/`len` are the LSP selection range (0-based
/// line, UTF-16 character/length); the server uses it for both range and
/// selectionRange at this tier.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Symbol {
    pub name: String,
    pub kind: SymKind,
    pub line: u32,
    pub start: u32,
    pub len: u32,
}

fn at(li: &LineIndex, src: &str, name: &str, kind: SymKind, start: u32, end: u32) -> Symbol {
    let (line, col) = li.position(src, start);
    Symbol {
        name: name.to_string(),
        kind,
        line,
        start: col,
        len: utf16_len(&src[start as usize..end as usize]),
    }
}

/// Outline for a Turtle/TriG document: its prefix declarations.
pub fn turtle_symbols(src: &str) -> Vec<Symbol> {
    let li = LineIndex::new(src);
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
        // The declared prefix is the *immediately* following PrefixName token
        // (the grammar allows nothing in between; a broken declaration emits no
        // symbol rather than stealing a name from a later statement).
        if let Some(p) = toks.get(k + 1).filter(|n| n.kind == HlKind::PrefixName) {
            let name = &src[p.start as usize..p.end as usize];
            out.push(at(&li, src, name, SymKind::Namespace, p.start, p.end));
        }
    }
    out
}

/// Outline for a SPARQL query/update: the query form and its prefix
/// declarations.
pub fn sparql_symbols(src: &str) -> Vec<Symbol> {
    let li = LineIndex::new(src);
    let toks = tokenize_resilient(src);
    let mut out = Vec::new();
    let mut seen_form = false;
    for (k, t) in toks.iter().enumerate() {
        match t.kind {
            TokenKind::Keyword(Kw::Select | Kw::Construct | Kw::Describe | Kw::Ask)
                if !seen_form =>
            {
                seen_form = true;
                let name = &src[t.span.start as usize..t.span.end as usize];
                out.push(at(&li, src, name, SymKind::Query, t.span.start, t.span.end));
            }
            TokenKind::Keyword(Kw::Prefix) => {
                // Immediately following only — see turtle_symbols.
                if let Some(ns) = toks.get(k + 1).filter(|n| n.kind == TokenKind::PNameNs) {
                    let name = &src[ns.span.start as usize..ns.span.end as usize];
                    out.push(at(
                        &li,
                        src,
                        name,
                        SymKind::Namespace,
                        ns.span.start,
                        ns.span.end,
                    ));
                }
            }
            _ => {}
        }
    }
    out
}

/// Outline for a JSON-LD document: the keys of the root object — or, when the
/// document is a root *array* of node objects (common in JSON-LD), the keys of
/// each object directly in that array.
pub fn jsonld_symbols(src: &str) -> Vec<Symbol> {
    let li = LineIndex::new(src);
    let b = src.as_bytes();
    let mut out = Vec::new();
    // Container stack: `true` = object, `false` = array.
    let mut stack: Vec<bool> = Vec::new();
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'"' => {
                let (end, terminated) = scan_string(b, i);
                let top_level = matches!(stack.as_slice(), [true] | [false, true]);
                if terminated && top_level && next_nonws_is_colon(b, end) {
                    let name = &src[i + 1..end - 1]; // inner text, without quotes
                    out.push(at(&li, src, name, SymKind::Key, i as u32, end as u32));
                }
                i = end;
            }
            b'{' => {
                stack.push(true);
                i += 1;
            }
            b'[' => {
                stack.push(false);
                i += 1;
            }
            b'}' => {
                // Pop only a matching opener (lenient on unbalanced input).
                if stack.last() == Some(&true) {
                    stack.pop();
                }
                i += 1;
            }
            b']' => {
                if stack.last() == Some(&false) {
                    stack.pop();
                }
                i += 1;
            }
            _ => i += 1,
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn turtle_prefix_outline() {
        let src = "@prefix ex: <http://e/> .\n@prefix rdf: <http://r/> .\nex:s ex:p ex:o .";
        let syms = turtle_symbols(src);
        assert_eq!(syms.len(), 2);
        assert_eq!(syms[0].name, "ex:");
        assert_eq!((syms[0].line, syms[0].start), (0, 8));
        assert_eq!(syms[1].name, "rdf:");
        assert_eq!(syms[1].line, 1);
        assert!(syms.iter().all(|s| s.kind == SymKind::Namespace));
    }

    #[test]
    fn sparql_form_and_prefixes() {
        let src = "PREFIX ex: <http://e/>\nSELECT ?s WHERE { ?s ex:p ?o }";
        let syms = sparql_symbols(src);
        assert_eq!(syms[0].name, "ex:");
        assert_eq!(syms[0].kind, SymKind::Namespace);
        assert!(syms
            .iter()
            .any(|s| s.kind == SymKind::Query && s.name == "SELECT"));
        // Only one query-form symbol.
        assert_eq!(syms.iter().filter(|s| s.kind == SymKind::Query).count(), 1);
    }

    #[test]
    fn jsonld_top_level_keys_only() {
        let src = r#"{"@context": {"nested": "x"}, "@id": "y", "items": [1, 2]}"#;
        let syms = jsonld_symbols(src);
        let names: Vec<_> = syms.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, vec!["@context", "@id", "items"]); // "nested" is depth 2
        assert!(syms.iter().all(|s| s.kind == SymKind::Key));
    }

    #[test]
    fn jsonld_root_array_of_node_objects() {
        // A root array is a valid JSON-LD document; each direct object's keys
        // are the outline. Deeper nesting stays hidden.
        let src = r#"[{"@id": "a", "deep": {"x": 1}}, {"@id": "b"}]"#;
        let names: Vec<_> = jsonld_symbols(src).into_iter().map(|s| s.name).collect();
        assert_eq!(names, vec!["@id", "deep", "@id"]);
    }

    #[test]
    fn broken_prefix_decl_steals_no_later_name() {
        // `@prefix` with its name missing must not grab `ex:` from line 2.
        let syms = turtle_symbols("@prefix <http://e/> .\nex:s ex:p ex:o .");
        assert!(syms.is_empty(), "stole a name: {syms:?}");
        let syms = sparql_symbols("PREFIX <http://e/>\nSELECT * { ex:s ?p ?o }");
        assert!(syms.iter().all(|s| s.kind != SymKind::Namespace));
    }

    #[test]
    fn never_panics_on_partial() {
        for src in ["@prefix", "PREFIX ex:", "SELECT", "{\"a\":", "{{}", ""] {
            let _ = turtle_symbols(src);
            let _ = sparql_symbols(src);
            let _ = jsonld_symbols(src);
        }
    }
}
