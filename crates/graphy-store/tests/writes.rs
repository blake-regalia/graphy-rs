//! M4 delta-layer tests: `Store::apply` + snapshot scans/counts against a
//! naive model across random commit interleavings, snapshot isolation, and
//! overlay-term round-trips.

use std::collections::BTreeSet;
use std::path::PathBuf;

use graphy_store::{BuilderConfig, Order, Profile, QuadBatch, SegmentBuilder, Store, TermPos};
use proptest::prelude::*;

/// Owned concise quad.
type CQuad = (Vec<u8>, Vec<u8>, Vec<u8>, Option<Vec<u8>>);

fn iri(space: &str, i: u8) -> Vec<u8> {
    format!(">http://x/{space}{i}").into_bytes()
}

fn object_bytes(o: u8) -> Vec<u8> {
    match o % 3 {
        0 => iri("s", o % 6), // overlaps subjects → shared
        1 => format!("\"lit{}", o % 5).into_bytes(),
        _ => format!("^>http://www.w3.org/2001/XMLSchema#integer\"{}", o % 7).into_bytes(),
    }
}

fn quad(s: u8, p: u8, o: u8, g: Option<u8>) -> CQuad {
    (
        iri("s", s % 6),
        iri("p", p % 3),
        object_bytes(o),
        g.map(|g| iri("g", g % 2)),
    )
}

/// Base segment with roughly half the quad universe.
fn build_base(dir: &PathBuf, quads: &BTreeSet<CQuad>) {
    let mut cfg = BuilderConfig::new(dir);
    cfg.profile = Profile::Balanced;
    cfg.sort_budget = 1 << 14;
    let mut b = SegmentBuilder::new(cfg).unwrap();
    for (s, p, o, g) in quads {
        b.push_quad(s, p, o, g.as_deref()).unwrap();
    }
    b.finish().unwrap();
}

fn dump(snap: &graphy_store::Snapshot) -> BTreeSet<CQuad> {
    let mut out = BTreeSet::new();
    let pat = snap.resolve_pattern(None, None, None, None).unwrap();
    for order in [Order::Spo] {
        let mut scan = snap.scan(&pat, order).unwrap();
        let mut batch = QuadBatch::new();
        out.clear();
        while scan.next_batch(&mut batch).unwrap() {
            for i in 0..batch.len() {
                let s = snap.decode_value(batch.s[i], TermPos::Subject).unwrap();
                let p = snap.decode_value(batch.p[i], TermPos::Predicate).unwrap();
                let o = snap.decode_value(batch.o[i], TermPos::Object).unwrap();
                let g = (batch.g[i] > 0)
                    .then(|| snap.decode_value(batch.g[i], TermPos::Graph).unwrap());
                out.insert((s, p, o, g));
            }
        }
    }
    out
}

static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn scratch() -> PathBuf {
    std::env::temp_dir().join(format!(
        "graphy-store-writes-{}-{}",
        std::process::id(),
        SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ))
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(32))]
    /// Random base + random commit script (each commit deletes then adds a
    /// few random quads): every intermediate snapshot equals the model, for
    /// scans in every ordering, counts, and pattern queries; earlier
    /// snapshots stay frozen. Some snapshots are released immediately and
    /// [`Store::gc`] interleaves randomly — reclamation must never change
    /// what any still-pinned snapshot reads.
    #[test]
    fn apply_matches_naive_model(
        base_raw in proptest::collection::vec((0u8..6, 0u8..3, 0u8..9, proptest::option::of(0u8..2)), 0..40),
        script in proptest::collection::vec(
            (
                proptest::collection::vec((0u8..6, 0u8..3, 0u8..9, proptest::option::of(0u8..2)), 0..4), // dels
                proptest::collection::vec((0u8..6, 0u8..3, 0u8..12, proptest::option::of(0u8..2)), 0..4), // adds (wider o range → overlay terms)
                any::<bool>(), // keep this snapshot pinned in history
                any::<bool>(), // run epoch GC after this commit
            ),
            1..6,
        ),
    ) {
        let dir = scratch();
        let _ = std::fs::remove_dir_all(&dir);
        let base: BTreeSet<CQuad> = base_raw.iter().map(|&(s, p, o, g)| quad(s, p, o, g)).collect();
        build_base(&dir, &base);
        let store = Store::open(&dir).unwrap();

        let mut model = base.clone();
        let mut history: Vec<(std::sync::Arc<graphy_store::Snapshot>, BTreeSet<CQuad>)> =
            vec![(store.snapshot(), model.clone())];

        for (dels, adds, keep, do_gc) in &script {
            let dels: Vec<CQuad> = dels.iter().map(|&(s, p, o, g)| quad(s, p, o, g)).collect();
            let adds: Vec<CQuad> = adds.iter().map(|&(s, p, o, g)| quad(s, p, o, g)).collect();
            let del_refs: Vec<_> = dels.iter().map(|q| (q.0.as_slice(), q.1.as_slice(), q.2.as_slice(), q.3.as_deref())).collect();
            let add_refs: Vec<_> = adds.iter().map(|q| (q.0.as_slice(), q.1.as_slice(), q.2.as_slice(), q.3.as_deref())).collect();
            let snap = store.apply(&del_refs, &add_refs).unwrap();
            for d in &dels {
                model.remove(d);
            }
            for a in &adds {
                model.insert(a.clone());
            }
            if *keep {
                history.push((snap, model.clone()));
            } else {
                drop(snap); // release the pin → this epoch becomes reclaimable
            }
            if *do_gc {
                store.gc();
            }
        }
        // The current snapshot is always verified against the final model.
        history.push((store.snapshot(), model.clone()));

        // Every historical snapshot equals its model (isolation + zipper).
        for (snap, expected) in &history {
            prop_assert_eq!(&dump(snap), expected, "epoch {}", snap.epoch());
            // Counts across pattern shapes agree with the model.
            for s in [None, Some(iri("s", 2))] {
                for p in [None, Some(iri("p", 1))] {
                    for g in [None, Some(None), Some(Some(iri("g", 0)))] {
                        let want = expected
                            .iter()
                            .filter(|(ms, mp, _, mg)| {
                                s.as_ref().is_none_or(|x| x == ms)
                                    && p.as_ref().is_none_or(|x| x == mp)
                                    && match &g {
                                        None => true,
                                        Some(None) => mg.is_none(),
                                        Some(Some(x)) => mg.as_ref() == Some(x),
                                    }
                            })
                            .count() as u64;
                        let pat = snap.resolve_pattern(
                            s.as_deref(),
                            p.as_deref(),
                            None,
                            g.as_ref().map(|x| x.as_deref()),
                        );
                        match pat {
                            None => prop_assert_eq!(want, 0),
                            Some(pat) => {
                                prop_assert_eq!(snap.count(&pat).unwrap(), want, "count {:?}", pat);
                                // Scans agree in every available ordering.
                                for order in snap.scan_orders() {
                                    let mut scan = snap.scan(&pat, order).unwrap();
                                    let mut batch = QuadBatch::with_capacity(3);
                                    let mut n = 0u64;
                                    let mut last: Option<[u64; 4]> = None;
                                    while scan.next_batch(&mut batch).unwrap() {
                                        for i in 0..batch.len() {
                                            let q = [batch.s[i], batch.p[i], batch.o[i], batch.g[i]];
                                            let [x, y, z] = order.to_xyz(q[0], q[1], q[2]);
                                            let key = [x, y, z, q[3]];
                                            prop_assert!(last.is_none_or(|l| l < key), "unordered");
                                            last = Some(key);
                                            n += 1;
                                        }
                                    }
                                    prop_assert_eq!(n, want, "scan {:?} via {}", pat, order.name());
                                }
                            }
                        }
                    }
                }
            }
        }
        std::fs::remove_dir_all(&dir).ok();
    }
}

#[test]
fn overlay_terms_round_trip() {
    let dir = scratch();
    build_base(&dir, &BTreeSet::from([quad(0, 0, 1, None)]));
    let store = Store::open(&dir).unwrap();

    // Add quads whose every term is new (subject, predicate, object, graph).
    let s = b">http://new/subject".to_vec();
    let p = b">http://new/predicate".to_vec();
    let o = "\"a brand new literal".as_bytes().to_vec();
    let g = b">http://new/graph".to_vec();
    let snap = store.apply(&[], &[(&s, &p, &o, Some(&g))]).unwrap();

    assert_eq!(snap.delta_events(), 1);
    let all = dump(&snap);
    assert!(all.contains(&(s.clone(), p.clone(), o.clone(), Some(g.clone()))));

    // Pattern resolution through overlay terms.
    let pat = snap
        .resolve_pattern(Some(&s), Some(&p), Some(&o), Some(Some(&g)))
        .expect("overlay terms resolve");
    assert_eq!(snap.count(&pat).unwrap(), 1);

    // Public TermId round-trip for an overlay subject.
    let id = snap.resolve(&s, TermPos::Subject).unwrap();
    assert_eq!(snap.decode(id).unwrap(), s);
    assert_eq!(
        snap.column(id, TermPos::Subject),
        snap.resolve_pattern(Some(&s), None, None, None).unwrap().s
    );

    // Delete it again; a later snapshot is empty of it, the old one is not.
    let snap2 = store.apply(&[(&s, &p, &o, Some(&g))], &[]).unwrap();
    assert_eq!(snap2.count(&pat).unwrap(), 0);
    assert_eq!(snap.count(&pat).unwrap(), 1, "old snapshot frozen");

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn base_delete_and_readd() {
    let dir = scratch();
    let q = quad(1, 1, 0, Some(0));
    build_base(&dir, &BTreeSet::from([q.clone(), quad(2, 0, 4, None)]));
    let store = Store::open(&dir).unwrap();
    let qr = (
        q.0.as_slice(),
        q.1.as_slice(),
        q.2.as_slice(),
        q.3.as_deref(),
    );

    let s0 = store.snapshot();
    let s1 = store.apply(&[qr], &[]).unwrap(); // delete base quad
    let s2 = store.apply(&[], &[qr]).unwrap(); // re-add it
    let s3 = store.apply(&[qr], &[qr]).unwrap(); // delete+add in one commit

    assert!(dump(&s0).contains(&q));
    assert!(!dump(&s1).contains(&q));
    assert!(dump(&s2).contains(&q));
    assert!(dump(&s3).contains(&q));
    assert_eq!(dump(&s0).len(), 2);
    assert_eq!(dump(&s1).len(), 1);

    // Deleting a never-present quad is a no-op.
    let ghost = quad(5, 2, 8, Some(1));
    let before = store.snapshot().delta_events();
    store
        .apply(
            &[(
                ghost.0.as_slice(),
                ghost.1.as_slice(),
                ghost.2.as_slice(),
                ghost.3.as_deref(),
            )],
            &[],
        )
        .unwrap();
    assert_eq!(store.snapshot().delta_events(), before);

    std::fs::remove_dir_all(&dir).ok();
}
