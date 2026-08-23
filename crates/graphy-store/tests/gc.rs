//! Epoch-GC tests (M4, doc 07 §2): [`Store::gc`] reclaims delta events no
//! live snapshot can observe — without changing what any live snapshot
//! reads — and lowers the budget pressure.

use std::collections::BTreeSet;
use std::path::PathBuf;

use graphy_store::{BuilderConfig, Order, Profile, QuadBatch, SegmentBuilder, Store, TermPos};

type CQuad = (Vec<u8>, Vec<u8>, Vec<u8>, Option<Vec<u8>>);
type QRef<'a> = (&'a [u8], &'a [u8], &'a [u8], Option<&'a [u8]>);

fn quad(name: &str) -> CQuad {
    (
        format!(">http://x/{name}").into_bytes(),
        b">http://x/p".to_vec(),
        format!("\"{name}").into_bytes(),
        None,
    )
}

fn refs(v: &[CQuad]) -> Vec<QRef<'_>> {
    v.iter()
        .map(|x| {
            (
                x.0.as_slice(),
                x.1.as_slice(),
                x.2.as_slice(),
                x.3.as_deref(),
            )
        })
        .collect()
}

fn dump(snap: &graphy_store::Snapshot) -> BTreeSet<CQuad> {
    let mut out = BTreeSet::new();
    let pat = snap.resolve_pattern(None, None, None, None).unwrap();
    let mut scan = snap.scan(&pat, Order::Spo).unwrap();
    let mut batch = QuadBatch::new();
    while scan.next_batch(&mut batch).unwrap() {
        for i in 0..batch.len() {
            out.insert((
                snap.decode_value(batch.s[i], TermPos::Subject).unwrap(),
                snap.decode_value(batch.p[i], TermPos::Predicate).unwrap(),
                snap.decode_value(batch.o[i], TermPos::Object).unwrap(),
                (batch.g[i] > 0).then(|| snap.decode_value(batch.g[i], TermPos::Graph).unwrap()),
            ));
        }
    }
    out
}

static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Base segment with quads `a` and `b`.
fn setup(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "graphy-store-gc-{}-{}-{name}",
        std::process::id(),
        SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    let mut cfg = BuilderConfig::new(&dir);
    cfg.profile = Profile::Balanced;
    let mut b = SegmentBuilder::new(cfg).unwrap();
    for q in [quad("a"), quad("b")] {
        b.push_quad(&q.0, &q.1, &q.2, q.3.as_deref()).unwrap();
    }
    b.finish().unwrap();
    dir
}

/// Superseded chains collapse; load-bearing tombstones survive; visible
/// state is untouched — including after a reopen (the WAL is not GC'd,
/// replay rebuilds the full event history).
#[test]
fn gc_collapses_superseded_chains() {
    let dir = setup("chains");
    let store = Store::open(&dir).unwrap();

    // 1: delete base quad a (load-bearing tombstone).
    store.apply(&refs(&[quad("a")]), &[]).unwrap();
    // 2–3: add overlay quad x, then delete it (dead pair).
    store.apply(&[], &refs(&[quad("x")])).unwrap();
    store.apply(&refs(&[quad("x")]), &[]).unwrap();
    // 4–5: delete base quad b, then re-add it (dead pair).
    store.apply(&refs(&[quad("b")]), &[]).unwrap();
    store.apply(&[], &refs(&[quad("b")])).unwrap();
    // 6: add overlay quad y (live content).
    let snap = store.apply(&[], &refs(&[quad("y")])).unwrap();
    assert_eq!(snap.delta_events(), 6);

    let before = dump(&snap);
    assert_eq!(before, [quad("b"), quad("y")].into_iter().collect());
    drop(snap);

    // Only the current snapshot is live: the x pair and the b delete/re-add
    // chain reclaim; a's tombstone and y's add stay.
    assert_eq!(store.gc(), 4);
    let snap = store.snapshot();
    assert_eq!(snap.delta_events(), 2);
    assert_eq!(dump(&snap), before);
    // Exact-count and membership paths agree post-GC.
    let pat = snap.resolve_pattern(None, None, None, None).unwrap();
    assert_eq!(snap.count(&pat).unwrap(), 2);
    assert!(snap
        .resolve_pattern(Some(&quad("a").0), None, None, None)
        .map(|p| snap.count(&p).unwrap())
        .unwrap_or(0)
        .eq(&0));

    // GC is idempotent, and writes continue normally afterwards.
    assert_eq!(store.gc(), 0);
    let snap = store.apply(&[], &refs(&[quad("a")])).unwrap();
    assert_eq!(
        dump(&snap),
        [quad("a"), quad("b"), quad("y")].into_iter().collect()
    );

    // Reopen: WAL replay rebuilds the full history; state and epoch match.
    let epoch = snap.epoch();
    drop(snap);
    drop(store);
    let store = Store::open(&dir).unwrap();
    assert_eq!(store.snapshot().epoch(), epoch);
    assert_eq!(
        dump(&store.snapshot()),
        [quad("a"), quad("b"), quad("y")].into_iter().collect()
    );
    std::fs::remove_dir_all(&dir).ok();
}

/// A held snapshot pins its epoch: events it can observe are not reclaimed,
/// and its view never changes. Dropping it moves the floor.
#[test]
fn gc_respects_live_snapshots() {
    let dir = setup("pins");
    let store = Store::open(&dir).unwrap();

    let s1 = store.apply(&[], &refs(&[quad("x")])).unwrap(); // epoch 1
    store.apply(&refs(&[quad("x")]), &[]).unwrap(); // epoch 2
    let view1 = dump(&s1);
    assert!(view1.contains(&quad("x")));

    // s1 (epoch 1) pins the add: nothing is reclaimable.
    assert_eq!(store.gc(), 0);
    assert_eq!(dump(&s1), view1);
    assert!(!dump(&store.snapshot()).contains(&quad("x")));

    // Release the old reader: the dead x pair reclaims.
    drop(s1);
    assert_eq!(store.gc(), 2);
    assert_eq!(store.snapshot().delta_events(), 0);
    assert!(!dump(&store.snapshot()).contains(&quad("x")));
    std::fs::remove_dir_all(&dir).ok();
}

/// GC lowers the event count the budget is enforced against.
#[test]
fn gc_relieves_budget_pressure() {
    let dir = setup("budget");
    let store = Store::open(&dir).unwrap();
    store.set_delta_budget(2, 4);

    for i in 0..2 {
        let q = [quad(&format!("t{i}"))];
        store.apply(&[], &refs(&q)).unwrap();
        store.apply(&refs(&q), &[]).unwrap();
    }
    let err = store
        .apply(&[], &refs(&[quad("blocked")]))
        .unwrap_err()
        .to_string();
    assert!(err.contains("budget exhausted"), "got: {err}");

    assert_eq!(store.gc(), 4);
    store.apply(&[], &refs(&[quad("unblocked")])).unwrap();
    std::fs::remove_dir_all(&dir).ok();
}
