//! M5 merger tests (doc 07 §6, §8): folding the delta into a new base
//! generation must preserve every reader's view — current snapshots equal
//! the model, pre-merge snapshots stay frozen on the old generation, the
//! store reopens onto the new generation with its epoch intact (rotated
//! WAL + `CURRENT` pointer), concurrent commits during a merge survive as
//! the remapped active suffix, retired generations unlink only after their
//! last snapshot drops, and the merged segment is byte-identical to an
//! offline rebuild of the same dataset (§8 invariant 3).

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use graphy_store::{
    resolve_segment_dir, BuilderConfig, MergeConfig, OpenMode, Order, Profile, QuadBatch,
    SegmentBuilder, Snapshot, Store, TermPos, CURRENT_NAME,
};
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

fn build_base(dir: &Path, quads: &BTreeSet<CQuad>) {
    let mut cfg = BuilderConfig::new(dir);
    cfg.profile = Profile::Balanced;
    cfg.sort_budget = 1 << 14;
    let mut b = SegmentBuilder::new(cfg).unwrap();
    for (s, p, o, g) in quads {
        b.push_quad(s, p, o, g.as_deref()).unwrap();
    }
    b.finish().unwrap();
}

fn refs(v: &[CQuad]) -> Vec<graphy_store::QuadTerms<'_>> {
    v.iter()
        .map(|q| {
            (
                q.0.as_slice(),
                q.1.as_slice(),
                q.2.as_slice(),
                q.3.as_deref(),
            )
        })
        .collect()
}

fn dump(snap: &Snapshot) -> BTreeSet<CQuad> {
    let mut out = BTreeSet::new();
    let pat = snap.resolve_pattern(None, None, None, None).unwrap();
    let mut scan = snap.scan(&pat, Order::Spo).unwrap();
    let mut batch = QuadBatch::new();
    while scan.next_batch(&mut batch).unwrap() {
        for i in 0..batch.len() {
            let s = snap.decode_value(batch.s[i], TermPos::Subject).unwrap();
            let p = snap.decode_value(batch.p[i], TermPos::Predicate).unwrap();
            let o = snap.decode_value(batch.o[i], TermPos::Object).unwrap();
            let g =
                (batch.g[i] > 0).then(|| snap.decode_value(batch.g[i], TermPos::Graph).unwrap());
            out.insert((s, p, o, g));
        }
    }
    out
}

static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn scratch() -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "graphy-store-merge-{}-{}",
        std::process::id(),
        SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

/// Small merge budget so tests exercise the external-sort path.
fn merge_cfg() -> MergeConfig {
    MergeConfig {
        sort_budget: 1 << 14,
        ..MergeConfig::default()
    }
}

#[test]
fn merge_folds_delta_and_isolates_old_snapshots() {
    let dir = scratch();
    let base: BTreeSet<CQuad> = (0..12)
        .map(|i| quad(i, i, i, (i % 4 == 0).then_some(i)))
        .collect();
    build_base(&dir, &base);
    let store = Store::open(&dir).unwrap();

    // Deletes of base quads, adds with brand-new terms (subject, predicate,
    // object literal, graph) → overlay ids that the fold must re-home.
    let dels: Vec<CQuad> = base.iter().take(3).cloned().collect();
    let adds: Vec<CQuad> = vec![
        (
            b">http://new/s".to_vec(),
            b">http://new/p".to_vec(),
            "\"fresh literal".as_bytes().to_vec(),
            Some(b">http://new/g".to_vec()),
        ),
        quad(1, 2, 10, None),
    ];
    store.apply(&refs(&dels), &refs(&adds)).unwrap();
    let mut model = base.clone();
    for d in &dels {
        model.remove(d);
    }
    for a in &adds {
        model.insert(a.clone());
    }

    let pre = store.snapshot();
    let pre_model = model.clone();
    let pre_epoch = pre.epoch();

    let merged = store.merge_with(&merge_cfg()).unwrap();
    assert_eq!(merged.generation(), 1);
    assert_eq!(merged.epoch(), pre_epoch, "merge must not consume epochs");
    assert_eq!(
        merged.delta_events(),
        0,
        "no suffix without concurrent writes"
    );
    assert!(!store.needs_merge());
    assert_eq!(dump(&merged), model);
    assert_eq!(dump(&store.snapshot()), model);

    // The pre-merge snapshot still reads the old generation, unchanged.
    assert_eq!(pre.generation(), 0);
    assert_eq!(dump(&pre), pre_model);

    // Writes continue on the new generation.
    let more: Vec<CQuad> = vec![quad(3, 1, 4, Some(1))];
    let snap = store.apply(&[], &refs(&more)).unwrap();
    for a in &more {
        model.insert(a.clone());
    }
    assert_eq!(dump(&snap), model);
    assert_eq!(snap.epoch(), pre_epoch + 1);

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn merge_reopen_restores_epoch_and_data() {
    let dir = scratch();
    let base: BTreeSet<CQuad> = (0..8).map(|i| quad(i, i, i + 1, None)).collect();
    build_base(&dir, &base);
    let mut model = base.clone();

    {
        let store = Store::open(&dir).unwrap();
        let adds: Vec<CQuad> = vec![quad(0, 1, 2, Some(0)), quad(5, 2, 7, None)];
        let dels: Vec<CQuad> = base.iter().take(2).cloned().collect();
        store.apply(&refs(&dels), &refs(&adds)).unwrap();
        for d in &dels {
            model.remove(d);
        }
        for a in &adds {
            model.insert(a.clone());
        }
        let merged = store.merge_with(&merge_cfg()).unwrap();
        assert_eq!(merged.epoch(), 1);

        // Post-merge commit → the rotated WAL must carry it on reopen.
        let post: Vec<CQuad> = vec![quad(2, 2, 11, Some(1))];
        store.apply(&refs(&post), &[]).unwrap(); // no-op delete (absent quad)
        store.apply(&[], &refs(&post)).unwrap();
        for a in &post {
            model.insert(a.clone());
        }
    }

    // CURRENT resolves to the new generation for tooling too.
    let seg_dir = resolve_segment_dir(&dir).unwrap();
    assert_ne!(seg_dir, dir);
    assert!(seg_dir.join("MANIFEST.json").exists());

    for mode in [OpenMode::Heap, OpenMode::Mmap] {
        let store = Store::open_with(&dir, mode).unwrap();
        let snap = store.snapshot();
        assert_eq!(snap.generation(), 1, "{mode:?}");
        assert_eq!(snap.epoch(), 2, "epoch restored via checkpoint + suffix");
        assert_eq!(dump(&snap), model, "{mode:?}");
    }

    // Writable after reopen; epochs continue.
    let store = Store::open(&dir).unwrap();
    let extra: Vec<CQuad> = vec![quad(4, 0, 3, None)];
    let snap = store.apply(&[], &refs(&extra)).unwrap();
    assert_eq!(snap.epoch(), 3);

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn merge_equals_offline_rebuild() {
    let dir = scratch();
    let base: BTreeSet<CQuad> = (0..20)
        .map(|i| quad(i, i, i, (i % 3 == 0).then_some(i)))
        .collect();
    build_base(&dir, &base);
    let store = Store::open(&dir).unwrap();

    let dels: Vec<CQuad> = base.iter().step_by(3).cloned().collect();
    let adds: Vec<CQuad> = (0..6)
        .map(|i| quad(i, i + 1, i + 5, (i % 2 == 0).then_some(i)))
        .collect();
    store.apply(&refs(&dels), &refs(&adds)).unwrap();
    let mut model = base.clone();
    for d in &dels {
        model.remove(d);
    }
    for a in &adds {
        model.insert(a.clone());
    }
    store.merge_with(&merge_cfg()).unwrap();

    // Offline rebuild of the equivalent flat dataset (same profile).
    let offline = scratch();
    build_base(&offline, &model);

    // Every component byte-identical (§8 invariant 3: deterministic builds).
    let gen_dir = resolve_segment_dir(&dir).unwrap();
    for sub in ["dict", "idx", "graphs", "stats"] {
        let a = gen_dir.join(sub);
        let b = offline.join(sub);
        let mut names: Vec<_> = std::fs::read_dir(&a)
            .map(|rd| rd.map(|e| e.unwrap().file_name()).collect())
            .unwrap_or_default();
        let mut names_b: Vec<_> = std::fs::read_dir(&b)
            .map(|rd| rd.map(|e| e.unwrap().file_name()).collect())
            .unwrap_or_default();
        names.sort();
        names_b.sort();
        assert_eq!(names, names_b, "component set {sub}/");
        for n in names {
            let fa = std::fs::read(a.join(&n)).unwrap();
            let fb = std::fs::read(b.join(&n)).unwrap();
            assert_eq!(fa, fb, "component {sub}/{}", n.to_string_lossy());
        }
    }

    std::fs::remove_dir_all(&dir).ok();
    std::fs::remove_dir_all(&offline).ok();
}

#[test]
fn merge_with_concurrent_writers_keeps_every_commit() {
    let dir = scratch();
    let base: BTreeSet<CQuad> = (0..24).map(|i| quad(i, i, i, None)).collect();
    build_base(&dir, &base);
    let store = Arc::new(Store::open(&dir).unwrap());

    // Each writer owns a disjoint slice of work: adds of novel quads and
    // deletes of its own base quads — the final state is order-independent.
    let n_threads = 4u8;
    let per_thread = 30u8;
    let mut model: BTreeSet<CQuad> = base.clone();
    for t in 0..n_threads {
        for k in 0..per_thread {
            model.insert((
                iri("w", t),
                iri("p", k % 3),
                format!("\"t{t}k{k}").into_bytes(),
                (k % 2 == 0).then(|| iri("g", t % 2)),
            ));
        }
        model.remove(&quad(t, t, t, None));
    }

    std::thread::scope(|s| {
        for t in 0..n_threads {
            let store = Arc::clone(&store);
            s.spawn(move || {
                let del = quad(t, t, t, None);
                store.apply(&refs(&[del]), &[]).unwrap();
                for k in 0..per_thread {
                    let add: CQuad = (
                        iri("w", t),
                        iri("p", k % 3),
                        format!("\"t{t}k{k}").into_bytes(),
                        (k % 2 == 0).then(|| iri("g", t % 2)),
                    );
                    store.apply(&[], &refs(&[add])).unwrap();
                }
            });
        }
        // Two merges race the writers (suffix remap both times).
        for _ in 0..2 {
            store.merge_with(&merge_cfg()).unwrap();
        }
    });

    let snap = store.snapshot();
    assert_eq!(dump(&snap), model);
    let final_epoch = snap.epoch();
    assert_eq!(
        final_epoch,
        u64::from(n_threads) * (u64::from(per_thread) + 1),
        "every commit consumed exactly one epoch"
    );
    drop(snap);

    // A final merge folds the leftovers; reopen agrees byte-for-byte.
    store.merge_with(&merge_cfg()).unwrap();
    drop(store);
    let store = Store::open(&dir).unwrap();
    let snap = store.snapshot();
    assert_eq!(dump(&snap), model);
    assert_eq!(snap.epoch(), final_epoch);

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn retirement_waits_for_the_last_old_snapshot() {
    let dir = scratch();
    let base: BTreeSet<CQuad> = (0..6).map(|i| quad(i, 0, i, None)).collect();
    build_base(&dir, &base);
    let store = Store::open(&dir).unwrap();

    let add: Vec<CQuad> = vec![quad(0, 1, 3, Some(0))];
    store.apply(&[], &refs(&add)).unwrap();
    let pre = store.snapshot();
    let pre_model = dump(&pre);

    store.merge_with(&merge_cfg()).unwrap();
    store.gc();
    // Old generation (at the store root) still pinned by `pre`.
    assert!(
        dir.join("MANIFEST.json").exists(),
        "gen 0 pinned → not retired"
    );
    assert_eq!(dump(&pre), pre_model, "pinned snapshot unaffected by merge");

    drop(pre);
    store.gc();
    assert!(!dir.join("MANIFEST.json").exists(), "gen 0 retired");
    assert!(!dir.join("dict").exists());
    assert!(dir.join(CURRENT_NAME).exists());

    // Second merge: gen-1 → gen-2, directory retirement.
    let gen1 = resolve_segment_dir(&dir).unwrap();
    store.apply(&[], &refs(&[quad(1, 1, 8, None)])).unwrap();
    store.merge_with(&merge_cfg()).unwrap();
    store.gc();
    assert!(!gen1.exists(), "gen 1 directory retired");
    let gen2 = resolve_segment_dir(&dir).unwrap();
    assert!(gen2.join("MANIFEST.json").exists());
    assert_eq!(store.snapshot().generation(), 2);

    std::fs::remove_dir_all(&dir).ok();
}

/// A paced merge (doc 07 §6.4 duty cycle) produces the same result as an
/// unpaced one — pacing only stretches the fold's wall time.
#[test]
fn paced_merge_preserves_state() {
    let dir = scratch();
    let base: BTreeSet<CQuad> = (0..30).map(|i| quad(i, i, i, None)).collect();
    build_base(&dir, &base);
    let store = Store::open(&dir).unwrap();

    store
        .apply(
            &refs(&[quad(0, 0, 0, None)]),
            &refs(&[quad(9, 1, 7, Some(1))]),
        )
        .unwrap();
    let mut expected: BTreeSet<CQuad> = base.clone();
    expected.remove(&quad(0, 0, 0, None));
    expected.insert(quad(9, 1, 7, Some(1)));

    let cfg = graphy_store::MergeConfig {
        pace_duty: Some(0.25),
        ..graphy_store::MergeConfig::default()
    };
    let merged = store.merge_with(&cfg).unwrap();
    assert_eq!(merged.generation(), 1);
    assert_eq!(dump(&merged), expected);
    // Out-of-range duties clamp instead of panicking or dividing by zero.
    for bad in [0.0, -1.0, 7.5] {
        let cfg = graphy_store::MergeConfig {
            pace_duty: Some(bad),
            ..graphy_store::MergeConfig::default()
        };
        store.merge_with(&cfg).unwrap();
    }
    assert_eq!(dump(&store.snapshot()), expected);
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn merge_relieves_budget_pressure() {
    let dir = scratch();
    let base: BTreeSet<CQuad> = (0..4).map(|i| quad(i, 0, i, None)).collect();
    build_base(&dir, &base);
    let store = Store::open(&dir).unwrap();
    store.set_delta_budget(2, 4);

    for i in 0..4 {
        store.apply(&[], &refs(&[quad(i, 1, i + 6, None)])).unwrap();
    }
    assert!(store.needs_merge());
    let over = store.apply(&[], &refs(&[quad(5, 2, 11, None)]));
    assert!(over.is_err(), "hard budget exhausted");

    store.merge_with(&merge_cfg()).unwrap();
    assert!(!store.needs_merge());
    store.apply(&[], &refs(&[quad(5, 2, 11, None)])).unwrap();

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn merge_edge_cases_empty_delta_and_empty_result() {
    let dir = scratch();
    let base: BTreeSet<CQuad> = (0..5)
        .map(|i| quad(i, i, i, (i == 0).then_some(0)))
        .collect();
    build_base(&dir, &base);
    let store = Store::open(&dir).unwrap();

    // Empty delta: the merge is an identical rebuild.
    let merged = store.merge_with(&merge_cfg()).unwrap();
    assert_eq!(merged.generation(), 1);
    assert_eq!(dump(&merged), base);

    // Delete everything (named graphs included) → empty generation.
    let all: Vec<CQuad> = base.iter().cloned().collect();
    store.apply(&refs(&all), &[]).unwrap();
    let merged = store.merge_with(&merge_cfg()).unwrap();
    assert_eq!(merged.generation(), 2);
    assert!(dump(&merged).is_empty());
    let pat = merged.resolve_pattern(None, None, None, None).unwrap();
    assert_eq!(merged.count(&pat).unwrap(), 0);

    // And the empty store still takes writes and merges.
    store.apply(&[], &refs(&[quad(1, 1, 1, Some(1))])).unwrap();
    let merged = store.merge_with(&merge_cfg()).unwrap();
    assert_eq!(dump(&merged), BTreeSet::from([quad(1, 1, 1, Some(1))]));

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn merge_carries_triple_terms_and_inline_values() {
    let dir = scratch();
    // Base with a triple term object and inline-value objects.
    let tt = {
        let mut out = Vec::new();
        graphy_core::concise::encode_triple_term(
            &mut out,
            &iri("s", 1),
            &iri("p", 1),
            "\"inner".as_bytes(),
        );
        out
    };
    let mut base: BTreeSet<CQuad> = BTreeSet::new();
    base.insert((iri("s", 0), iri("p", 0), tt.clone(), None));
    base.insert((iri("s", 2), iri("p", 1), object_bytes(2), None));
    build_base(&dir, &base);
    let store = Store::open(&dir).unwrap();

    // A NEW triple term via the write path (overlay object), plus a delete
    // of the base one — the fold drops its record, the add re-homes.
    let tt2 = {
        let mut out = Vec::new();
        graphy_core::concise::encode_triple_term(
            &mut out,
            &iri("s", 3),
            &iri("p", 2),
            "^>http://www.w3.org/2001/XMLSchema#integer\"42".as_bytes(),
        );
        out
    };
    let add: CQuad = (iri("s", 4), iri("p", 2), tt2.clone(), None);
    let del: CQuad = (iri("s", 0), iri("p", 0), tt.clone(), None);
    store
        .apply(
            &refs(std::slice::from_ref(&del)),
            &refs(std::slice::from_ref(&add)),
        )
        .unwrap();
    let mut model = base.clone();
    model.remove(&del);
    model.insert(add);

    let merged = store.merge_with(&merge_cfg()).unwrap();
    assert_eq!(dump(&merged), model);

    // Reopen: same view.
    drop(merged);
    drop(store);
    let store = Store::open(&dir).unwrap();
    assert_eq!(dump(&store.snapshot()), model);

    std::fs::remove_dir_all(&dir).ok();
}

/// Streaming-dict specifics (doc 07 §6.2): role migration (a base
/// subject-only term gains an object role through the delta and moves to
/// the shared section), garbage (a term whose only quad was tombstoned
/// vanishes from the merged dictionary), and a nested overlay quoted
/// triple whose component terms exist nowhere else.
#[test]
fn merge_migrates_roles_and_drops_garbage() {
    let dir = scratch();
    let term = |n: &str| format!(">http://x/{n}").into_bytes();
    let lit = |n: &str| format!("\"literal {n}").into_bytes();
    let mut base: BTreeSet<CQuad> = BTreeSet::new();
    base.insert((term("A"), term("P"), lit("x"), None));
    base.insert((term("B"), term("P"), lit("y"), None));
    build_base(&dir, &base);
    let store = Store::open(&dir).unwrap();

    // Nested quoted triple: <<E P2 <<F P2 "z">>>> — every component new.
    let tt_inner = {
        let mut out = Vec::new();
        graphy_core::concise::encode_triple_term(&mut out, &term("F"), &term("P2"), &lit("z"));
        out
    };
    let tt_outer = {
        let mut out = Vec::new();
        graphy_core::concise::encode_triple_term(&mut out, &term("E"), &term("P2"), &tt_inner);
        out
    };
    let dels: Vec<CQuad> = vec![(term("B"), term("P"), lit("y"), None)];
    let adds: Vec<CQuad> = vec![
        (term("C"), term("P"), term("A"), None), // A: subject → shared
        (term("D"), term("P"), tt_outer.clone(), None),
    ];
    store.apply(&refs(&dels), &refs(&adds)).unwrap();
    let mut model = base.clone();
    model.remove(&dels[0]);
    model.extend(adds.iter().cloned());

    let merged = store.merge_with(&merge_cfg()).unwrap();
    assert_eq!(dump(&merged), model);
    let c = &merged.segment().manifest.counts;
    // shared {A} · subjects {C, D, E, F} · predicates {P, P2} ·
    // objects {"x", "z"} · two tt records; B and "y" are gone.
    assert_eq!(
        (
            c.shared,
            c.subjects,
            c.predicates,
            c.objects,
            c.graphs,
            c.triple_terms
        ),
        (1, 4, 2, 2, 0, 2),
        "section counts after migration/garbage"
    );
    graphy_store::Segment::verify(&graphy_store::resolve_segment_dir(&dir).unwrap()).unwrap();

    // Reopen: same view.
    drop(merged);
    drop(store);
    let store = Store::open(&dir).unwrap();
    assert_eq!(dump(&store.snapshot()), model);
    std::fs::remove_dir_all(&dir).ok();
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(24))]
    /// Random base + random commit script with merges sprinkled between
    /// commits: after every step the current snapshot equals the model,
    /// historical snapshots stay frozen across generation swaps, and the
    /// store reopens onto the final state (rotated WAL + CURRENT).
    #[test]
    fn merge_matches_naive_model(
        base_raw in proptest::collection::vec((0u8..6, 0u8..3, 0u8..9, proptest::option::of(0u8..2)), 0..30),
        script in proptest::collection::vec(
            (
                proptest::collection::vec((0u8..6, 0u8..3, 0u8..9, proptest::option::of(0u8..2)), 0..4), // dels
                proptest::collection::vec((0u8..6, 0u8..3, 0u8..12, proptest::option::of(0u8..2)), 0..4), // adds
                any::<bool>(), // keep this snapshot pinned in history
                proptest::bool::weighted(0.35), // merge after this commit
            ),
            1..6,
        ),
    ) {
        let dir = scratch();
        let base: BTreeSet<CQuad> = base_raw.iter().map(|&(s, p, o, g)| quad(s, p, o, g)).collect();
        build_base(&dir, &base);
        let store = Store::open(&dir).unwrap();

        let mut model = base.clone();
        let mut history: Vec<(Arc<Snapshot>, BTreeSet<CQuad>)> =
            vec![(store.snapshot(), model.clone())];

        for (dels, adds, keep, do_merge) in &script {
            let dels: Vec<CQuad> = dels.iter().map(|&(s, p, o, g)| quad(s, p, o, g)).collect();
            let adds: Vec<CQuad> = adds.iter().map(|&(s, p, o, g)| quad(s, p, o, g)).collect();
            let snap = store.apply(&refs(&dels), &refs(&adds)).unwrap();
            for d in &dels {
                model.remove(d);
            }
            for a in &adds {
                model.insert(a.clone());
            }
            if *keep {
                history.push((snap, model.clone()));
            }
            if *do_merge {
                let merged = store.merge_with(&merge_cfg()).unwrap();
                prop_assert_eq!(dump(&merged), model.clone(), "post-merge state");
                store.gc(); // sweep retirements whenever pins allow
            }
        }

        // Every historical snapshot still equals its model, across however
        // many generations came and went while it was pinned.
        history.push((store.snapshot(), model.clone()));
        for (snap, expected) in &history {
            prop_assert_eq!(&dump(snap), expected, "epoch {} gen {}", snap.epoch(), snap.generation());
        }
        let final_epoch = store.snapshot().epoch();
        drop(history);
        drop(store);

        // Reopen: final state and epoch survive the WAL rotation(s).
        let store = Store::open(&dir).unwrap();
        let snap = store.snapshot();
        prop_assert_eq!(dump(&snap), model);
        prop_assert_eq!(snap.epoch(), final_epoch);

        std::fs::remove_dir_all(&dir).ok();
    }
}
