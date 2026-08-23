//! Storage↔engine seam tests (M3): ordering-explicit `QuadScan` batches,
//! `Store`/`Snapshot`, and the public `TermId` boundary.

use std::collections::BTreeSet;
use std::path::PathBuf;

use graphy_core::TermId;
use graphy_store::{
    BuilderConfig, OpenMode, Order, Pattern, Profile, QuadBatch, Segment, SegmentBuilder, Store,
    TermPos,
};
use graphy_turtle::{NQuadsParser, Options};

const DATA: &str = concat!(
    "<http://x/a> <http://x/p> <http://x/b> .\n",
    "<http://x/b> <http://x/p> <http://x/a> .\n",
    "<http://x/a> <http://x/q> \"plain\" .\n",
    "<http://x/a> <http://x/q> \"tagged\"@en-US <http://x/g1> .\n",
    "<http://x/b> <http://x/q> \"42\"^^<http://www.w3.org/2001/XMLSchema#integer> <http://x/g1> .\n",
    // One triple in three graphs: fan-out group of size 3.
    "<http://x/m> <http://x/p> <http://x/a> <http://x/g1> .\n",
    "<http://x/m> <http://x/p> <http://x/a> <http://x/g2> .\n",
    "<http://x/m> <http://x/p> <http://x/a> .\n",
    "<http://x/r> <http://x/reifies> <<( <http://x/a> <http://x/p> <http://x/b> )>> .\n",
);

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("graphy-store-seam-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

fn build(dir: &PathBuf, input: &str, profile: Profile) {
    let mut cfg = BuilderConfig::new(dir);
    cfg.profile = profile;
    cfg.sort_budget = 1 << 16;
    let mut b = SegmentBuilder::new(cfg).unwrap();
    let mut p = NQuadsParser::new(Options::default()).unwrap();
    p.feed(input.as_bytes()).unwrap();
    for q in p.drain() {
        b.push_quad(q.s, q.p, q.o, q.g).unwrap();
    }
    b.finish().unwrap();
}

fn iri(s: &str) -> Vec<u8> {
    let mut v = vec![b'>'];
    v.extend_from_slice(s.as_bytes());
    v
}

/// Collect a scan through batches of the given capacity.
fn collect(seg: &Segment, pat: &Pattern, order: Order, cap: usize) -> Vec<[u64; 4]> {
    let mut scan = seg.scan_order(pat, order).unwrap();
    let mut batch = QuadBatch::with_capacity(cap);
    let mut out = Vec::new();
    while scan.next_batch(&mut batch).unwrap() {
        for i in 0..batch.len() {
            out.push([batch.s[i], batch.p[i], batch.o[i], batch.g[i]]);
        }
    }
    out
}

/// A representative set of patterns over the DATA vocabulary.
fn patterns(seg: &Segment) -> Vec<Pattern> {
    let mut pats = vec![Pattern::default()];
    let mut push = |s: Option<&[u8]>, p: Option<&[u8]>, o: Option<&[u8]>, g| {
        if let Some(pat) = seg.resolve_pattern(s, p, o, g) {
            pats.push(pat);
        }
    };
    let (a, m, p, q, g1, g2) = (
        iri("http://x/a"),
        iri("http://x/m"),
        iri("http://x/p"),
        iri("http://x/q"),
        iri("http://x/g1"),
        iri("http://x/g2"),
    );
    push(Some(&a), None, None, None);
    push(None, Some(&p), None, None);
    push(None, None, Some(&a), None);
    push(Some(&m), Some(&p), None, None);
    push(None, Some(&q), Some(b"\"plain"), None);
    push(None, None, None, Some(None));
    push(None, None, None, Some(Some(&g1)));
    push(Some(&m), None, None, Some(Some(&g2)));
    pats
}

#[test]
fn scan_order_agrees_with_scan_and_is_ordered() {
    let dir = scratch("orderings");
    build(&dir, DATA, Profile::Covering);
    let seg = Segment::open(&dir).unwrap();
    let all_orders = [
        Order::Spo,
        Order::Sop,
        Order::Pos,
        Order::Pso,
        Order::Osp,
        Order::Ops,
    ];
    for pat in patterns(&seg) {
        let expected: BTreeSet<[u64; 4]> = seg.scan(&pat).unwrap().into_iter().collect();
        for order in all_orders {
            let rows = collect(&seg, &pat, order, 1024);
            let got: BTreeSet<[u64; 4]> = rows.iter().copied().collect();
            assert_eq!(got, expected, "{pat:?} via {}", order.name());
            // Output is monotone in the requested ordering's (x, y, z)
            // (ties = graph fan-out of one triple, in tg order).
            let keys: Vec<[u64; 3]> = rows
                .iter()
                .map(|q| order.to_xyz(q[0], q[1], q[2]))
                .collect();
            assert!(
                keys.windows(2).all(|w| w[0] <= w[1]),
                "{pat:?} via {} not ordered",
                order.name()
            );
        }
    }
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn tiny_batches_resume_fan_out() {
    let dir = scratch("tiny-batches");
    build(&dir, DATA, Profile::Balanced);
    let seg = Segment::open(&dir).unwrap();
    for pat in patterns(&seg) {
        for order in [Order::Spo, Order::Pos, Order::Osp] {
            let reference = collect(&seg, &pat, order, 1024);
            for cap in 1..=4 {
                let rows = collect(&seg, &pat, order, cap);
                assert_eq!(rows, reference, "{pat:?} via {} cap {cap}", order.name());
            }
        }
    }
    // The unmaterialized ordering is a clean error, not a panic.
    assert!(seg.scan_order(&Pattern::default(), Order::Pso).is_err());
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn compact_foq_virtual_orderings() {
    let dir = scratch("compact-foq");
    build(&dir, DATA, Profile::Compact);
    let seg = Segment::open(&dir).unwrap();
    assert_eq!(seg.scan_orders(), vec![Order::Spo, Order::Pso, Order::Osp]);

    for pat in patterns(&seg) {
        let expected: BTreeSet<[u64; 4]> = seg.scan(&pat).unwrap().into_iter().collect();
        for order in [Order::Spo, Order::Pso, Order::Osp] {
            let rows = collect(&seg, &pat, order, 1024);
            let got: BTreeSet<[u64; 4]> = rows.iter().copied().collect();
            assert_eq!(got, expected, "{pat:?} via {} on compact", order.name());
            let keys: Vec<[u64; 3]> = rows
                .iter()
                .map(|q| order.to_xyz(q[0], q[1], q[2]))
                .collect();
            assert!(
                keys.windows(2).all(|w| w[0] <= w[1]),
                "{pat:?} via {} on compact not ordered",
                order.name()
            );
            // Tiny batches resume FoQ runs and fan-outs identically.
            assert_eq!(collect(&seg, &pat, order, 2), rows);
        }
        // Materialized-only orderings stay unavailable.
        assert!(seg.scan_order(&pat, Order::Pos).is_err());
    }
    std::fs::remove_dir_all(&dir).unwrap();

    // O(1) bound-object counts on a triples-only compact segment.
    let dir = scratch("compact-count");
    build(
        &dir,
        "<http://x/s> <http://x/p> <http://x/o> .\n\
         <http://x/t> <http://x/p> <http://x/o> .\n\
         <http://x/s> <http://x/q> \"v\" .\n",
        Profile::Compact,
    );
    let seg = Segment::open(&dir).unwrap();
    let pat = seg
        .resolve_pattern(None, None, Some(&iri("http://x/o")), None)
        .unwrap();
    assert_eq!(seg.count(&pat).unwrap(), 2);
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn store_snapshot_round_trip() {
    let dir = scratch("store");
    build(&dir, DATA, Profile::Balanced);
    for mode in [OpenMode::Heap, OpenMode::Mmap] {
        let store = Store::open_with(&dir, mode).unwrap();
        let snap = store.snapshot();
        assert_eq!(snap.generation(), 0);
        assert_eq!(snap.epoch(), 0);
        let pat = snap
            .resolve_pattern(None, Some(&iri("http://x/p")), None, None)
            .unwrap();
        let mut scan = snap.scan(&pat, Order::Pos).unwrap();
        let mut batch = QuadBatch::new();
        let mut n = 0u64;
        while scan.next_batch(&mut batch).unwrap() {
            n += batch.len() as u64;
        }
        assert_eq!(n, snap.count(&pat).unwrap());
        assert_eq!(n, snap.segment().scan(&pat).unwrap().len() as u64);
        // scan_best picks a materialized ordering and agrees.
        let mut best = snap.scan_best(&pat).unwrap();
        let mut m = 0u64;
        while best.next_batch(&mut batch).unwrap() {
            m += batch.len() as u64;
        }
        assert_eq!(m, n, "mode {mode:?}");
        // Snapshots outlive the store handle (Arc semantics).
        drop(store);
        assert_eq!(snap.count(&pat).unwrap(), n);
    }
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn term_id_boundary_round_trips() {
    let dir = scratch("termids");
    build(&dir, DATA, Profile::Balanced);
    let store = Store::open(&dir).unwrap();
    let snap = store.snapshot();
    let seg = snap.segment();

    // Every column value of every quad round-trips through the public
    // TermId space and decodes to the same concise bytes.
    for q in seg.scan(&Pattern::default()).unwrap() {
        for (v, pos) in [
            (q[0], TermPos::Subject),
            (q[1], TermPos::Predicate),
            (q[2], TermPos::Object),
            (q[3], TermPos::Graph),
        ] {
            let id = snap.term_id(v, pos);
            assert_eq!(snap.column(id, pos), Some(v), "column({id:?}, {pos:?})");
            if pos == TermPos::Graph && v == 0 {
                assert_eq!(id, TermId::DEFAULT_GRAPH);
                continue; // the default graph has no concise form
            }
            let bytes = snap.decode(id).unwrap();
            // decode_value's graph space is section-ordinal-indexed; the
            // column value reserves 0 for the default graph.
            let dv = if pos == TermPos::Graph { v - 1 } else { v };
            assert_eq!(bytes, seg.decode_value(dv, pos).unwrap());
            // resolve() maps the bytes back to the same id.
            assert_eq!(snap.resolve(&bytes, pos), Some(id), "resolve {pos:?}");
        }
    }

    // Wrong-position lookups are None, not garbage.
    let p_id = snap
        .resolve(&iri("http://x/p"), TermPos::Predicate)
        .unwrap();
    assert_eq!(snap.column(p_id, TermPos::Subject), None);
    assert_eq!(snap.column(TermId::UNDEF, TermPos::Object), None);
    assert_eq!(snap.column(TermId::DEFAULT_GRAPH, TermPos::Graph), Some(0));
    assert!(snap.decode(TermId::UNDEF).is_err());
    assert!(snap.decode(TermId::NULL).is_err());
    // Out-of-range ordinals are rejected.
    let huge = TermId::dict(graphy_core::Section::Predicates, 1 << 40);
    assert_eq!(snap.column(huge, TermPos::Predicate), None);

    std::fs::remove_dir_all(&dir).unwrap();
}
