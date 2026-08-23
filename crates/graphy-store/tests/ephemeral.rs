//! Ephemeral store (docs/11 M12a): the embedded empty segment + null WAL.
//! Guards: the embedded image must stay byte-identical to what the live
//! builders produce, and delta-only stores must behave like real stores for
//! apply/scan/snapshot-isolation while rejecting merge machinery.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use graphy_store::{
    BuilderConfig, Order, QuadBatch, SegmentBuilder, Store, StoreError, TermPos, EMPTY_SEGMENT,
};

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("graphy-ephemeral-{}-{name}", std::process::id()));
    if dir.exists() {
        std::fs::remove_dir_all(&dir).unwrap();
    }
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn collect_files(root: &Path, dir: &Path, out: &mut Vec<(String, Vec<u8>)>) {
    for e in std::fs::read_dir(dir).unwrap() {
        let p = e.unwrap().path();
        if p.is_dir() {
            collect_files(root, &p, out);
        } else {
            out.push((
                p.strip_prefix(root).unwrap().to_string_lossy().into_owned(),
                std::fs::read(&p).unwrap(),
            ));
        }
    }
}

/// Format drift breaks this on purpose: regenerate with
/// `cargo run -p graphy-store --example gen_empty_segment`.
#[test]
fn embedded_empty_segment_matches_builders() {
    let dir = scratch("regen");
    let b = SegmentBuilder::new(BuilderConfig::new(&dir)).unwrap();
    b.finish().unwrap();
    let mut built = Vec::new();
    collect_files(&dir, &dir, &mut built);
    built.sort();

    let mut embedded: Vec<(String, Vec<u8>)> = EMPTY_SEGMENT
        .iter()
        .map(|(r, b)| ((*r).to_owned(), b.to_vec()))
        .collect();
    embedded.sort();

    assert_eq!(
        built.iter().map(|(r, _)| r).collect::<Vec<_>>(),
        embedded.iter().map(|(r, _)| r).collect::<Vec<_>>(),
        "embedded file set drifted from the builders"
    );
    for ((rel, want), (_, got)) in built.iter().zip(&embedded) {
        assert_eq!(want, got, "{rel}: bytes drifted — regenerate the fixture");
    }
    std::fs::remove_dir_all(&dir).ok();
}

type CQuad = (Vec<u8>, Vec<u8>, Vec<u8>, Option<Vec<u8>>);

fn iri(tag: &str, n: u8) -> Vec<u8> {
    format!(">http://x/{tag}{n}").into_bytes()
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

fn dump(snap: &graphy_store::Snapshot) -> BTreeSet<CQuad> {
    dump_in(snap, Order::Spo)
}

fn dump_in(snap: &graphy_store::Snapshot, order: Order) -> BTreeSet<CQuad> {
    let mut out = BTreeSet::new();
    let pat = snap.resolve_pattern(None, None, None, None).unwrap();
    let mut scan = snap.scan(&pat, order).unwrap();
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

#[test]
fn ephemeral_store_applies_scans_and_isolates() {
    let store = Store::ephemeral().unwrap();
    assert!(store.is_ephemeral());
    let empty = store.snapshot();
    assert!(dump(&empty).is_empty());

    // Adds across default + named graphs, typed literal, then a delete.
    let q1: CQuad = (iri("s", 1), iri("p", 1), iri("o", 1), None);
    let q2: CQuad = (
        iri("s", 1),
        iri("p", 1),
        b"^>http://www.w3.org/2001/XMLSchema#integer\"42".to_vec(),
        None,
    );
    let q3: CQuad = (iri("s", 2), iri("p", 2), iri("o", 2), Some(iri("g", 1)));
    let adds: Vec<_> = [&q1, &q2, &q3]
        .iter()
        .map(|q| {
            (
                q.0.as_slice(),
                q.1.as_slice(),
                q.2.as_slice(),
                q.3.as_deref(),
            )
        })
        .collect();
    let snap1 = store.apply(&[], &adds).unwrap();
    let want: BTreeSet<CQuad> = [q1.clone(), q2.clone(), q3.clone()].into();
    assert_eq!(dump(&snap1), want);

    // Delete one; the older snapshot still sees it (snapshot isolation).
    let dels = vec![(q1.0.as_slice(), q1.1.as_slice(), q1.2.as_slice(), None)];
    let snap2 = store.apply(&dels, &[]).unwrap();
    let want2: BTreeSet<CQuad> = [q2.clone(), q3.clone()].into();
    assert_eq!(dump(&snap2), want2);
    assert_eq!(dump(&snap1), want, "older snapshot mutated");

    // Re-add is a set-semantics add; epoch GC runs (pure in-memory).
    let adds2 = vec![(q1.0.as_slice(), q1.1.as_slice(), q1.2.as_slice(), None)];
    let snap3 = store.apply(&[], &adds2).unwrap();
    assert_eq!(dump(&snap3), want);
    drop((empty, snap1, snap2));
    store.gc();

    // Merge machinery is rejected, loudly and typed.
    assert!(matches!(
        store.merge_with(&graphy_store::MergeConfig::default()),
        Err(StoreError::Ephemeral(_))
    ));
    assert!(matches!(
        store.add_ordering(Order::Pos),
        Err(StoreError::Ephemeral(_))
    ));
}

#[test]
fn wal_capture_persists_and_restores() {
    let store = Store::ephemeral_persistent(None).unwrap();
    let q1: CQuad = (iri("s", 1), iri("p", 1), iri("o", 1), None);
    let q2: CQuad = (iri("s", 2), iri("p", 2), iri("o", 2), Some(iri("g", 1)));
    let adds: Vec<_> = [&q1, &q2]
        .iter()
        .map(|q| {
            (
                q.0.as_slice(),
                q.1.as_slice(),
                q.2.as_slice(),
                q.3.as_deref(),
            )
        })
        .collect();
    store.apply(&[], &adds).unwrap();
    let mut log = store.drain_wal_capture();
    assert!(!log.is_empty());
    // Draining is incremental: nothing new until the next commit.
    assert!(store.drain_wal_capture().is_empty());
    let dels = vec![(q1.0.as_slice(), q1.1.as_slice(), q1.2.as_slice(), None)];
    store.apply(&dels, &[]).unwrap();
    log.extend(store.drain_wal_capture());

    // Restore from the captured image: q2 only.
    let restored = Store::ephemeral_persistent(Some(&log)).unwrap();
    let want: BTreeSet<CQuad> = [q2.clone()].into();
    assert_eq!(dump(&restored.snapshot()), want);

    // A torn tail (half-appended frame) truncates instead of failing, and the
    // recovering variant reports the valid prefix so a host can truncate its
    // durable log there (appending after the tear would strand the appends).
    let mut torn = log.clone();
    torn.extend_from_slice(&[7, 0, 0, 0, 9, 9]);
    let (restored, valid) = Store::ephemeral_persistent_recovering(Some(&torn)).unwrap();
    assert_eq!(valid, log.len() as u64);
    assert_eq!(dump(&restored.snapshot()), want);
    let (_, whole) = Store::ephemeral_persistent_recovering(Some(&log)).unwrap();
    assert_eq!(whole, log.len() as u64);

    // Restored stores keep logging: epochs continue past the replayed ones.
    let adds2 = vec![(q1.0.as_slice(), q1.1.as_slice(), q1.2.as_slice(), None)];
    restored.apply(&[], &adds2).unwrap();
    let mut log2 = log.clone();
    log2.extend(restored.drain_wal_capture());
    let again = Store::ephemeral_persistent(Some(&log2)).unwrap();
    let want2: BTreeSet<CQuad> = [q1.clone(), q2.clone()].into();
    assert_eq!(dump(&again.snapshot()), want2);

    // Pack compaction: one transaction, same dataset, replays clean.
    let packed = again.pack_log().unwrap();
    assert!(packed.len() <= log2.len(), "pack should not grow the log");
    let from_pack = Store::ephemeral_persistent(Some(&packed)).unwrap();
    assert_eq!(dump(&from_pack.snapshot()), want2);

    // Post-pack appends continue from the packed epoch.
    let q3: CQuad = (iri("s", 3), iri("p", 1), iri("o", 3), None);
    from_pack
        .apply(
            &[],
            &[(q3.0.as_slice(), q3.1.as_slice(), q3.2.as_slice(), None)],
        )
        .unwrap();
    let mut log3 = packed.clone();
    log3.extend(from_pack.drain_wal_capture());
    let final_store = Store::ephemeral_persistent(Some(&log3)).unwrap();
    assert_eq!(dump(&final_store.snapshot()).len(), 3);
}

/// docs/11 §6 "scale": an ephemeral store whose base is a real segment
/// image (fetched/OPFS-loaded in the browser), with delta writes and a
/// capture log layered on top.
#[test]
fn open_image_over_a_real_segment() {
    // Build a real segment natively.
    let dir = scratch("image");
    let mut b = SegmentBuilder::new(BuilderConfig::new(&dir)).unwrap();
    for i in 0..500u32 {
        let s = format!(">http://x/s{}", i % 50).into_bytes();
        let p = format!(">http://x/p{}", i % 5).into_bytes();
        let o = format!(">http://x/o{i}").into_bytes();
        b.push_quad(&s, &p, &o, None).unwrap();
    }
    b.finish().unwrap();

    // Read its files — exactly what a browser fetch/OPFS read hands over.
    let mut files: Vec<(String, Vec<u8>)> = Vec::new();
    collect_files(&dir, &dir, &mut files);
    let refs: Vec<(&str, &[u8])> = files
        .iter()
        .map(|(r, b)| (r.as_str(), b.as_slice()))
        .collect();

    let store = Store::open_image(&refs, None).unwrap();
    assert!(store.is_ephemeral());
    assert_eq!(dump(&store.snapshot()).len(), 500);

    // Delta writes on top of the image; the log captures only the edits.
    // (A quad NOT in the base — adds of present quads are set-semantics
    // no-ops, which is itself worth having exercised here.)
    let q: CQuad = (iri("new", 1), iri("p", 1), iri("o", 1), None);
    store
        .apply(
            &[],
            &[(q.0.as_slice(), q.1.as_slice(), q.2.as_slice(), None)],
        )
        .unwrap();
    assert_eq!(dump(&store.snapshot()).len(), 501);
    let log = store.drain_wal_capture();
    assert!(
        !log.is_empty() && log.len() < 200,
        "edits only: {}",
        log.len()
    );

    // Reopen image + log: base + edits both present; merge still rejected.
    let again = Store::open_image(&refs, Some(&log)).unwrap();
    assert_eq!(dump(&again.snapshot()).len(), 501);
    assert!(matches!(
        again.merge_with(&graphy_store::MergeConfig::default()),
        Err(StoreError::Ephemeral(_))
    ));
    std::fs::remove_dir_all(&dir).ok();
}

/// The ephemeral compaction (docs/11): a churned delta folds to its net
/// state — same dataset through every scan order, history and tombstones
/// reclaimed, old snapshots frozen, and the WAL capture cycle (append-only
/// durable log + pack) coherent across the fold.
#[test]
fn compaction_folds_churn_and_preserves_state() {
    let store = Store::ephemeral_persistent(None).unwrap();

    // Churn rounds: adds across default + named graphs (including a term
    // used as BOTH subject and object — the alias-unification path), then
    // deletes and re-adds. Net state: evens of the last round + survivors.
    let quads: Vec<CQuad> = (0..60u8)
        .map(|i| {
            let g = (i % 3 == 2).then(|| iri("g", i % 2));
            if i % 5 == 0 {
                // shared term: object of one quad, subject of another
                (iri("shared", i), iri("p", i % 4), iri("shared", i / 2), g)
            } else {
                (iri("s", i), iri("p", i % 4), iri("o", i), g)
            }
        })
        .collect();
    let mut log = Vec::new();
    for round in 0..3 {
        store.apply(&[], &refs(&quads)).unwrap();
        // Rounds 0/1 delete everything (full-replace churn); round 2
        // deletes the odd half and leaves the evens live.
        let dels: Vec<CQuad> = if round < 2 {
            quads.clone()
        } else {
            quads.iter().skip(1).step_by(2).cloned().collect()
        };
        store.apply(&refs(&dels), &[]).unwrap();
        log.extend(store.drain_wal_capture());
    }
    let pinned = store.snapshot();
    let want = dump(&pinned);
    assert_eq!(want.len(), 30, "evens of the last round survive");
    let ev_before = pinned.delta_events();
    assert!(ev_before > 300, "churn history resident: {ev_before}");

    // Fold. Net state only: one add per live quad, zero tombstones (the
    // base is empty), same dataset through every order.
    let snap = store.compact_ephemeral().unwrap();
    assert_eq!(snap.delta_events(), want.len() as u64);
    assert_eq!(snap.delta_tombstones(), 0);
    for order in snap.scan_orders() {
        assert_eq!(dump_in(&snap, order), want, "order {order:?} diverged");
    }
    assert_eq!(dump(&pinned), want, "pre-compaction snapshot mutated");

    // Post-compaction commits keep working — including resolving terms
    // through the rebuilt overlay (delete one shared-term quad by bytes).
    let shared_del: Vec<CQuad> = vec![quads[0].clone()];
    store.apply(&refs(&shared_del), &[]).unwrap();
    let after: BTreeSet<CQuad> = want.iter().filter(|q| **q != quads[0]).cloned().collect();
    assert_eq!(dump(&store.snapshot()), after);
    assert_eq!(
        dump(&pinned),
        want,
        "pinned snapshot mutated by later write"
    );

    // WAL capture stayed coherent: the durable log (whole churn history +
    // the post-compaction delete) restores to the same dataset…
    log.extend(store.drain_wal_capture());
    let restored = Store::ephemeral_persistent(Some(&log)).unwrap();
    assert_eq!(dump(&restored.snapshot()), after);
    // …and pack_log over the compacted store round-trips too.
    let packed = store.pack_log().unwrap();
    let from_pack = Store::ephemeral_persistent(Some(&packed)).unwrap();
    assert_eq!(dump(&from_pack.snapshot()), after);
}

/// Compaction over an image base (docs/11 §6 "scale"): deletions of base
/// quads survive as the delta's only tombstones; overlay adds re-intern;
/// delete/re-add chains over base quads fold to plain base membership.
#[test]
fn compaction_over_an_image_base_keeps_base_tombstones() {
    let dir = scratch("compact-image");
    let mut b = SegmentBuilder::new(BuilderConfig::new(&dir)).unwrap();
    for i in 0..500u32 {
        let s = format!(">http://x/s{}", i % 50).into_bytes();
        let p = format!(">http://x/p{}", i % 5).into_bytes();
        let o = format!(">http://x/o{i}").into_bytes();
        b.push_quad(&s, &p, &o, None).unwrap();
    }
    b.finish().unwrap();
    let mut files: Vec<(String, Vec<u8>)> = Vec::new();
    collect_files(&dir, &dir, &mut files);
    let file_refs: Vec<(&str, &[u8])> = files
        .iter()
        .map(|(r, b)| (r.as_str(), b.as_slice()))
        .collect();
    let store = Store::open_image(&file_refs, None).unwrap();

    let base_quad = |i: u32| -> CQuad {
        (
            format!(">http://x/s{}", i % 50).into_bytes(),
            format!(">http://x/p{}", i % 5).into_bytes(),
            format!(">http://x/o{i}").into_bytes(),
            None,
        )
    };
    // 5 permanent base deletes; a 6th base quad deleted and re-added
    // (folds away); 3 overlay adds; one overlay add+delete (folds away);
    // plus overlay churn (delete + re-add).
    let dels: Vec<CQuad> = (0..5).map(base_quad).collect();
    store.apply(&refs(&dels), &[]).unwrap();
    let chain: Vec<CQuad> = vec![base_quad(5)];
    store.apply(&refs(&chain), &[]).unwrap();
    store.apply(&[], &refs(&chain)).unwrap();
    let adds: Vec<CQuad> = (0..3)
        .map(|i| (iri("new", i), iri("p", i), iri("o", i), None))
        .collect();
    store.apply(&[], &refs(&adds)).unwrap();
    let gone: Vec<CQuad> = vec![(iri("gone", 1), iri("p", 1), iri("o", 1), None)];
    store.apply(&[], &refs(&gone)).unwrap();
    store.apply(&refs(&gone), &[]).unwrap();
    store.apply(&refs(&adds[..1]), &[]).unwrap();
    store.apply(&[], &refs(&adds[..1])).unwrap();

    let want = dump(&store.snapshot());
    assert_eq!(want.len(), 500 - 5 + 3);
    let snap = store.compact_ephemeral().unwrap();
    // Exactly the 5 permanent base deletions survive as tombstones and
    // the 3 net-new overlay quads as adds.
    assert_eq!(snap.delta_tombstones(), 5);
    assert_eq!(snap.delta_events(), 8);
    for order in snap.scan_orders() {
        assert_eq!(dump_in(&snap, order), want, "order {order:?} diverged");
    }
    let pat = snap.resolve_pattern(None, None, None, None).unwrap();
    assert_eq!(snap.count(&pat).unwrap(), 498);

    // Writes continue on the folded delta: a repeat delete of an
    // already-tombstoned base quad is a no-op; re-adding it lands over
    // the tombstone.
    store.apply(&refs(&dels[..1]), &[]).unwrap();
    store.apply(&[], &refs(&[base_quad(0)])).unwrap();
    assert_eq!(dump(&store.snapshot()).len(), 499);
    std::fs::remove_dir_all(&dir).ok();
}

/// Measurement companion to the identity gates (docs/11; run with
/// `--release -- --ignored` and read the printed numbers): a delta
/// holding ~30× history vs live data, full-scan timed before and after
/// the fold. The assertion is deliberately loose (folded reads must not
/// be slower); the point of the run is the printed ratio.
#[test]
#[ignore = "measurement — run with --release -- --ignored"]
fn compaction_restores_read_performance() {
    let store = Store::ephemeral().unwrap();
    // Ten full-replace rounds of EVOLVING content (the fabric-host shape:
    // every repo PUT rewrites whole graphs with new terms): each round's
    // 50k quads are distinct, so dead keys and dead overlay terms
    // accumulate alongside the event history.
    let round_quads = |r: u32| -> Vec<CQuad> {
        (0..50_000u32)
            .map(|i| {
                let n = r * 50_000 + i;
                (
                    format!(">http://x/s{n}").into_bytes(),
                    format!(">http://x/p{}", i % 8).into_bytes(),
                    format!(">http://x/o{n}").into_bytes(),
                    None,
                )
            })
            .collect()
    };
    for r in 0..10 {
        let quads = round_quads(r);
        store.apply(&[], &refs(&quads)).unwrap();
        if r < 9 {
            store.apply(&refs(&quads), &[]).unwrap();
        }
    }
    // Two read shapes: one unbound full scan, and a join-like probe storm
    // (bound-subject scans across every subject — the shape whose per-probe
    // range collection pays for resident history over and over).
    let scan_all = |snap: &graphy_store::Snapshot| -> (u64, std::time::Duration) {
        let t = std::time::Instant::now();
        let pat = snap.resolve_pattern(None, None, None, None).unwrap();
        let mut scan = snap.scan(&pat, Order::Spo).unwrap();
        let mut batch = QuadBatch::new();
        let mut n = 0u64;
        while scan.next_batch(&mut batch).unwrap() {
            n += batch.len() as u64;
        }
        (n, t.elapsed())
    };
    let preds: Vec<Vec<u8>> = (0..8u32)
        .map(|i| format!(">http://x/p{i}").into_bytes())
        .collect();
    let probe_all = |snap: &graphy_store::Snapshot| -> (u64, std::time::Duration) {
        let t = std::time::Instant::now();
        let mut n = 0u64;
        let mut batch = QuadBatch::new();
        for i in 0..200 {
            let pat = snap
                .resolve_pattern(None, Some(&preds[i % preds.len()]), None, None)
                .unwrap();
            let mut scan = snap.scan_best(&pat).unwrap();
            while scan.next_batch(&mut batch).unwrap() {
                n += batch.len() as u64;
            }
        }
        (n, t.elapsed())
    };
    let churned = store.snapshot();
    let (n_before, scan_before) = scan_all(&churned);
    let (p_before, probe_before) = probe_all(&churned);
    let t0 = std::time::Instant::now();
    let snap = store.compact_ephemeral().unwrap();
    let t_fold = t0.elapsed();
    let (n_after, scan_after) = scan_all(&snap);
    let (p_after, probe_after) = probe_all(&snap);
    assert_eq!(n_before, n_after);
    assert_eq!(p_before, p_after);
    println!(
        "events {} -> {}; full scan {:?} -> {:?}; 200 p-bound driving scans {:?} -> {:?}; fold {:?}",
        churned.delta_events(),
        snap.delta_events(),
        scan_before,
        scan_after,
        probe_before,
        probe_after,
        t_fold,
    );
    assert!(
        probe_after <= probe_before,
        "folded probes slower: {probe_after:?} > {probe_before:?}"
    );
}

/// The size-triggered policy: due only with resident tombstones AND
/// minimum growth AND doubling since the last fold; directory-backed
/// stores are never due and reject the explicit call.
#[test]
fn compaction_trigger_and_directory_rejection() {
    let store = Store::ephemeral().unwrap();
    store.set_ephemeral_compaction_min(4);
    assert!(!store.ephemeral_compaction_due());

    let quads: Vec<CQuad> = (0..6)
        .map(|i| (iri("s", i), iri("p", 0), iri("o", i), None))
        .collect();
    store.apply(&[], &refs(&quads)).unwrap();
    // Growth alone is not due: every event is a live add; folding would
    // rebuild 6 events into 6 events.
    assert!(!store.ephemeral_compaction_due());
    assert!(store.compact_ephemeral_if_due().unwrap().is_none());

    let dels: Vec<CQuad> = quads[..2].to_vec();
    store.apply(&refs(&dels), &[]).unwrap();
    // 8 events, 2 tombstones, floor 0: due; folding lands at 4 live quads.
    assert!(store.ephemeral_compaction_due());
    let snap = store.compact_ephemeral_if_due().unwrap().expect("due");
    assert_eq!(snap.delta_events(), 4);
    assert!(!store.ephemeral_compaction_due(), "fresh fold re-armed");

    // Churn below the doubling target stays not-due; reaching it fires.
    let churn: Vec<CQuad> = quads[2..4].to_vec();
    store.apply(&refs(&churn), &[]).unwrap();
    assert!(!store.ephemeral_compaction_due(), "6 events < 2×4 floor");
    store.apply(&[], &refs(&churn)).unwrap();
    assert!(
        store.ephemeral_compaction_due(),
        "8 events = growth + doubling"
    );
    let snap = store.compact_ephemeral_if_due().unwrap().expect("due");
    assert_eq!(snap.delta_events(), 4);

    // Directory-backed stores: never due, if_due is a no-op, the explicit
    // call is a typed misuse.
    let dir = scratch("compact-dir");
    let b = SegmentBuilder::new(BuilderConfig::new(&dir)).unwrap();
    b.finish().unwrap();
    let disk = Store::open(&dir).unwrap();
    assert!(!disk.ephemeral_compaction_due());
    assert!(disk.compact_ephemeral_if_due().unwrap().is_none());
    assert!(disk.compact_ephemeral().is_err());
    std::fs::remove_dir_all(&dir).ok();
}

/// The near-hard rider must not require a full minimum-growth step: when a
/// fold's live floor is already close to hard, even one delete can create
/// reclaimable history at the limit and would otherwise strand the store.
#[test]
fn compaction_near_hard_bypasses_minimum_growth() {
    let store = Store::ephemeral().unwrap();
    store.set_delta_budget(5, 6);
    store.set_ephemeral_compaction_min(4);
    let quads: Vec<CQuad> = (0..5)
        .map(|i| (iri("hard-s", i), iri("hard-p", 0), iri("hard-o", i), None))
        .collect();
    store.apply(&[], &refs(&quads)).unwrap();
    store.compact_ephemeral().unwrap(); // floor = 5, just below hard = 6

    store.apply(&refs(&quads[..1]), &[]).unwrap();
    assert_eq!(store.snapshot().delta_events(), 6);
    assert!(
        store.ephemeral_compaction_due(),
        "reclaimable churn at hard must not wait for floor + min"
    );
    let snap = store.compact_ephemeral_if_due().unwrap().expect("due");
    assert_eq!(snap.delta_events(), 4);

    // Capacity was actually restored, not merely signalled.
    let extra = vec![(iri("hard-s", 9), iri("hard-p", 0), iri("hard-o", 9), None)];
    store.apply(&[], &refs(&extra)).unwrap();
}
