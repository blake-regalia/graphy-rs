//! Group-commit and delta-budget tests (M4 inc. 3+4, doc 07 §3–§5):
//! concurrent writers serialize into a consistent epoch order, the final
//! state matches a model replay of that order, everything survives reopen,
//! and the budget signals/enforces.

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::Mutex;

use graphy_store::{BuilderConfig, Order, Profile, QuadBatch, SegmentBuilder, Store, TermPos};

type CQuad = (Vec<u8>, Vec<u8>, Vec<u8>, Option<Vec<u8>>);

fn quad(t: usize, i: usize) -> CQuad {
    (
        format!(">http://x/t{t}s{i}").into_bytes(),
        format!(">http://x/p{}", i % 3).into_bytes(),
        format!("\"value {t}/{i}").into_bytes(),
        (i % 2 == 0).then(|| format!(">http://x/g{}", i % 2).into_bytes()),
    )
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
        "graphy-store-group-{}-{}-{name}",
        std::process::id(),
        SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    let mut cfg = BuilderConfig::new(&dir);
    cfg.profile = Profile::Balanced;
    let mut b = SegmentBuilder::new(cfg).unwrap();
    b.push_quad(b">http://x/base", b">http://x/p0", b"\"base", None)
        .unwrap();
    b.finish().unwrap();
    dir
}

/// 8 threads × 12 commits each. Every commit is effective by construction
/// (unique add per commit; odd commits also delete the thread's previous
/// add), so every commit gets a unique epoch. Reconstructing the serialized
/// order by epoch and replaying it into a model must equal the store —
/// which validates group planning (the intra-group carry), WAL logging,
/// and publication ordering all at once. Then reopen and check again.
#[test]
fn concurrent_commits_serialize_correctly() {
    const THREADS: usize = 8;
    const COMMITS: usize = 12;
    let dir = setup("stress");
    let store = Store::open(&dir).unwrap();

    // (epoch, dels, adds) per successful commit, collected across threads.
    let log = Mutex::new(Vec::<(u64, Vec<CQuad>, Vec<CQuad>)>::new());
    std::thread::scope(|scope| {
        for t in 0..THREADS {
            let (store, log) = (&store, &log);
            scope.spawn(move || {
                for i in 0..COMMITS {
                    let adds = vec![quad(t, i)];
                    let dels = if i % 2 == 1 {
                        vec![quad(t, i - 1)]
                    } else {
                        vec![]
                    };
                    let snap = store.apply(&refs(&dels), &refs(&adds)).unwrap();
                    log.lock().unwrap().push((snap.epoch(), dels, adds));
                }
            });
        }
    });

    let mut log = log.into_inner().unwrap();
    log.sort_by_key(|(e, _, _)| *e);
    // Unique, contiguous epochs 1..=N.
    let epochs: Vec<u64> = log.iter().map(|(e, _, _)| *e).collect();
    assert_eq!(epochs, (1..=(THREADS * COMMITS) as u64).collect::<Vec<_>>());

    // Model replay in epoch order.
    let mut model: BTreeSet<CQuad> = BTreeSet::new();
    model.insert((
        b">http://x/base".to_vec(),
        b">http://x/p0".to_vec(),
        b"\"base".to_vec(),
        None,
    ));
    for (_, dels, adds) in &log {
        for d in dels {
            model.remove(d);
        }
        for a in adds {
            model.insert(a.clone());
        }
    }
    assert_eq!(dump(&store.snapshot()), model);

    // Reopen: the grouped WAL replays to the same state and epoch.
    let final_epoch = store.snapshot().epoch();
    drop(store);
    let store = Store::open(&dir).unwrap();
    assert_eq!(store.snapshot().epoch(), final_epoch);
    assert_eq!(dump(&store.snapshot()), model);
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn delta_budget_signals_and_enforces() {
    let dir = setup("budget");
    let store = Store::open(&dir).unwrap();
    store.set_delta_budget(2, 4);
    assert!(!store.needs_merge());

    // Two events: at the soft limit, not over it.
    let a = [quad(9, 0), quad(9, 1)];
    store.apply(&[], &refs(&a)).unwrap();
    assert!(!store.needs_merge());

    // Crossing soft sets the sticky signal.
    let b = [quad(9, 2)];
    store.apply(&[], &refs(&b)).unwrap();
    assert!(store.needs_merge());

    // Reaching hard fails the commit and leaves state unchanged.
    let c = [quad(9, 4), quad(9, 6)];
    store.apply(&[], &refs(&c)).unwrap(); // 4 events = hard
    let before = dump(&store.snapshot());
    let epoch = store.snapshot().epoch();
    let d = [quad(9, 8)];
    let err = store.apply(&[], &refs(&d)).unwrap_err().to_string();
    assert!(err.contains("budget exhausted"), "got: {err}");
    assert_eq!(dump(&store.snapshot()), before);
    assert_eq!(store.snapshot().epoch(), epoch);

    // Raising the budget unblocks writes.
    store.set_delta_budget(100, 100);
    store.apply(&[], &refs(&d)).unwrap();
    std::fs::remove_dir_all(&dir).ok();
}
