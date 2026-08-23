//! Resilient-highlighter tests (docs/10 §3.3, §7): the partition invariant on
//! arbitrary input, classification on well-formed input, and the recovery /
//! provisional-token behaviour on partial input.

use graphy_turtle::{highlight_tokens, HlKind, HlToken};

/// Assert the structural invariant every tokenization must satisfy: spans are
/// in bounds, non-empty, strictly ordered, and never overlap.
fn check_partition(src: &[u8], toks: &[HlToken]) {
    let mut prev_end = 0u32;
    for t in toks {
        assert!(t.start < t.end, "empty/inverted span {t:?} in {src:?}");
        assert!(
            t.end as usize <= src.len(),
            "span past end {t:?} len {}",
            src.len()
        );
        assert!(
            t.start >= prev_end,
            "overlap/disorder: {t:?} after end {prev_end} in {src:?}"
        );
        prev_end = t.end;
    }
}

fn kinds(src: &str) -> Vec<(HlKind, &str)> {
    highlight_tokens(src.as_bytes())
        .into_iter()
        .map(|t| (t.kind, &src[t.start as usize..t.end as usize]))
        .collect()
}

#[test]
fn classifies_a_turtle_statement() {
    let got = kinds("@prefix ex: <http://e/> .\nex:s ex:p \"lit\"@en .");
    assert_eq!(
        got,
        vec![
            (HlKind::Keyword, "@prefix"),
            (HlKind::PrefixName, "ex:"),
            (HlKind::Iri, "<http://e/>"),
            (HlKind::Punct, "."),
            (HlKind::PrefixName, "ex:"),
            (HlKind::LocalName, "s"),
            (HlKind::PrefixName, "ex:"),
            (HlKind::LocalName, "p"),
            (HlKind::String, "\"lit\""),
            (HlKind::LangTag, "@en"),
            (HlKind::Punct, "."),
        ]
    );
}

#[test]
fn empty_prefix_and_typed_literal() {
    let got = kinds(":s a 1 , 2.5 ; :p \"x\"^^<http://t/> .");
    assert_eq!(
        got,
        vec![
            (HlKind::PrefixName, ":"),
            (HlKind::LocalName, "s"),
            (HlKind::Keyword, "a"),
            (HlKind::Number, "1"),
            (HlKind::Punct, ","),
            (HlKind::Number, "2.5"),
            (HlKind::Punct, ";"),
            (HlKind::PrefixName, ":"),
            (HlKind::LocalName, "p"),
            (HlKind::String, "\"x\""),
            (HlKind::Operator, "^^"),
            (HlKind::Iri, "<http://t/>"),
            (HlKind::Punct, "."),
        ]
    );
}

#[test]
fn comments_and_whitespace_are_gaps_not_tokens() {
    let toks = kinds("ex:s # trailing comment\n  ex:p ex:o .");
    // Every emitted token is meaningful; the comment/newline is a gap.
    assert!(toks.iter().all(|(k, _)| *k != HlKind::Error));
    assert_eq!(toks.first(), Some(&(HlKind::PrefixName, "ex:")));
}

#[test]
fn rdf12_operators() {
    let toks = kinds("<<( :s :p :o )>> ~ _:r {| :a :b |} .");
    let ops: Vec<&str> = toks
        .iter()
        .filter(|(k, _)| *k == HlKind::Operator)
        .map(|(_, s)| *s)
        .collect();
    assert_eq!(ops, vec!["<<(", ")>>", "~", "{|", "|}"]);
    assert!(toks.contains(&(HlKind::BlankNode, "_:r")));
}

#[test]
fn provisional_iri_while_typing() {
    // Unterminated IRI at EOF stays coloured as an IRI, not an error.
    let toks = highlight_tokens(b"ex:s ex:p <http://example.org/incomplet");
    let last = toks.last().unwrap();
    assert_eq!(last.kind, HlKind::Iri);
    assert_eq!(
        last.end as usize,
        "ex:s ex:p <http://example.org/incomplet".len()
    );
}

#[test]
fn provisional_string_while_typing() {
    let toks = highlight_tokens(b"ex:s ex:p \"half a stri");
    assert_eq!(toks.last().unwrap().kind, HlKind::String);
}

#[test]
fn garbage_recovers_and_keeps_scanning() {
    // A bad byte between two good statements must not swallow the rest.
    let src = b"ex:a ex:b ex:c .\n\x07\x07\x07\nex:d ex:e ex:f .";
    let toks = highlight_tokens(src);
    check_partition(src, &toks);
    assert!(toks.iter().any(|t| t.kind == HlKind::Error));
    // The statement after the garbage is still classified.
    let names = toks.iter().filter(|t| t.kind == HlKind::LocalName).count();
    assert_eq!(names, 6);
}

#[test]
fn valid_docs_have_no_error_tokens() {
    for doc in VALID_DOCS {
        let toks = highlight_tokens(doc.as_bytes());
        check_partition(doc.as_bytes(), &toks);
        assert!(
            toks.iter().all(|t| t.kind != HlKind::Error),
            "unexpected Error token in valid doc:\n{doc}"
        );
    }
}

const VALID_DOCS: &[&str] = &[
    "<http://a/s> <http://a/p> <http://a/o> .",
    "<http://a/s> <http://a/p> \"o\" <http://a/g> .",
    "@prefix : <http://e/> .\n:s :p :o , :o2 ; :q [ :r :s ] .",
    "PREFIX ex: <http://e/>\nex:s ex:p 1, 2.0, 3.0e0, true, false .",
    "@prefix ex: <http://e/> .\nex:s ex:p \"\"\"long\nstring\"\"\" .",
    ":s :p \"chat\"@fr-be , \"dir\"@en--ltr .",
    "{ <http://a/s> <http://a/p> <http://a/o> }",
];

// ---- proptest: the arbitrary-input contract (docs/10 §3.3) ----

use proptest::prelude::*;

proptest! {
    #![proptest_config(ProptestConfig::with_cases(1024))]

    /// For ANY byte string the tokenizer terminates without panic and yields a
    /// well-formed, non-overlapping, in-bounds span sequence.
    #[test]
    fn arbitrary_bytes_partition(src in proptest::collection::vec(any::<u8>(), 0..512)) {
        let toks = highlight_tokens(&src);
        check_partition(&src, &toks);
    }

    /// Biased toward RDF-ish bytes to exercise the real lexer paths (angle
    /// brackets, quotes, colons, escapes) rather than mostly-garbage.
    #[test]
    fn rdfish_bytes_partition(
        src in proptest::collection::vec(
            prop_oneof![
                Just(b'<'), Just(b'>'), Just(b'"'), Just(b'\''), Just(b':'),
                Just(b'_'), Just(b'@'), Just(b'^'), Just(b'.'), Just(b';'),
                Just(b'['), Just(b']'), Just(b'{'), Just(b'}'), Just(b'('),
                Just(b')'), Just(b'|'), Just(b'~'), Just(b'\\'), Just(b' '),
                Just(b'\n'), Just(b'a'), Just(b'1'), Just(b'#'),
                any::<u8>(),
            ],
            0..256,
        )
    ) {
        let toks = highlight_tokens(&src);
        check_partition(&src, &toks);
    }
}
