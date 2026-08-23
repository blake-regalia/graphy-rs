//! Interchange codecs for graphy: RDF/XML and JSON-LD, parse and serialize. Terms use
//! graphy's concise encoding (doc 01 §3: `>iri`, `_label`, `"lexical`,
//! `@lang"lexical`, `^>datatype"lexical`).
//!
//! Subset boundaries (documented rather than silent): the XML reader rejects
//! external entity declarations; bounded internal general entities are supported,
//! and `parseType="Literal"` produces canonical XML
//! lexical forms. The JSON-LD parser rejects remote contexts, `@reverse` and
//! non-`@list`/`@set` containers.

mod jsonld;
mod rdfxml;
mod xml;

pub use jsonld::{parse_jsonld, parse_jsonld_with_options, write_jsonld};
pub use rdfxml::{parse_rdfxml, parse_rdfxml_with_options, write_rdfxml};

/// Interop parser configuration.
#[derive(Debug, Clone, Copy, Default)]
pub struct ParseOptions {
    /// Document-scoped blank-label namespace. When absent, the parser
    /// generates a random 128-bit namespace. Supplying one is useful for
    /// reproducible builds and tests; separate documents being combined
    /// into one dataset must use distinct values.
    pub label_ns: Option<u128>,
}

fn document_label_ns(options: ParseOptions) -> u128 {
    options.label_ns.unwrap_or_else(|| {
        let mut bytes = [0; 16];
        getrandom::fill(&mut bytes)
            .expect("secure randomness is required for blank-node freshness");
        u128::from_le_bytes(bytes)
    })
}

/// One concise-encoded triple.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Triple {
    pub s: Vec<u8>,
    pub p: Vec<u8>,
    pub o: Vec<u8>,
}

/// A codec parse failure.
pub struct ParseError(pub String);

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}
impl std::fmt::Debug for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "ParseError({})", self.0)
    }
}
impl std::error::Error for ParseError {}

#[cfg(test)]
mod tests {
    use super::*;

    const TEST_NS: u128 = 7;

    fn test_options() -> ParseOptions {
        ParseOptions {
            label_ns: Some(TEST_NS),
        }
    }

    fn scoped_surface(label: &str) -> Vec<u8> {
        format!("_i{TEST_NS:032x}s{label}").into_bytes()
    }

    fn canonical_blanks(mut triples: Vec<Triple>) -> Vec<Triple> {
        let mut labels = std::collections::HashMap::<Vec<u8>, Vec<u8>>::new();
        let mut next = 0usize;
        for triple in &mut triples {
            for term in [&mut triple.s, &mut triple.o] {
                if !term.starts_with(b"_") {
                    continue;
                }
                let normalized = labels
                    .entry(term.clone())
                    .or_insert_with(|| {
                        let label = format!("_b{next}").into_bytes();
                        next += 1;
                        label
                    })
                    .clone();
                *term = normalized;
            }
        }
        triples.sort_by(|a, b| (&a.s, &a.p, &a.o).cmp(&(&b.s, &b.p, &b.o)));
        triples
    }

    fn iri(value: &str) -> Vec<u8> {
        format!(">{value}").into_bytes()
    }

    #[test]
    fn rdfxml_basic_document() {
        let triples = parse_rdfxml(
            r#"<?xml version="1.0"?>
<rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#"
         xmlns:dct="http://purl.org/dc/terms/">
    <rdf:Description rdf:about="">
        <dct:title xml:lang="en">An Org</dct:title>
    </rdf:Description>
</rdf:RDF>"#,
            Some("http://x/orgs/o"),
        )
        .unwrap();
        assert_eq!(triples.len(), 1);
        assert_eq!(triples[0].s, iri("http://x/orgs/o"));
        assert_eq!(triples[0].p, iri("http://purl.org/dc/terms/title"));
        assert_eq!(triples[0].o, "@en\"An Org".as_bytes().to_vec());
    }

    #[test]
    fn rdfxml_typed_nodes_and_nesting() {
        let triples = parse_rdfxml_with_options(
            r#"<rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#"
         xmlns:ex="http://x/ns#">
    <ex:Widget rdf:about="http://x/w1" ex:name="gadget">
        <ex:part rdf:resource="http://x/p1"/>
        <ex:count rdf:datatype="http://www.w3.org/2001/XMLSchema#integer">5</ex:count>
        <ex:maker>
            <ex:Person rdf:nodeID="alice"/>
        </ex:maker>
    </ex:Widget>
</rdf:RDF>"#,
            None,
            test_options(),
        )
        .unwrap();
        let has = |s: &[u8], p: &str, o: &[u8]| {
            triples
                .iter()
                .any(|t| t.s == s && t.p == iri(p) && t.o == o)
        };
        let w1 = iri("http://x/w1");
        assert!(has(
            &w1,
            "http://www.w3.org/1999/02/22-rdf-syntax-ns#type",
            &iri("http://x/ns#Widget")
        ));
        assert!(has(&w1, "http://x/ns#name", b"\"gadget"));
        assert!(has(&w1, "http://x/ns#part", &iri("http://x/p1")));
        assert!(has(
            &w1,
            "http://x/ns#count",
            "^>http://www.w3.org/2001/XMLSchema#integer\"5".as_bytes()
        ));
        let alice = scoped_surface("alice");
        assert!(has(&w1, "http://x/ns#maker", &alice));
        assert!(has(
            &alice,
            "http://www.w3.org/1999/02/22-rdf-syntax-ns#type",
            &iri("http://x/ns#Person")
        ));
    }

    #[test]
    fn rdfxml_round_trip() {
        let source = parse_rdfxml(
            r#"<rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#"
         xmlns:ex="http://x/ns#">
    <rdf:Description rdf:about="http://x/a">
        <ex:knows rdf:resource="http://x/b"/>
        <ex:label xml:lang="en-US">Hi &amp; bye</ex:label>
    </rdf:Description>
</rdf:RDF>"#,
            None,
        )
        .unwrap();
        let serialized = write_rdfxml(&source);
        let reparsed = parse_rdfxml(&serialized, None).unwrap();
        assert_eq!(canonical_blanks(source), canonical_blanks(reparsed));
    }

    #[test]
    fn jsonld_layer1_body_shape() {
        // the shape the layer1 suite posts
        let triples = parse_jsonld(
            r#"{
                "@id": "",
                "http://purl.org/dc/terms/title": {"@value": "An Org", "@language": "en"}
            }"#,
            Some("http://x/orgs/o"),
        )
        .unwrap();
        assert_eq!(triples.len(), 1);
        assert_eq!(triples[0].s, iri("http://x/orgs/o"));
        assert_eq!(triples[0].o, "@en\"An Org".as_bytes().to_vec());
    }

    #[test]
    fn jsonld_context_and_natives() {
        let triples = parse_jsonld(
            r#"{
                "@context": {
                    "ex": "http://x/ns#",
                    "name": "ex:name",
                    "knows": {"@id": "ex:knows", "@type": "@id"},
                    "tags": {"@id": "ex:tag", "@container": "@list"}
                },
                "@id": "http://x/a",
                "@type": "ex:Person",
                "name": "Alice",
                "ex:age": 30,
                "ex:active": true,
                "knows": "http://x/b",
                "tags": ["one", "two"]
            }"#,
            None,
        )
        .unwrap();
        let has = |p: &str, o: &[u8]| {
            triples
                .iter()
                .any(|t| t.s == iri("http://x/a") && t.p == iri(p) && t.o == o)
        };
        assert!(has(
            "http://www.w3.org/1999/02/22-rdf-syntax-ns#type",
            &iri("http://x/ns#Person")
        ));
        assert!(has("http://x/ns#name", b"\"Alice"));
        assert!(has(
            "http://x/ns#age",
            "^>http://www.w3.org/2001/XMLSchema#integer\"30".as_bytes()
        ));
        assert!(has(
            "http://x/ns#active",
            "^>http://www.w3.org/2001/XMLSchema#boolean\"true".as_bytes()
        ));
        assert!(has("http://x/ns#knows", &iri("http://x/b")));
        // list: head cell carries first "one"
        let first = iri("http://www.w3.org/1999/02/22-rdf-syntax-ns#first");
        assert!(triples.iter().any(|t| t.p == first && t.o == b"\"one"));
    }

    #[test]
    fn jsonld_round_trip() {
        let source = parse_jsonld(
            r#"{
                "@id": "http://x/a",
                "@type": "http://x/ns#Person",
                "http://x/ns#name": {"@value": "Grüße", "@language": "de"},
                "http://x/ns#knows": {"@id": "http://x/b"}
            }"#,
            None,
        )
        .unwrap();
        let serialized = write_jsonld(&source);
        let reparsed = parse_jsonld(&serialized, None).unwrap();
        assert_eq!(canonical_blanks(source), canonical_blanks(reparsed));
    }

    /// Surface blank labels must stay disjoint from every other minting
    /// domain, including the update executor's process-fresh `_u…` labels.
    #[test]
    fn surface_blank_labels_disjoint_from_minted_domains() {
        let engine_minted = |label: &[u8]| label.starts_with(b"u");
        let triples = parse_jsonld_with_options(
            r#"[
                {"@id": "_:0", "http://x/ns#knows": {"@id": "_:1"}},
                {"@id": "http://x/a", "http://x/ns#tags": {"@list": ["one"]}}
            ]"#,
            None,
            test_options(),
        )
        .unwrap();
        let mut saw_surface = false;
        for term in triples.iter().flat_map(|t| [&t.s, &t.o]) {
            let Some(label) = term.strip_prefix(b"_") else {
                continue;
            };
            assert!(
                !engine_minted(label),
                "surface label {:?} collides with the engine's `_u…` domain",
                String::from_utf8_lossy(term)
            );
            saw_surface |= label.starts_with(b"i");
        }
        assert!(saw_surface, "expected `i{{label}}` surface blanks");
        // `_:0` and `_:1` keep their document co-reference under the new prefix
        assert!(triples
            .iter()
            .any(|t| t.s == scoped_surface("0") && t.o == scoped_surface("1")));

        // RDF/XML nodeID labels land in the same disjoint domain
        let triples = parse_rdfxml_with_options(
            r#"<rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#"
         xmlns:ex="http://x/ns#">
    <rdf:Description rdf:nodeID="u7">
        <ex:p rdf:resource="http://x/b"/>
    </rdf:Description>
</rdf:RDF>"#,
            None,
            test_options(),
        )
        .unwrap();
        assert_eq!(triples[0].s, scoped_surface("u7"));
        assert!(!engine_minted(&triples[0].s[1..]));
    }

    #[test]
    fn interop_blank_labels_are_document_scoped() {
        let json = r#"{"@id":"_:same","http://x/p":{"@id":"_:other"}}"#;
        let first = parse_jsonld(json, None).unwrap();
        let second = parse_jsonld(json, None).unwrap();
        assert_ne!(first[0].s, second[0].s);
        assert_ne!(first[0].o, second[0].o);

        let xml = r#"<rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#"
                     xmlns:ex="http://x/">
            <rdf:Description rdf:nodeID="same"><ex:p rdf:nodeID="other"/></rdf:Description>
        </rdf:RDF>"#;
        let first = parse_rdfxml(xml, None).unwrap();
        let second = parse_rdfxml(xml, None).unwrap();
        assert_ne!(first[0].s, second[0].s);
        assert_ne!(first[0].o, second[0].o);
    }

    #[test]
    fn rejections() {
        assert!(parse_rdfxml(
            "<rdf:RDF xmlns:rdf='http://www.w3.org/1999/02/22-rdf-syntax-ns#'><rdf:li/></rdf:RDF>",
            None
        )
        .is_err());
        assert!(parse_rdfxml("not xml", None).is_err());
        assert!(parse_jsonld(r#"{"@context": "http://remote/ctx"}"#, None).is_err());
        assert!(parse_jsonld("[1,2", None).is_err());
    }
}
