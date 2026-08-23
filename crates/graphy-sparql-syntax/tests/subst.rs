//! §M13d substitution: injection-safe parameterized queries — values are
//! terms placed into the tree, printed like any other term; declared
//! variables and position-invalid values are rejected.

use graphy_sparql_syntax::ast::{Term, TermKind};
use graphy_sparql_syntax::token::Span;
use graphy_sparql_syntax::{parse_query, print_query, substitute_query, SubstError};

fn iri(v: &str) -> Term {
    Term {
        kind: TermKind::Iri(v.to_owned()),
        span: Span { start: 0, end: 0 },
    }
}

fn lit(v: &str) -> Term {
    Term {
        kind: TermKind::Literal {
            lexical: v.to_owned(),
            kind: graphy_sparql_syntax::ast::LiteralKind::Plain,
        },
        span: Span { start: 0, end: 0 },
    }
}

#[test]
fn substitutes_terms_everywhere() {
    let q = parse_query(
        "PREFIX ex: <http://ex/> SELECT ?o WHERE { ?s ex:p ?o . GRAPH ?g { ?s ?p2 ?o } FILTER(?s != ex:no) }",
    )
    .unwrap();
    let out = substitute_query(
        &q,
        &[
            ("s".into(), iri("http://ex/subject")),
            ("g".into(), iri("http://g/")),
            ("p2".into(), iri("http://ex/q")),
        ],
    )
    .unwrap();
    let text = print_query(&out);
    assert!(text.contains("ex:subject ex:p ?o"), "{text}");
    assert!(text.contains("GRAPH <http://g/>"), "{text}");
    assert!(text.contains("ex:subject ex:q ?o"), "{text}");
    assert!(text.contains("FILTER(ex:subject != ex:no)"), "{text}");
    // The result still parses (structurally valid output).
    parse_query(&text).unwrap();
}

#[test]
fn injection_is_inert() {
    let q = parse_query("SELECT * WHERE { ?s <http://ex/p> ?o }").unwrap();
    // A hostile "value" stays a quoted literal — never new syntax.
    let out = substitute_query(&q, &[("o".into(), lit("\" } DELETE WHERE { ?a ?b ?c"))]).unwrap();
    let text = print_query(&out);
    let re = parse_query(&text).unwrap();
    // Still exactly one group element; the payload is literal content.
    assert_eq!(re.pattern.elements.len(), 1);
    assert!(text.contains("\\\" } DELETE WHERE"), "{text}");
}

#[test]
fn rejections() {
    let q = parse_query("SELECT ?x WHERE { ?x ?p ?y . BIND(1 AS ?b) VALUES ?v { 1 } }").unwrap();
    // Projected, BIND-declared, VALUES-declared variables refuse binding.
    for var in ["x", "b", "v"] {
        assert!(matches!(
            substitute_query(&q, &[(var.into(), iri("http://x/"))]),
            Err(SubstError::DeclaredVar(_))
        ));
    }
    // Predicate position demands an IRI.
    assert!(matches!(
        substitute_query(&q, &[("p".into(), lit("nope"))]),
        Err(SubstError::InvalidPredicate(_))
    ));
    // Blank-node values are refused outright.
    let blank = Term {
        kind: TermKind::BlankNode("b0".into()),
        span: Span { start: 0, end: 0 },
    };
    assert!(matches!(
        substitute_query(&q, &[("y".into(), blank)]),
        Err(SubstError::BlankValue(_))
    ));
    // Graph position demands an IRI.
    let g = parse_query("SELECT * WHERE { GRAPH ?g { ?s ?p ?o } }").unwrap();
    assert!(matches!(
        substitute_query(&g, &[("g".into(), lit("nope"))]),
        Err(SubstError::InvalidGraphName(_))
    ));
}

#[test]
fn substitution_matches_values_semantics() {
    // Substituting ?s ≡ the same query with VALUES ?s { <iri> } modulo
    // binding visibility — here just check the printed pattern matches a
    // hand-written constant query.
    let q = parse_query("PREFIX ex: <http://ex/> SELECT ?o WHERE { ?s ex:p ?o }").unwrap();
    let out = substitute_query(&q, &[("s".into(), iri("http://ex/k"))]).unwrap();
    let direct = parse_query("PREFIX ex: <http://ex/> SELECT ?o WHERE { ex:k ex:p ?o }").unwrap();
    assert_eq!(print_query(&out), print_query(&direct));
}
