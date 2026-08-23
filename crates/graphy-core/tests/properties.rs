//! Property tests for graphy-core (plan M0 exit criteria): concise round-trip
//! against structured term specs, inline-ID semantics against naive
//! string/bigint models (especially canonical vs non-canonical numerics),
//! order preservation, and no-panic fuzzing of the decoders.

use std::cmp::Ordering;

use graphy_core::id::InlineDateTime;
use graphy_core::{concise, vocab, Dir, InlineValue, Term, TermId, TermRef};
use proptest::prelude::*;

// ------------------------------------------------------- term round-trips

/// A structured description of a term, independent of the encoding.
#[derive(Debug, Clone)]
enum Spec {
    Iri(String),
    Blank(String),
    Simple(String),
    Lang(String, String, Option<Dir>),
    Typed(String, String),
    Triple(Box<Spec>, Box<Spec>, Box<Spec>),
}

fn arb_iri() -> impl Strategy<Value = String> {
    "[A-Za-z0-9._~-]{0,16}".prop_map(|s| format!("http://ex.example/{s}"))
}

fn arb_subject() -> impl Strategy<Value = Spec> {
    prop_oneof![
        arb_iri().prop_map(Spec::Iri),
        "[A-Za-z0-9]{1,10}".prop_map(Spec::Blank),
    ]
}

fn arb_spec() -> impl Strategy<Value = Spec> {
    let leaf = prop_oneof![
        arb_iri().prop_map(Spec::Iri),
        "[A-Za-z0-9]{1,10}".prop_map(Spec::Blank),
        any::<String>().prop_map(Spec::Simple),
        (
            any::<String>(),
            "[a-z]{1,8}(-[a-z0-9]{1,8}){0,2}",
            proptest::option::of(prop_oneof![Just(Dir::Ltr), Just(Dir::Rtl)]),
        )
            .prop_map(|(lex, tag, dir)| Spec::Lang(lex, tag, dir)),
        (any::<String>(), "[A-Za-z0-9]{1,8}")
            .prop_map(|(lex, dt)| Spec::Typed(lex, format!("http://dt.example/{dt}"))),
    ];
    leaf.prop_recursive(3, 12, 2, |inner| {
        (arb_subject(), arb_iri().prop_map(Spec::Iri), inner)
            .prop_map(|(s, p, o)| Spec::Triple(Box::new(s), Box::new(p), Box::new(o)))
    })
}

fn build(spec: &Spec) -> Term {
    match spec {
        Spec::Iri(i) => Term::iri(i).unwrap(),
        Spec::Blank(l) => Term::blank_node(l).unwrap(),
        Spec::Simple(lex) => Term::literal_simple(lex),
        Spec::Lang(lex, tag, dir) => Term::literal_lang(lex, tag, *dir).unwrap(),
        Spec::Typed(lex, dt) => Term::literal_typed(lex, dt).unwrap(),
        Spec::Triple(s, p, o) => Term::triple_term(&build(s), &build(p), &build(o)).unwrap(),
    }
}

fn check(spec: &Spec, got: TermRef<'_>) {
    match (spec, got) {
        (Spec::Iri(i), TermRef::Iri(g)) => assert_eq!(g, i),
        (Spec::Blank(l), TermRef::BlankNode(g)) => assert_eq!(g, l),
        (Spec::Simple(lex), TermRef::Literal(p)) => {
            assert_eq!(p.lexical(), lex);
            assert_eq!(p.lang(), None);
            assert_eq!(p.datatype(), vocab::XSD_STRING);
        }
        (Spec::Lang(lex, tag, dir), TermRef::Literal(p)) => {
            assert_eq!(p.lexical(), lex);
            // Generated tags are already lowercase, so normalization is identity.
            assert_eq!(p.lang(), Some((tag.as_str(), *dir)));
        }
        (Spec::Typed(lex, dt), TermRef::Literal(p)) => {
            assert_eq!(p.lexical(), lex);
            assert_eq!(p.datatype(), dt);
        }
        (Spec::Triple(s, p, o), TermRef::TripleTerm(tt)) => {
            check(s, tt.subject());
            check(p, tt.predicate());
            check(o, tt.object());
        }
        (spec, got) => panic!("kind mismatch: {spec:?} decoded as {got:?}"),
    }
}

proptest! {
    #[test]
    fn concise_round_trip(spec in arb_spec()) {
        let term = build(&spec);
        check(&spec, term.as_term_ref());
        // Adopting the bytes reproduces an equal term; construction is
        // deterministic.
        prop_assert_eq!(&Term::from_concise(term.as_concise()).unwrap(), &term);
        prop_assert_eq!(&build(&spec), &term);
    }

    #[test]
    fn term_total_order_laws(a in arb_spec(), b in arb_spec(), c in arb_spec()) {
        let (ta, tb, tc) = (build(&a), build(&b), build(&c));
        prop_assert_eq!(ta.cmp(&tb), tb.cmp(&ta).reverse());
        prop_assert_eq!(ta == tb, ta.cmp(&tb) == Ordering::Equal);
        if ta <= tb && tb <= tc {
            prop_assert!(ta <= tc);
        }
        // Term order is definitionally concise byte order.
        prop_assert_eq!(ta.cmp(&tb), ta.as_concise().cmp(tb.as_concise()));
    }

    #[test]
    fn decode_never_panics(bytes in proptest::collection::vec(any::<u8>(), 0..64)) {
        let _ = concise::decode(&bytes);
    }

    #[test]
    fn id_decode_never_panics(raw in any::<u64>()) {
        if let Some(v) = TermId::from_raw(raw).decode() {
            let _ = v.canonical_lexical();
            let _ = v.datatype_iri();
        }
    }
}

// ------------------------------------------------- integers vs naive model

fn arb_wide_int() -> impl Strategy<Value = i128> {
    prop_oneof![
        any::<i64>().prop_map(i128::from),
        -(1i128 << 62)..(1i128 << 62),
        -1000i128..1000,
        Just((1i128 << 59) - 1),
        Just(1i128 << 59),
        Just(-(1i128 << 59)),
        Just(-(1i128 << 59) - 1),
    ]
}

proptest! {
    #[test]
    fn integer_inline_matches_model(v in arb_wide_int()) {
        let lex = v.to_string();
        let id = TermId::try_inline(&lex, vocab::XSD_INTEGER);
        let fits = (-(1i128 << 59)..(1i128 << 59)).contains(&v);
        prop_assert_eq!(id.is_some(), fits, "{}", lex);
        if let Some(id) = id {
            let decoded = id.decode().unwrap();
            prop_assert_eq!(&decoded, &InlineValue::Integer(v as i64));
            prop_assert_eq!(decoded.canonical_lexical(), lex);
        }
        // Valid but non-canonical spellings never inline.
        prop_assert_eq!(TermId::try_inline(&format!("+{v}"), vocab::XSD_INTEGER), None);
        prop_assert_eq!(TermId::try_inline(&format!("0{v}"), vocab::XSD_INTEGER), None);
    }

    #[test]
    fn integer_id_order_is_value_order(a in arb_wide_int(), b in arb_wide_int()) {
        let (Some(ia), Some(ib)) = (
            TermId::try_inline(&a.to_string(), vocab::XSD_INTEGER),
            TermId::try_inline(&b.to_string(), vocab::XSD_INTEGER),
        ) else {
            return Ok(());
        };
        // Raw id order (offset-binary payload) equals value order.
        prop_assert_eq!(ia.cmp(&ib), a.cmp(&b));
        prop_assert_eq!(graphy_core::id::partial_cmp_value(ia, ib), Some(a.cmp(&b)));
    }
}

// ------------------------------------------------- decimals vs naive model

/// Canonicalize an xsd:decimal lexical by string surgery alone (independent
/// of the library's parse/render pair). `None` = not a valid decimal lexical.
fn naive_canonical_decimal(lex: &str) -> Option<String> {
    let (sign, rest) = match lex.strip_prefix('-') {
        Some(r) => ("-", r),
        None => ("", lex.strip_prefix('+').unwrap_or(lex)),
    };
    let (int_part, frac_part) = match rest.split_once('.') {
        Some((i, f)) => (i, f),
        None => (rest, ""),
    };
    if int_part.is_empty() && frac_part.is_empty() {
        return None;
    }
    if !int_part.bytes().all(|b| b.is_ascii_digit())
        || !frac_part.bytes().all(|b| b.is_ascii_digit())
    {
        return None;
    }
    let int_t = int_part.trim_start_matches('0');
    let frac_t = frac_part.trim_end_matches('0');
    let int_c = if int_t.is_empty() { "0" } else { int_t };
    let frac_c = if frac_t.is_empty() { "0" } else { frac_t };
    let sign = if int_c == "0" && frac_c == "0" {
        ""
    } else {
        sign
    };
    Some(format!("{sign}{int_c}.{frac_c}"))
}

/// Whether a canonical decimal fits the inline payload (|unscaled| < 2⁵¹,
/// scale ≤ 255), computed from the digit strings.
fn naive_fits_inline(canon: &str) -> bool {
    let mag = canon.strip_prefix('-').unwrap_or(canon);
    let (int_c, frac_c) = mag.split_once('.').unwrap();
    if frac_c.len() > 255 {
        return false;
    }
    let digits: String = int_c.chars().chain(frac_c.chars()).collect();
    match digits.parse::<i128>() {
        Ok(u) => u < 1 << 51, // magnitude; sign handled by symmetric range
        Err(_) => false,
    }
}

/// Compare two canonical decimals by string alignment alone.
fn naive_cmp_decimal(a: &str, b: &str) -> Ordering {
    match (a.starts_with('-'), b.starts_with('-')) {
        (true, false) => Ordering::Less,
        (false, true) => Ordering::Greater,
        (neg, _) => {
            let split = |s: &str| {
                let s = s.strip_prefix('-').unwrap_or(s).to_owned();
                let (i, f) = s.split_once('.').unwrap();
                (i.to_owned(), f.to_owned())
            };
            let (ia, mut fa) = split(a);
            let (ib, mut fb) = split(b);
            let flen = fa.len().max(fb.len());
            fa.push_str(&"0".repeat(flen - fa.len()));
            fb.push_str(&"0".repeat(flen - fb.len()));
            // Canonical integer parts have no leading zeros: (len, lex) order.
            let ord = (ia.len(), ia, fa).cmp(&(ib.len(), ib, fb));
            if neg {
                ord.reverse()
            } else {
                ord
            }
        }
    }
}

fn arb_decimal_lex() -> impl Strategy<Value = String> {
    ("[+-]?", "[0-9]{0,20}", proptest::option::of("[0-9]{0,20}")).prop_map(|(sign, int, frac)| {
        match frac {
            Some(f) => format!("{sign}{int}.{f}"),
            None => format!("{sign}{int}"),
        }
    })
}

proptest! {
    #[test]
    fn decimal_inline_matches_model(lex in arb_decimal_lex()) {
        let id = TermId::try_inline(&lex, vocab::XSD_DECIMAL);
        match naive_canonical_decimal(&lex) {
            None => prop_assert_eq!(id, None, "invalid lexical {:?} inlined", lex),
            Some(canon) if canon != lex => {
                prop_assert_eq!(id, None, "non-canonical {:?} inlined", lex);
                // The canonical spelling of the same value inlines iff it fits.
                prop_assert_eq!(
                    TermId::try_inline(&canon, vocab::XSD_DECIMAL).is_some(),
                    naive_fits_inline(&canon)
                );
            }
            Some(canon) => {
                prop_assert_eq!(id.is_some(), naive_fits_inline(&canon));
                if let Some(id) = id {
                    prop_assert_eq!(id.decode().unwrap().canonical_lexical(), lex);
                }
            }
        }
    }

    #[test]
    fn decimal_value_order_matches_model(a in arb_decimal_lex(), b in arb_decimal_lex()) {
        let (Some(ca), Some(cb)) = (naive_canonical_decimal(&a), naive_canonical_decimal(&b))
        else {
            return Ok(());
        };
        let (Some(ia), Some(ib)) = (
            TermId::try_inline(&ca, vocab::XSD_DECIMAL),
            TermId::try_inline(&cb, vocab::XSD_DECIMAL),
        ) else {
            return Ok(());
        };
        prop_assert_eq!(
            graphy_core::id::partial_cmp_value(ia, ib),
            Some(naive_cmp_decimal(&ca, &cb)),
            "{} vs {}", ca, cb
        );
    }

    #[test]
    fn integer_decimal_cross_compare_matches_model(v in arb_wide_int(), lex in arb_decimal_lex()) {
        let Some(iv) = TermId::try_inline(&v.to_string(), vocab::XSD_INTEGER) else {
            return Ok(());
        };
        let Some(canon) = naive_canonical_decimal(&lex) else { return Ok(()); };
        let Some(id) = TermId::try_inline(&canon, vocab::XSD_DECIMAL) else { return Ok(()); };
        // An integer's exact decimal spelling is `v.0`, already canonical.
        let expected = naive_cmp_decimal(&format!("{v}.0"), &canon);
        prop_assert_eq!(
            graphy_core::id::partial_cmp_value(iv, id),
            Some(expected),
            "{} vs {}", v, canon
        );
    }
}

// -------------------------------------------------- doubles and dateTimes

proptest! {
    #[test]
    fn double_render_parse_identity(bits in any::<u64>()) {
        // Zero the low 5 mantissa bits so the value is inlinable by
        // construction; skip NaNs (only the canonical NaN's spelling inlines).
        let v = f64::from_bits(bits & !0x1F);
        prop_assume!(!v.is_nan());
        let id = TermId::inline(InlineValue::Double { value: v, declared_float: false }).unwrap();
        let lex = id.decode().unwrap().canonical_lexical();
        prop_assert_eq!(TermId::try_inline(&lex, vocab::XSD_DOUBLE), Some(id), "{}", lex);
    }

    #[test]
    fn float_render_parse_identity(bits in any::<u32>()) {
        let f = f32::from_bits(bits);
        prop_assume!(!f.is_nan());
        let id = TermId::inline(InlineValue::Double {
            value: f64::from(f),
            declared_float: true,
        })
        .expect("f32 widening always inlinable");
        let lex = id.decode().unwrap().canonical_lexical();
        prop_assert_eq!(TermId::try_inline(&lex, vocab::XSD_FLOAT), Some(id), "{}", lex);
    }

    #[test]
    fn datetime_render_parse_identity(
        date_only in any::<bool>(),
        seconds in -(1i64 << 40)..(1i64 << 40),
        millis in 0u16..1000,
        tz in proptest::option::of(-56i8..=56),
    ) {
        let dt = if date_only {
            // Dates are wall-clock midnights: snap to the enclosing day.
            let wall = seconds + i64::from(tz.unwrap_or(0)) * 900;
            let wall = wall - wall.rem_euclid(86400);
            InlineDateTime {
                date_only,
                seconds: wall - i64::from(tz.unwrap_or(0)) * 900,
                millis: 0,
                tz_quarters: tz,
            }
        } else {
            InlineDateTime { date_only, seconds, millis, tz_quarters: tz }
        };
        // Snapping may step past the i41 floor; skip those.
        let Some(id) = TermId::inline(InlineValue::DateTime(dt)) else { return Ok(()); };
        let decoded = id.decode().unwrap();
        let lex = decoded.canonical_lexical();
        prop_assert_eq!(
            TermId::try_inline(&lex, decoded.datatype_iri()),
            Some(id),
            "{}", lex
        );
    }
}
