//! `tokenize_resilient` contract (docs/10 §3.2–3.3): never fails, always makes
//! progress, spans stay ordered and in bounds, and it agrees with the strict
//! `tokenize` on well-formed input.

use graphy_sparql_syntax::{tokenize, tokenize_resilient, Token, TokenKind};

/// Ordered, non-overlapping, in-bounds spans (gaps are trivia).
fn check_spans(src: &str, toks: &[Token]) {
    let mut prev_end = 0u32;
    for t in toks {
        assert!(
            t.span.start < t.span.end,
            "empty/inverted {:?} in {src:?}",
            t.span
        );
        assert!(t.span.end as usize <= src.len(), "past end {:?}", t.span);
        assert!(
            t.span.start >= prev_end,
            "overlap/disorder {:?} in {src:?}",
            t.span
        );
        prev_end = t.span.end;
    }
}

const VALID: &[&str] = &[
    "SELECT * WHERE { ?s ?p ?o }",
    "PREFIX ex: <http://e/>\nASK { ex:s ex:p ?o . FILTER(?o > 1 && ?o < 10) }",
    "SELECT (COUNT(?x) AS ?n) WHERE { ?x a ex:T } GROUP BY ?x",
    "INSERT DATA { <http://a/s> <http://a/p> \"lit\"@en }",
    "SELECT * { <<( ?s ?p ?o )>> ~ ?r { ?a ?b ?c } }",
];

#[test]
fn agrees_with_strict_on_valid_input() {
    for q in VALID {
        let strict = tokenize(q).expect("valid query lexes");
        let resilient = tokenize_resilient(q);
        assert_eq!(strict, resilient, "divergence on:\n{q}");
        assert!(
            resilient.iter().all(|t| t.kind != TokenKind::Error),
            "unexpected Error token in:\n{q}"
        );
        check_spans(q, &resilient);
    }
}

#[test]
fn recovers_past_bad_bytes() {
    // A backtick is not a SPARQL byte; scanning must continue past it.
    let src = "SELECT ?s ` WHERE { ?s ?p ?o }";
    let toks = tokenize_resilient(src);
    check_spans(src, &toks);
    assert!(toks.iter().any(|t| t.kind == TokenKind::Error));
    assert!(toks.iter().filter(|t| t.kind == TokenKind::Var).count() == 4);
}

#[test]
fn unterminated_iri_does_not_hang() {
    let src = "SELECT * WHERE { ?s <http://unterminated";
    let toks = tokenize_resilient(src);
    check_spans(src, &toks);
}

use proptest::prelude::*;

proptest! {
    #![proptest_config(ProptestConfig::with_cases(1024))]

    /// Any string terminates without panic and yields ordered, in-bounds spans.
    #[test]
    fn arbitrary_strings(src in proptest::collection::vec(any::<char>(), 0..256)) {
        let src: String = src.into_iter().collect();
        check_spans(&src, &tokenize_resilient(&src));
    }

    /// SPARQL-biased bytes to hit the real lexer paths.
    #[test]
    fn sparqlish(src in proptest::collection::vec(
        prop_oneof![
            Just('<'), Just('>'), Just('"'), Just('\''), Just(':'), Just('?'),
            Just('$'), Just('_'), Just('@'), Just('^'), Just('.'), Just(';'),
            Just(','), Just('('), Just(')'), Just('{'), Just('}'), Just('['),
            Just(']'), Just('|'), Just('&'), Just('!'), Just('='), Just('~'),
            Just(' '), Just('\n'), Just('a'), Just('1'), Just('#'), any::<char>(),
        ],
        0..200,
    )) {
        let src: String = src.into_iter().collect();
        check_spans(&src, &tokenize_resilient(&src));
    }
}
