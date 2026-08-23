//! Hover (docs/10 §10, M11b tier 2): prefixed-name expansion to the full
//! IRI (document declarations first, well-known table as a marked fallback),
//! SPARQL keyword/function signatures, shorthand-literal datatypes, language
//! tags, and JSON-LD keyword one-liners. Purely lexical — token at cursor.

use graphy_sparql_syntax::{tokenize_resilient, Kw, TokenKind};
use graphy_turtle::{highlight_tokens, HlKind};

use crate::completion::WELL_KNOWN_PREFIXES;
use crate::jsonld::scan_string;

/// A hover result: markdown for the byte span `[start, end)`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HoverInfo {
    pub start: u32,
    pub end: u32,
    pub markdown: String,
}

fn info(start: u32, end: u32, markdown: String) -> Option<HoverInfo> {
    Some(HoverInfo {
        start,
        end,
        markdown,
    })
}

/// Expansion for `name:` (with colon): declared IRI, or the well-known table
/// with an "undeclared" marker.
fn expand(decls: &[(String, String)], name: &str) -> Option<(String, bool)> {
    if let Some((_, iri)) = decls.iter().find(|(n, _)| n == name) {
        return Some((iri.clone(), true));
    }
    WELL_KNOWN_PREFIXES
        .iter()
        .find(|(n, _)| format!("{n}:") == name)
        .map(|(_, iri)| ((*iri).to_string(), false))
}

fn pname_markdown(decls: &[(String, String)], prefix: &str, local: &str) -> Option<String> {
    let (iri, declared) = expand(decls, prefix)?;
    let suffix = if declared {
        ""
    } else {
        " *(well-known, undeclared in this document)*"
    };
    Some(format!("**{prefix}{local}**\n\n<{iri}{local}>{suffix}"))
}

// ------------------------------------------------------------ Turtle family

/// Hover for a Turtle-family document at byte offset `at`.
pub fn turtle_hover(src: &str, at: usize) -> Option<HoverInfo> {
    let toks = highlight_tokens(src.as_bytes());
    let k = toks
        .iter()
        .position(|t| (t.start as usize) <= at && at < t.end as usize)?;
    let t = &toks[k];
    let text = &src[t.start as usize..t.end as usize];
    let decls = crate::completion::turtle_decls(src);
    match t.kind {
        HlKind::PrefixName => {
            // Hovering `ex:` of `ex:local` covers the whole pname.
            let (local, end) = match toks.get(k + 1).filter(|n| n.kind == HlKind::LocalName) {
                Some(l) => (&src[l.start as usize..l.end as usize], l.end),
                None => ("", t.end),
            };
            info(t.start, end, pname_markdown(&decls, text, local)?)
        }
        HlKind::LocalName => {
            let p = toks.get(k - 1).filter(|n| n.kind == HlKind::PrefixName)?;
            let prefix = &src[p.start as usize..p.end as usize];
            info(p.start, t.end, pname_markdown(&decls, prefix, text)?)
        }
        HlKind::LangTag => {
            let dir = if let Some(d) = text.split_once("--").map(|(_, d)| d) {
                format!(" with base direction `{d}`")
            } else {
                String::new()
            };
            info(
                t.start,
                t.end,
                format!("**{text}** — BCP 47 language tag{dir}"),
            )
        }
        HlKind::Number => {
            let dt = if text.contains(['e', 'E']) {
                "xsd:double"
            } else if text.contains('.') {
                "xsd:decimal"
            } else {
                "xsd:integer"
            };
            info(t.start, t.end, format!("`{text}` — shorthand for `{dt}`"))
        }
        HlKind::Boolean => info(
            t.start,
            t.end,
            format!("`{text}` — shorthand for `xsd:boolean`"),
        ),
        HlKind::Keyword => {
            let doc = match text.to_ascii_lowercase().as_str() {
                "a" => "shorthand for `rdf:type`",
                "@prefix" | "prefix" => "declares a namespace prefix",
                "@base" | "base" => "sets the base IRI for relative references",
                "@version" | "version" => "declares the RDF version (RDF 1.2)",
                "graph" => "names a graph block (TriG)",
                _ => return None,
            };
            info(t.start, t.end, format!("**{text}** — {doc}"))
        }
        _ => None,
    }
}

// ------------------------------------------------------------------- SPARQL

/// Signatures for the commonly hovered builtins; everything else keyword-ish
/// gets a generic line.
const SPARQL_SIGS: &[(Kw, &str)] = &[
    (
        Kw::Count,
        "`COUNT(*| expr)` → `xsd:integer` — aggregate: number of solutions",
    ),
    (Kw::Sum, "`SUM(expr)` — aggregate: numeric sum"),
    (Kw::Avg, "`AVG(expr)` — aggregate: numeric mean"),
    (
        Kw::Min,
        "`MIN(expr)` — aggregate: minimum by ORDER BY ordering",
    ),
    (
        Kw::Max,
        "`MAX(expr)` — aggregate: maximum by ORDER BY ordering",
    ),
    (
        Kw::Sample,
        "`SAMPLE(expr)` — aggregate: an arbitrary value from the group",
    ),
    (
        Kw::GroupConcat,
        "`GROUP_CONCAT(expr; SEPARATOR=\"…\")` — aggregate: string join",
    ),
    (
        Kw::Regex,
        "`REGEX(text, pattern [, flags])` → `xsd:boolean`",
    ),
    (
        Kw::Str,
        "`STR(term)` → simple literal — lexical form / IRI string",
    ),
    (
        Kw::Lang,
        "`LANG(literal)` → simple literal — language tag or \"\"",
    ),
    (Kw::Datatype, "`DATATYPE(literal)` → IRI"),
    (Kw::Bound, "`BOUND(?var)` → `xsd:boolean`"),
    (
        Kw::Iri,
        "`IRI(expr)` / `URI(expr)` → IRI (resolved against BASE)",
    ),
    (Kw::BNode, "`BNODE([expr])` → blank node"),
    (
        Kw::Coalesce,
        "`COALESCE(expr, …)` — first argument without error",
    ),
    (Kw::If, "`IF(cond, then, else)`"),
    (Kw::Exists, "`EXISTS { pattern }` → `xsd:boolean`"),
    (Kw::Filter, "`FILTER constraint` — restricts solutions"),
    (Kw::Optional, "`OPTIONAL { pattern }` — left join"),
    (
        Kw::Minus,
        "`MINUS { pattern }` — removes compatible solutions",
    ),
    (Kw::Union, "`{ A } UNION { B }` — alternative patterns"),
    (
        Kw::Bind,
        "`BIND(expr AS ?var)` — assigns into a fresh variable",
    ),
    (Kw::Values, "`VALUES (?v …) { (row) … }` — inline data"),
    (
        Kw::Service,
        "`SERVICE [SILENT] iri { pattern }` — federated query",
    ),
];

/// Hover for a SPARQL document at byte offset `at`.
pub fn sparql_hover(src: &str, at: usize) -> Option<HoverInfo> {
    let toks = tokenize_resilient(src);
    let t = toks
        .iter()
        .find(|t| (t.span.start as usize) <= at && at < t.span.end as usize)?;
    let text = &src[t.span.start as usize..t.span.end as usize];
    match t.kind {
        TokenKind::PNameLn | TokenKind::PNameNs => {
            let decls = crate::completion::sparql_decls(src, &toks);
            let colon = text.find(':')?;
            let (prefix, local) = text.split_at(colon + 1);
            info(
                t.span.start,
                t.span.end,
                pname_markdown(&decls, prefix, local)?,
            )
        }
        TokenKind::Var => {
            let name = &text[1..];
            let uses = toks
                .iter()
                .filter(|o| {
                    o.kind == TokenKind::Var
                        && src[o.span.start as usize + 1..o.span.end as usize] == *name
                })
                .count();
            info(
                t.span.start,
                t.span.end,
                format!(
                    "**{text}** — {uses} occurrence{} in this document",
                    if uses == 1 { "" } else { "s" }
                ),
            )
        }
        TokenKind::LangTag(_) => info(
            t.span.start,
            t.span.end,
            format!("**{text}** — BCP 47 language tag"),
        ),
        TokenKind::Keyword(kw) => {
            let md = match SPARQL_SIGS.iter().find(|(k, _)| *k == kw) {
                Some((_, sig)) => (*sig).to_string(),
                None => format!("**{}** — SPARQL keyword", kw.as_str()),
            };
            info(t.span.start, t.span.end, md)
        }
        TokenKind::A => info(
            t.span.start,
            t.span.end,
            "**a** — shorthand for `rdf:type`".to_string(),
        ),
        _ => None,
    }
}

// ------------------------------------------------------------------ JSON-LD

const JSONLD_DOCS: &[(&str, &str)] = &[
    ("@base", "base IRI for resolving relative IRI references"),
    (
        "@container",
        "container mapping for a term (`@list`, `@set`, `@index`, …)",
    ),
    ("@context", "maps terms to IRIs and sets processing options"),
    ("@direction", "base text direction (`\"ltr\"` / `\"rtl\"`)"),
    (
        "@graph",
        "expresses a named graph or the default graph's contents",
    ),
    ("@id", "the node's IRI (or blank node identifier)"),
    ("@language", "default or literal language tag"),
    ("@list", "an ordered collection (RDF list)"),
    ("@nest", "groups properties under a nesting key"),
    ("@reverse", "reverse property (object → subject)"),
    ("@set", "an unordered collection (dropped on expansion)"),
    ("@type", "the node's type IRI, or a literal's datatype"),
    ("@value", "the literal value of a value object"),
    ("@version", "JSON-LD processing mode (`1.1`)"),
    ("@vocab", "default vocabulary IRI for terms"),
];

/// Hover for a JSON-LD document at byte offset `at`: `@`-keyword strings.
pub fn jsonld_hover(src: &str, at: usize) -> Option<HoverInfo> {
    let b = src.as_bytes();
    // Find the string token containing `at`.
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'"' {
            let (end, terminated) = scan_string(b, i);
            if terminated && i <= at && at < end {
                let content = &src[i + 1..end - 1];
                let doc = JSONLD_DOCS
                    .iter()
                    .find(|(k, _)| *k == content)
                    .map(|(_, d)| *d)
                    .or_else(|| {
                        crate::diagnostics::JSONLD_KEYWORDS
                            .contains(&content)
                            .then_some("JSON-LD keyword")
                    })?;
                return info(i as u32, end as u32, format!("**{content}** — {doc}"));
            }
            i = end;
        } else {
            i += 1;
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn turtle_pname_expands_from_decl() {
        let src = "@prefix ex: <http://e/> .\nex:s ex:p ex:o .";
        let at = src.find("ex:p").unwrap() + 3; // inside the local name
        let h = turtle_hover(src, at).expect("hover");
        assert!(h.markdown.contains("<http://e/p>"), "{h:?}");
        // The range covers the whole pname.
        assert_eq!(&src[h.start as usize..h.end as usize], "ex:p");
        // Hovering the prefix part covers the pname too.
        let h2 = turtle_hover(src, src.find("ex:p").unwrap()).expect("hover");
        assert_eq!(h2, h);
    }

    #[test]
    fn turtle_well_known_fallback_is_marked() {
        let src = "ex:s foaf:knows ex:o .";
        let at = src.find("foaf:").unwrap() + 6;
        let h = turtle_hover(src, at).expect("hover");
        assert!(h.markdown.contains("xmlns.com/foaf"), "{h:?}");
        assert!(h.markdown.contains("undeclared"), "{h:?}");
        // Unknown prefix, no declaration: no hover.
        assert!(turtle_hover("zzz:a zzz:b zzz:c .", 1).is_none());
    }

    #[test]
    fn turtle_literals_and_keywords() {
        let src = "@prefix ex: <http://e/> .\nex:s ex:p 2.5, \"x\"@en--ltr, true .";
        let h = turtle_hover(src, src.find("2.5").unwrap()).unwrap();
        assert!(h.markdown.contains("xsd:decimal"));
        let h = turtle_hover(src, src.find("@en").unwrap() + 1).unwrap();
        assert!(h.markdown.contains("language tag"), "{h:?}");
        assert!(h.markdown.contains("ltr"), "{h:?}");
        let h = turtle_hover(src, src.find("true").unwrap()).unwrap();
        assert!(h.markdown.contains("xsd:boolean"));
        let h = turtle_hover(src, 1).unwrap(); // inside @prefix
        assert!(h.markdown.contains("namespace prefix"));
    }

    #[test]
    fn sparql_keyword_signatures_and_vars() {
        let src = "PREFIX ex: <http://e/>\nSELECT ?s WHERE { ?s ex:p ?o . FILTER(REGEX(STR(?o), \"a\")) }";
        let h = sparql_hover(src, src.find("REGEX").unwrap()).unwrap();
        assert!(h.markdown.contains("REGEX(text, pattern"), "{h:?}");
        let h = sparql_hover(src, src.find("SELECT").unwrap()).unwrap();
        assert!(h.markdown.contains("SPARQL keyword"));
        let h = sparql_hover(src, src.find("?s").unwrap() + 1).unwrap();
        assert!(h.markdown.contains("2 occurrences"), "{h:?}");
        let h = sparql_hover(src, src.find("ex:p").unwrap() + 3).unwrap();
        assert!(h.markdown.contains("<http://e/p>"), "{h:?}");
    }

    #[test]
    fn jsonld_keyword_docs() {
        let src = r#"{"@context": {"name": "http://s/n"}, "@id": "x"}"#;
        let h = jsonld_hover(src, src.find("@context").unwrap()).unwrap();
        assert!(h.markdown.contains("maps terms"), "{h:?}");
        // Non-keyword strings: no hover.
        assert!(jsonld_hover(src, src.find("name").unwrap()).is_none());
    }

    #[test]
    fn never_panics_on_odd_offsets() {
        for src in ["", "ex:", "?x", "\u{1F600}", "\"@id\""] {
            for at in 0..=src.len() + 1 {
                let _ = turtle_hover(src, at);
                let _ = sparql_hover(src, at);
                let _ = jsonld_hover(src, at);
            }
        }
    }
}
