//! WAL durability tests (M4 inc. 2, doc 07 §4): reopen round-trips, epoch
//! preservation, torn-tail truncation, checksum damage, relaxed mode, and
//! no-op commits writing nothing.

use std::collections::BTreeSet;
use std::path::PathBuf;

use graphy_store::{
    BuilderConfig, Durability, Order, Profile, QuadBatch, SegmentBuilder, Store, TermPos,
};

type CQuad = (Vec<u8>, Vec<u8>, Vec<u8>, Option<Vec<u8>>);

fn iri(s: &str) -> Vec<u8> {
    format!(">http://x/{s}").into_bytes()
}

fn q(s: &str, p: &str, o: &str, g: Option<&str>) -> CQuad {
    (iri(s), iri(p), format!("\"{o}").into_bytes(), g.map(iri))
}

type QRef<'a> = (&'a [u8], &'a [u8], &'a [u8], Option<&'a [u8]>);

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

fn setup(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "graphy-store-wal-{}-{}-{name}",
        std::process::id(),
        SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    let mut cfg = BuilderConfig::new(&dir);
    cfg.profile = Profile::Balanced;
    let mut b = SegmentBuilder::new(cfg).unwrap();
    let base = q("s0", "p0", "base value", None);
    b.push_quad(&base.0, &base.1, &base.2, None).unwrap();
    b.finish().unwrap();
    dir
}

#[test]
fn commits_survive_reopen() {
    let dir = setup("reopen");
    let a1 = q("s1", "p1", "one", None);
    let a2 = q("s2", "p1", "two", Some("g1"));
    let base = q("s0", "p0", "base value", None);

    {
        let store = Store::open(&dir).unwrap();
        store.apply(&[], &refs(std::slice::from_ref(&a1))).unwrap();
        store
            .apply(
                &refs(std::slice::from_ref(&base)),
                &refs(std::slice::from_ref(&a2)),
            )
            .unwrap();
        assert_eq!(store.snapshot().epoch(), 2);
    } // drop without any explicit shutdown

    let store = Store::open(&dir).unwrap();
    let snap = store.snapshot();
    assert_eq!(snap.epoch(), 2, "epochs restored from CommitTx records");
    assert_eq!(
        dump(&snap),
        BTreeSet::from([a1.clone(), a2.clone()]),
        "delete of base quad and both adds survive"
    );

    // Writing continues after the replayed tail, and survives again.
    let a3 = q("s3", "p2", "three", None);
    store.apply(&[], &refs(std::slice::from_ref(&a3))).unwrap();
    drop(store);
    let store = Store::open(&dir).unwrap();
    assert_eq!(store.snapshot().epoch(), 3);
    assert_eq!(dump(&store.snapshot()), BTreeSet::from([a1, a2, a3]));
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn torn_tail_truncates_to_last_commit() {
    let dir = setup("torn");
    let a1 = q("s1", "p1", "kept", None);
    let a2 = q("s2", "p1", "lost", None);
    {
        let store = Store::open(&dir).unwrap();
        store.apply(&[], &refs(std::slice::from_ref(&a1))).unwrap();
        store.apply(&[], &refs(std::slice::from_ref(&a2))).unwrap();
    }
    let wal = dir.join("wal.log");
    let bytes = std::fs::read(&wal).unwrap();

    // Chop the final commit's last few bytes: the second transaction loses
    // its CommitTx and must be discarded entirely.
    std::fs::write(&wal, &bytes[..bytes.len() - 5]).unwrap();
    {
        let store = Store::open(&dir).unwrap();
        assert_eq!(store.snapshot().epoch(), 1);
        assert!(dump(&store.snapshot()).contains(&a1));
        assert!(!dump(&store.snapshot()).contains(&a2));
        // The torn tail was truncated; new commits land cleanly after it.
        let a3 = q("s3", "p1", "after damage", None);
        store.apply(&[], &refs(std::slice::from_ref(&a3))).unwrap();
        drop(store);
        let store = Store::open(&dir).unwrap();
        assert_eq!(store.snapshot().epoch(), 2);
        assert!(dump(&store.snapshot()).contains(&a3));
    }

    // Flip a byte inside the first transaction's frame: checksum fails and
    // everything from that record on is discarded.
    std::fs::write(&wal, &bytes).unwrap();
    let mut damaged = bytes.clone();
    damaged[14] ^= 0xFF; // inside the first record's payload
    std::fs::write(&wal, &damaged).unwrap();
    let store = Store::open(&dir).unwrap();
    assert_eq!(store.snapshot().epoch(), 0);
    assert!(!dump(&store.snapshot()).contains(&a1));
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn relaxed_mode_and_noop_commits() {
    let dir = setup("relaxed");
    let store = Store::open(&dir).unwrap();
    let base = q("s0", "p0", "base value", None);
    let a1 = q("s1", "p1", "relaxed", None);

    // Relaxed commits are visible and (absent a crash) durable.
    store
        .apply_with(&[], &refs(std::slice::from_ref(&a1)), Durability::Relaxed)
        .unwrap();
    assert_eq!(store.snapshot().epoch(), 1);

    // No-op commits: adding a present quad, deleting an absent one —
    // nothing hits the WAL and the epoch stays put.
    let wal_len = std::fs::metadata(dir.join("wal.log")).unwrap().len();
    let ghost = q("nope", "p9", "never", None);
    store
        .apply(&refs(&[ghost]), &refs(&[base.clone(), a1.clone()]))
        .unwrap();
    assert_eq!(store.snapshot().epoch(), 1, "no effective ops → no epoch");
    assert_eq!(
        std::fs::metadata(dir.join("wal.log")).unwrap().len(),
        wal_len,
        "no effective ops → nothing logged"
    );

    drop(store);
    let store = Store::open(&dir).unwrap();
    assert_eq!(store.snapshot().epoch(), 1);
    assert!(dump(&store.snapshot()).contains(&a1));
    std::fs::remove_dir_all(&dir).ok();
}
