//! Minor-merge tests (M5, doc 07 §6.4): lazily materializing an ordering
//! on the current generation — correctness through the snapshot seam
//! (including delta events recorded BEFORE the ordering existed, via the
//! backfilled delta index), persistence across reopen and major merges,
//! byte-identity of the added component with an offline build, and the
//! explicit profile-change merge.

use std::collections::BTreeSet;
use std::path::PathBuf;

use graphy_store::{
    resolve_segment_dir, BuilderConfig, MergeConfig, Order, Profile, QuadBatch, Segment,
    SegmentBuilder, Snapshot, Store, TermPos,
};

type CQuad = (Vec<u8>, Vec<u8>, Vec<u8>, Option<Vec<u8>>);
type QRef<'a> = (&'a [u8], &'a [u8], &'a [u8], Option<&'a [u8]>);

fn quad(s: u8, p: u8, o: u8, g: Option<u8>) -> CQuad {
    (
        format!(">http://x/s{s}").into_bytes(),
        format!(">http://x/p{p}").into_bytes(),
        match o % 3 {
            0 => format!(">http://x/s{}", o % 5).into_bytes(),
            1 => format!("\"lit{o}").into_bytes(),
            _ => format!("^>http://www.w3.org/2001/XMLSchema#integer\"{o}").into_bytes(),
        },
        g.map(|g| format!(">http://x/g{g}").into_bytes()),
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

/// Dump through a specific scan order (exercises that order's zipper).
fn dump_via(snap: &Snapshot, order: Order) -> BTreeSet<CQuad> {
    let mut out = BTreeSet::new();
    let pat = snap.resolve_pattern(None, None, None, None).unwrap();
    let mut scan = snap.scan(&pat, order).unwrap();
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

fn build(name: &str, profile: Profile, quads: &BTreeSet<CQuad>) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "graphy-store-minor-{}-{}-{name}",
        std::process::id(),
        SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    let mut cfg = BuilderConfig::new(&dir);
    cfg.profile = profile;
    cfg.sort_budget = 1 << 14;
    let mut b = SegmentBuilder::new(cfg).unwrap();
    for (s, p, o, g) in quads {
        b.push_quad(s, p, o, g.as_deref()).unwrap();
    }
    b.finish().unwrap();
    dir
}

fn base_quads() -> BTreeSet<CQuad> {
    (0..40u8)
        .map(|i| quad(i % 7, i % 3, i, (i % 4 == 0).then_some(i % 2)))
        .collect()
}

/// The added ordering serves correct scans — including delta events from
/// BEFORE the minor merge (backfilled index) and writes after it — and
/// persists across reopen.
#[test]
fn add_ordering_serves_scans_with_delta_history() {
    let base = base_quads();
    let dir = build("scans", Profile::Compact, &base);
    let store = Store::open(&dir).unwrap();

    // Writes BEFORE the ordering exists.
    let dels: Vec<CQuad> = base.iter().take(3).cloned().collect();
    let adds: Vec<CQuad> = (100..106u8).map(|i| quad(i % 9, 2, i, None)).collect();
    let pre = store.apply(&refs(&dels), &refs(&adds)).unwrap();
    let mut model: BTreeSet<CQuad> = base.clone();
    for d in &dels {
        model.remove(d);
    }
    model.extend(adds.iter().cloned());

    // Pos is neither materialized nor FoQ-virtual on compact.
    assert!(!store.snapshot().scan_orders().contains(&Order::Pos));
    let snap = store.add_ordering(Order::Pos).unwrap();
    assert_eq!(snap.generation(), 0, "minor merges keep the generation");
    assert_eq!(snap.epoch(), pre.epoch(), "no epoch consumed");
    assert!(snap.scan_orders().contains(&Order::Pos));
    assert_eq!(dump_via(&snap, Order::Pos), model, "history through Pos");
    assert_eq!(dump_via(&snap, Order::Spo), model);

    // Writes AFTER the minor merge land in the new order too.
    let more: Vec<CQuad> = (110..114u8).map(|i| quad(i % 9, 1, i, Some(1))).collect();
    let snap = store.apply(&[], &refs(&more)).unwrap();
    model.extend(more.iter().cloned());
    assert_eq!(dump_via(&snap, Order::Pos), model);

    // Deep verify + reopen persistence.
    Segment::verify(&resolve_segment_dir(&dir).unwrap()).unwrap();
    drop(snap);
    drop(pre);
    drop(store);
    let store = Store::open(&dir).unwrap();
    assert!(store.snapshot().scan_orders().contains(&Order::Pos));
    assert_eq!(dump_via(&store.snapshot(), Order::Pos), model);
    std::fs::remove_dir_all(&dir).ok();
}

/// Idempotence; survival across a same-profile major merge; reset on an
/// explicit profile change.
#[test]
fn added_ordering_survives_major_merge_until_profile_change() {
    let base = base_quads();
    let dir = build("survive", Profile::Compact, &base);
    let store = Store::open(&dir).unwrap();

    store.add_ordering(Order::Pos).unwrap();
    // Idempotent: a second call publishes nothing new.
    let again = store.add_ordering(Order::Pos).unwrap();
    assert_eq!(
        again.segment().manifest.orderings,
        vec!["spo".to_owned(), "pos".to_owned()]
    );

    // A same-profile major merge preserves the lazily added ordering.
    let adds: Vec<CQuad> = (120..124u8).map(|i| quad(i % 9, 0, i, None)).collect();
    store.apply(&[], &refs(&adds)).unwrap();
    let merged = store.merge().unwrap();
    assert_eq!(merged.generation(), 1);
    assert!(merged.scan_orders().contains(&Order::Pos));
    assert_eq!(
        merged.segment().manifest.orderings,
        vec!["spo".to_owned(), "pos".to_owned()]
    );
    let mut model = base.clone();
    model.extend(adds.iter().cloned());
    assert_eq!(dump_via(&merged, Order::Pos), model);

    // An explicit profile change resets to the new profile's orderings.
    let cfg = MergeConfig {
        profile: Some(Profile::Covering),
        sort_budget: 1 << 14,
        ..MergeConfig::default()
    };
    let covering = store.merge_with(&cfg).unwrap();
    assert_eq!(covering.segment().manifest.profile, "covering");
    assert_eq!(covering.segment().manifest.orderings.len(), 6);
    assert_eq!(dump_via(&covering, Order::Ops), model);
    std::fs::remove_dir_all(&dir).ok();
}

/// The minor-merged component is byte-identical to the same ordering from
/// a full offline build (same data, same widths ⇒ same bytes).
#[test]
fn minor_component_matches_offline_build() {
    let base = base_quads();
    let compact_dir = build("bytes-compact", Profile::Compact, &base);
    let balanced_dir = build("bytes-balanced", Profile::Balanced, &base);

    let store = Store::open(&compact_dir).unwrap();
    store.add_ordering(Order::Pos).unwrap();
    drop(store);

    let minor = std::fs::read(compact_dir.join("idx/pos.bt")).unwrap();
    let offline = std::fs::read(balanced_dir.join("idx/pos.bt")).unwrap();
    assert_eq!(minor, offline, "minor pos.bt ≠ offline pos.bt");
    std::fs::remove_dir_all(&compact_dir).ok();
    std::fs::remove_dir_all(&balanced_dir).ok();
}
