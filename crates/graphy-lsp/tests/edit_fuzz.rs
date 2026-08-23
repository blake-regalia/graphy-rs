//! Incremental-sync fuzz (docs/10 §14 M11a exit criterion): random edit
//! sequences applied through `Doc::apply` (rope + UTF-16 positions) must agree
//! with a naive `String` model at every step. The model does its own
//! char-walking position math, so a bug in the rope path can't hide.

use graphy_lsp::document::{Doc, Lang};
use lsp_types::{Position, Range, TextDocumentContentChangeEvent};
use proptest::prelude::*;

/// LSP `(line, UTF-16 character)` of char index `at` in `model`.
fn position_of(model: &str, at: usize) -> Position {
    let (mut line, mut col) = (0u32, 0u32);
    for c in model.chars().take(at) {
        if c == '\n' {
            line += 1;
            col = 0;
        } else {
            col += c.len_utf16() as u32;
        }
    }
    Position::new(line, col)
}

/// Replace the char range `[a, b)` of `model` with `ins`.
fn model_splice(model: &str, a: usize, b: usize, ins: &str) -> String {
    let byte_of = |n: usize| {
        model
            .char_indices()
            .nth(n)
            .map(|(i, _)| i)
            .unwrap_or(model.len())
    };
    let mut out = String::with_capacity(model.len() + ins.len());
    out.push_str(&model[..byte_of(a)]);
    out.push_str(ins);
    out.push_str(&model[byte_of(b)..]);
    out
}

/// ASCII, multi-byte BMP, astral, and newline — every UTF-16 width class.
fn doc_char() -> impl Strategy<Value = char> {
    prop_oneof![
        Just('a'),
        Just('b'),
        Just(' '),
        Just('é'),
        Just('😀'),
        Just('\n'),
    ]
}

fn doc_string(max: usize) -> impl Strategy<Value = String> {
    proptest::collection::vec(doc_char(), 0..max).prop_map(|cs| cs.into_iter().collect())
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(512))]

    /// Ranged edits: rope and model agree after every step.
    #[test]
    fn ranged_edits_match_naive_model(
        initial in doc_string(40),
        edits in proptest::collection::vec(
            (any::<prop::sample::Index>(), any::<prop::sample::Index>(), doc_string(8)),
            1..8,
        ),
    ) {
        let mut doc = Doc::new(&initial, 1, Lang::Turtle);
        let mut model = initial;
        for (ia, ib, ins) in edits {
            let n = model.chars().count();
            let (mut a, mut b) = (ia.index(n + 1), ib.index(n + 1));
            if a > b {
                std::mem::swap(&mut a, &mut b);
            }
            let range = Range::new(position_of(&model, a), position_of(&model, b));
            doc.apply(TextDocumentContentChangeEvent {
                range: Some(range),
                range_length: None,
                text: ins.clone(),
            });
            model = model_splice(&model, a, b, &ins);
            prop_assert_eq!(doc.text(), model.clone());
        }
    }

    /// A full-document replace (no range) always matches exactly.
    #[test]
    fn full_replace_matches(initial in doc_string(40), replacement in doc_string(40)) {
        let mut doc = Doc::new(&initial, 1, Lang::Turtle);
        doc.apply(TextDocumentContentChangeEvent {
            range: None,
            range_length: None,
            text: replacement.clone(),
        });
        prop_assert_eq!(doc.text(), replacement);
    }
}
