//! Property test: random quad sets → segment → every pattern shape agrees
//! with a naive in-memory model (M2 exit criterion: scans and exact counts
//! are correct for arbitrary data).

use std::collections::BTreeSet;
use std::sync::atomic::{AtomicU64, Ordering};

use graphy_store::{BuilderConfig, Profile, Segment, SegmentBuilder, TermPos};
use proptest::prelude::*;

type Quad = (u8, u8, u8, Option<u8>);

/// A decoded concise quad for model comparison.
type MQuad = (Vec<u8>, Vec<u8>, Vec<u8>, Option<Vec<u8>>);

fn iri(space: &str, i: u8) -> Vec<u8> {
    format!(">http://x/{space}{i}").into_bytes()
}

/// Object universe: IRIs (overlapping the subject space for shared terms),
/// plain literals, and inline-able integers.
fn object_bytes(o: u8) -> Vec<u8> {
    match o % 3 {
        0 => iri("s", o % 8), // overlaps subjects → shared section
        1 => format!("\"lit{}", o % 5).into_bytes(),
        _ => format!("^>http://www.w3.org/2001/XMLSchema#integer\"{}", o % 7).into_bytes(),
    }
}

static SEQ: AtomicU64 = AtomicU64::new(0);

fn build_segment(quads: &BTreeSet<Quad>, profile: Profile) -> Segment {
    let dir = std::env::temp_dir().join(format!(
        "graphy-store-prop-{}-{}",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    let mut cfg = BuilderConfig::new(&dir);
    cfg.profile = profile;
    cfg.sort_budget = 1 << 12; // force spills
    let mut b = SegmentBuilder::new(cfg).unwrap();
    for &(s, p, o, g) in quads {
        b.push_quad(
            &iri("s", s % 8),
            &iri("p", p % 4),
            &object_bytes(o),
            g.map(|g| iri("g", g % 3)).as_deref(),
        )
        .unwrap();
    }
    b.finish().unwrap();
    let seg = Segment::open(&dir).unwrap();
    std::fs::remove_dir_all(&dir).ok();
    seg
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]
    #[test]
    fn scans_match_naive_model(
        raw in proptest::collection::vec(
            (0u8..8, 0u8..4, 0u8..12, proptest::option::of(0u8..3)),
            0..120,
        ),
    ) {
        // The naive model: the set of normalized concise quads.
        let quads: BTreeSet<Quad> = raw.into_iter().collect();
        let model: BTreeSet<MQuad> = quads
            .iter()
            .map(|&(s, p, o, g)| {
                (
                    iri("s", s % 8),
                    iri("p", p % 4),
                    object_bytes(o),
                    g.map(|g| iri("g", g % 3)),
                )
            })
            .collect();
        // Balanced exercises materialized-ordering scans; compact exercises
        // the FoQ accessors (P- and O-bound patterns route through Wp and
        // the O-index).
        let segments = [
            build_segment(&quads, Profile::Balanced),
            build_segment(&quads, Profile::Compact),
        ];

        // Every pattern over the term universe (+ absent terms) must agree.
        let subjects: Vec<Option<Vec<u8>>> =
            std::iter::once(None).chain((0..8).map(|i| Some(iri("s", i)))).collect();
        let predicates: Vec<Option<Vec<u8>>> =
            std::iter::once(None).chain((0..4).map(|i| Some(iri("p", i)))).collect();
        let objects: Vec<Option<Vec<u8>>> =
            std::iter::once(None).chain((0..12).map(object_bytes).map(Some)).collect();
        // Graph filter: any, default-only, each named graph.
        let graph_filters: Vec<Option<Option<Vec<u8>>>> = std::iter::once(None)
            .chain(std::iter::once(Some(None)))
            .chain((0..3).map(|i| Some(Some(iri("g", i)))))
            .collect();

        for s in &subjects {
            for p in &predicates {
                for o in &objects {
                    for g in &graph_filters {
                        let expected: BTreeSet<_> = model
                            .iter()
                            .filter(|(ms, mp, mo, mg)| {
                                s.as_ref().is_none_or(|x| x == ms)
                                    && p.as_ref().is_none_or(|x| x == mp)
                                    && o.as_ref().is_none_or(|x| x == mo)
                                    && match g {
                                        None => true,
                                        Some(None) => mg.is_none(),
                                        Some(Some(x)) => mg.as_ref() == Some(x),
                                    }
                            })
                            .cloned()
                            .collect();
                        for seg in &segments {
                            let pat = seg.resolve_pattern(
                                s.as_deref(),
                                p.as_deref(),
                                o.as_deref(),
                                g.as_ref().map(|x| x.as_deref()),
                            );
                            let Some(pat) = pat else {
                                prop_assert!(
                                    expected.is_empty(),
                                    "unresolved pattern with matches"
                                );
                                continue;
                            };
                            let got: BTreeSet<_> = seg
                                .scan(&pat)
                                .unwrap()
                                .into_iter()
                                .map(|q| {
                                    (
                                        seg.decode_value(q[0], TermPos::Subject).unwrap(),
                                        seg.decode_value(q[1], TermPos::Predicate).unwrap(),
                                        seg.decode_value(q[2], TermPos::Object).unwrap(),
                                        (q[3] > 0).then(|| {
                                            seg.decode_value(q[3] - 1, TermPos::Graph).unwrap()
                                        }),
                                    )
                                })
                                .collect();
                            prop_assert_eq!(&got, &expected, "scan {:?}", pat);
                            prop_assert_eq!(
                                seg.count(&pat).unwrap(),
                                expected.len() as u64,
                                "count {:?}",
                                pat
                            );
                        }
                    }
                }
            }
        }
    }
}
