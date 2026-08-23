//! Recovering-parser contract (docs/10 §3.2): on valid input it agrees with
//! the strict parse; on broken input it reports *several* localized errors
//! (group-anchor and `;`-boundary resync) and still returns a usable tree;
//! and it never fails, panics, or loops on arbitrary input.

use graphy_sparql_syntax::ast::{GroupElement, UpdateOp};
use graphy_sparql_syntax::{
    parse_query, parse_query_recovering, parse_update, parse_update_recovering,
};

const VALID_QUERIES: &[&str] = &[
    "SELECT * WHERE { ?s ?p ?o }",
    "PREFIX ex: <http://e/>\nASK { ex:s ex:p ?o . FILTER(?o > 1 && ?o < 10) }",
    "SELECT (COUNT(?x) AS ?n) WHERE { ?x a <http://T> } GROUP BY ?x",
    "SELECT * { { ?a ?b ?c } UNION { ?d ?e ?f } OPTIONAL { ?g ?h ?i } }",
    "CONSTRUCT { ?s ?p ?o } WHERE { GRAPH ?g { ?s ?p ?o } } LIMIT 5",
];

const VALID_UPDATES: &[&str] = &[
    "INSERT DATA { <http://a/s> <http://a/p> \"lit\"@en }",
    "PREFIX ex: <http://e/>\nDELETE WHERE { ?s ex:p ?o } ;\nCLEAR GRAPH <http://g>",
    "WITH <http://g> DELETE { ?s ?p ?o } INSERT { ?s ?p 1 } WHERE { ?s ?p ?o }",
];

#[test]
fn agrees_with_strict_on_valid_input() {
    for q in VALID_QUERIES {
        let strict = parse_query(q).expect("valid query");
        let (tree, errors) = parse_query_recovering(q);
        assert!(errors.is_empty(), "spurious errors on {q}: {errors:?}");
        assert_eq!(format!("{strict:?}"), format!("{:?}", tree.unwrap()), "{q}");
    }
    for u in VALID_UPDATES {
        let strict = parse_update(u).expect("valid update");
        let (tree, errors) = parse_update_recovering(u);
        assert!(errors.is_empty(), "spurious errors on {u}: {errors:?}");
        assert_eq!(format!("{strict:?}"), format!("{:?}", tree.unwrap()), "{u}");
    }
}

#[test]
fn multiple_broken_group_elements_all_report() {
    // Two broken elements (empty FILTER, BIND missing its expression)
    // interleaved with three valid triple runs.
    let src = "SELECT * WHERE {\n\
               ?s ?p ?o .\n\
               FILTER() .\n\
               ?a ?b ?c .\n\
               BIND(AS ?x) .\n\
               ?d ?e ?f\n\
               }";
    let (tree, errors) = parse_query_recovering(src);
    assert_eq!(errors.len(), 2, "{errors:?}");
    // Errors are localized: first inside FILTER(), second inside BIND().
    let filter_at = src.find("FILTER").unwrap() as u32;
    let bind_at = src.find("BIND").unwrap() as u32;
    assert!(errors[0].span.start >= filter_at && errors[0].span.start < bind_at);
    assert!(errors[1].span.start >= bind_at);
    // The tree keeps the valid triple runs.
    let q = tree.expect("recovered tree");
    let triples = q
        .pattern
        .elements
        .iter()
        .filter(|e| matches!(e, GroupElement::Triples(_)))
        .count();
    assert_eq!(triples, 3, "recovered tree should keep all valid runs");
}

#[test]
fn lex_garbage_is_reported_and_skipped() {
    let src = "SELECT * WHERE { ?s ?p ` ?o }";
    let (tree, errors) = parse_query_recovering(src);
    assert_eq!(errors.len(), 1, "{errors:?}");
    assert_eq!(errors[0].message, "unrecognized input");
    assert_eq!(errors[0].span.start, src.find('`').unwrap() as u32);
    // With the garbage dropped the triple parses.
    assert!(tree.is_some());
}

#[test]
fn broken_update_op_does_not_hide_later_ops() {
    let src = "INSERT DATA { <http://a/s> <http://a/p> } ;\n\
               CLEAR GRAPH <http://g> ;\n\
               DROP GRAPH <http://h>";
    let (tree, errors) = parse_update_recovering(src);
    assert!(!errors.is_empty(), "the truncated triple must error");
    let u = tree.expect("recovered request");
    assert!(
        u.operations
            .iter()
            .any(|op| matches!(op, UpdateOp::Clear { .. })),
        "CLEAR after the broken op must survive: {:?}",
        u.operations
    );
    assert!(u
        .operations
        .iter()
        .any(|op| matches!(op, UpdateOp::Drop { .. })));
}

#[test]
fn strict_failure_implies_recovering_errors() {
    for src in [
        "SELECT ?s WHERE { ?s ?p }",
        "SELECT * WHERE { FILTER( }",
        "INSERT DATA { <a> }",
        "ASK { BIND(1 AS ?x) BIND(2 AS ?x) }",
    ] {
        let strict_q = parse_query(src).is_err();
        let strict_u = parse_update(src).is_err();
        if strict_q && strict_u {
            let (_, eq) = parse_query_recovering(src);
            let (_, eu) = parse_update_recovering(src);
            assert!(
                !eq.is_empty() && !eu.is_empty(),
                "strict fails but recovering silent on {src}"
            );
        }
    }
}

use proptest::prelude::*;

proptest! {
    #![proptest_config(ProptestConfig::with_cases(512))]

    /// Recovering parses terminate without panic on arbitrary input, and
    /// every reported span is in bounds.
    #[test]
    fn arbitrary_input_never_panics(src in proptest::collection::vec(
        prop_oneof![
            Just("SELECT"), Just("WHERE"), Just("FILTER"), Just("BIND"),
            Just("INSERT"), Just("DELETE"), Just("DATA"), Just("{"), Just("}"),
            Just("("), Just(")"), Just("."), Just(";"), Just("?x"), Just("?y"),
            Just("<http://e/>"), Just("ex:p"), Just("\"lit\""), Just("1"),
            Just("+"), Just("a"), Just("`"), Just("UNION"), Just("OPTIONAL"),
        ],
        0..40,
    )) {
        let src = src.join(" ");
        for errors in [parse_query_recovering(&src).1, parse_update_recovering(&src).1] {
            for e in errors {
                prop_assert!(e.span.start <= e.span.end);
                prop_assert!(e.span.end as usize <= src.len());
            }
        }
    }
}
