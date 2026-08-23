//! Merge-scheduler tests (M5, doc 07 §6.4): triggers fire (soft budget,
//! hard pressure, tombstone ratio, explicit), hard-budget pressure becomes
//! wait-for-merge backpressure while a merger is attached (and reverts to
//! fail-fast without one / after detach), and shutdown is clean.

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use graphy_store::{
    BuilderConfig, MergeScheduler, Order, Profile, QuadBatch, SchedulerConfig, SegmentBuilder,
    Store, TermPos,
};

type CQuad = (Vec<u8>, Vec<u8>, Vec<u8>, Option<Vec<u8>>);
type QRef<'a> = (&'a [u8], &'a [u8], &'a [u8], Option<&'a [u8]>);

fn quad(name: &str) -> CQuad {
    (
        format!(">http://sch/{name}").into_bytes(),
        b">http://sch/p".to_vec(),
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

/// Base segment with `base0..base{n}`.
fn setup(name: &str, n: usize) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "graphy-store-sched-{}-{}-{name}",
        std::process::id(),
        SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    let mut cfg = BuilderConfig::new(&dir);
    cfg.profile = Profile::Balanced;
    let mut b = SegmentBuilder::new(cfg).unwrap();
    for i in 0..n {
        let q = quad(&format!("base{i}"));
        b.push_quad(&q.0, &q.1, &q.2, q.3.as_deref()).unwrap();
    }
    b.finish().unwrap();
    dir
}

/// Poll `cond` until true or the deadline; panics with `what` on timeout.
fn wait_for(what: &str, mut cond: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(20);
    while !cond() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// Config with every automatic trigger off (only hard pressure — which is
/// never configurable away — and explicit kicks remain).
fn manual_config() -> SchedulerConfig {
    SchedulerConfig {
        on_soft_budget: false,
        tombstone_ratio: None,
        ..SchedulerConfig::default()
    }
}

/// Crossing the soft budget merges automatically and clears the signal.
#[test]
fn soft_budget_triggers_merge() {
    let dir = setup("soft", 2);
    let store = Arc::new(Store::open(&dir).unwrap());
    store.set_delta_budget(4, 1_000);
    let sched = MergeScheduler::spawn(
        Arc::clone(&store),
        SchedulerConfig {
            interval: Duration::from_millis(10),
            ..SchedulerConfig::default()
        },
    );

    let adds: Vec<CQuad> = (0..6).map(|i| quad(&format!("s{i}"))).collect();
    store.apply(&[], &refs(&adds)).unwrap();
    assert!(store.needs_merge());

    wait_for("soft-budget merge", || {
        store.snapshot().generation() >= 1 && !store.needs_merge()
    });
    // The counter increments after merge_with returns — poll, don't assert.
    wait_for("merge counter", || sched.merges_completed() == 1);
    assert_eq!(sched.last_error(), None);
    let snap = store.snapshot();
    assert_eq!(snap.delta_events(), 0);
    assert_eq!(dump(&snap).len(), 8);
    drop(sched);
    std::fs::remove_dir_all(&dir).ok();
}

/// At the hard budget a writer blocks on the gate and completes once the
/// scheduler's merge folds the delta away — no commit failure.
#[test]
fn hard_pressure_backpressures_instead_of_failing() {
    let dir = setup("hard", 2);
    let store = Arc::new(Store::open(&dir).unwrap());
    store.set_delta_budget(1_000, 4); // soft never fires; hard = 4
    let sched = MergeScheduler::spawn(
        Arc::clone(&store),
        SchedulerConfig {
            // Only the always-on hard-pressure trigger can fire here; the
            // gated writer resolves when its evaluation runs.
            interval: Duration::from_millis(50),
            ..manual_config()
        },
    );

    for i in 0..4 {
        store.apply(&[], &refs(&[quad(&format!("h{i}"))])).unwrap();
    }
    assert_eq!(store.snapshot().delta_events(), 4);

    // This commit is over the hard budget: it must WAIT, then succeed.
    let blocked = quad("blocked-then-fine");
    let snap = store
        .apply(&[], &refs(std::slice::from_ref(&blocked)))
        .unwrap();
    assert!(
        snap.generation() >= 1,
        "commit resolved without a merge having run"
    );
    assert!(dump(&snap).contains(&blocked));
    wait_for("merge counter", || sched.merges_completed() >= 1);
    drop(sched);
    std::fs::remove_dir_all(&dir).ok();
}

/// Without a merger, the hard budget still fails fast (M4 behavior).
#[test]
fn hard_pressure_fails_fast_without_merger() {
    let dir = setup("failfast", 2);
    let store = Store::open(&dir).unwrap();
    store.set_delta_budget(1_000, 4);
    for i in 0..4 {
        store.apply(&[], &refs(&[quad(&format!("f{i}"))])).unwrap();
    }
    let err = store
        .apply(&[], &refs(&[quad("nope")]))
        .unwrap_err()
        .to_string();
    assert!(err.contains("budget exhausted"), "got: {err}");
    std::fs::remove_dir_all(&dir).ok();
}

/// A merger that never fires (all triggers off, huge interval) bounds the
/// gate wait: the writer fails with the backpressure-timeout error.
#[test]
fn backpressure_times_out_against_an_idle_merger() {
    let dir = setup("timeout", 2);
    let store = Arc::new(Store::open(&dir).unwrap());
    store.set_delta_budget(1_000, 4);
    store.set_backpressure_timeout(Duration::from_millis(100));
    let sched = MergeScheduler::spawn(
        Arc::clone(&store),
        SchedulerConfig {
            interval: Duration::from_secs(3_600),
            ..manual_config()
        },
    );

    for i in 0..4 {
        store.apply(&[], &refs(&[quad(&format!("t{i}"))])).unwrap();
    }
    let err = store
        .apply(&[], &refs(&[quad("nope")]))
        .unwrap_err()
        .to_string();
    assert!(err.contains("backpressure timed out"), "got: {err}");
    drop(sched);
    std::fs::remove_dir_all(&dir).ok();
}

/// Deletes alone (under both budgets) trip the tombstone-ratio guard.
#[test]
fn tombstone_ratio_triggers_merge() {
    let dir = setup("tomb", 10);
    let store = Arc::new(Store::open(&dir).unwrap());
    // Budgets far away: only the ratio can fire.
    store.set_delta_budget(1_000, 2_000);
    let sched = MergeScheduler::spawn(
        Arc::clone(&store),
        SchedulerConfig {
            interval: Duration::from_millis(10),
            on_soft_budget: false,
            tombstone_ratio: Some(0.25),
            ..SchedulerConfig::default()
        },
    );

    // 2 tombstones / 10 base quads: under the 25% ratio — no merge.
    let dels: Vec<CQuad> = (0..2).map(|i| quad(&format!("base{i}"))).collect();
    store.apply(&refs(&dels), &[]).unwrap();
    std::thread::sleep(Duration::from_millis(100));
    assert_eq!(store.snapshot().generation(), 0, "merged below the ratio");

    // A third delete crosses 25%.
    store.apply(&refs(&[quad("base2")]), &[]).unwrap();
    wait_for("tombstone-ratio merge", || {
        store.snapshot().generation() >= 1
    });
    let snap = store.snapshot();
    assert_eq!(snap.delta_tombstones(), 0);
    assert_eq!(dump(&snap).len(), 7);
    drop(sched);
    std::fs::remove_dir_all(&dir).ok();
}

/// `request_merge` folds even when no automatic trigger would fire.
#[test]
fn explicit_request_merges() {
    let dir = setup("explicit", 2);
    let store = Arc::new(Store::open(&dir).unwrap());
    let sched = MergeScheduler::spawn(
        Arc::clone(&store),
        SchedulerConfig {
            interval: Duration::from_secs(3_600),
            ..manual_config()
        },
    );

    store.apply(&[], &refs(&[quad("x")])).unwrap();
    assert_eq!(store.snapshot().generation(), 0);
    sched.request_merge();
    wait_for("explicit merge", || store.snapshot().generation() >= 1);
    wait_for("merge counter", || sched.merges_completed() == 1);
    drop(sched);
    std::fs::remove_dir_all(&dir).ok();
}

/// Dropping the scheduler detaches the merger: a writer blocked on the
/// gate falls back to the fail-fast error instead of hanging.
#[test]
fn shutdown_unblocks_gated_writers() {
    let dir = setup("shutdown", 2);
    let store = Arc::new(Store::open(&dir).unwrap());
    store.set_delta_budget(1_000, 4);
    let sched = MergeScheduler::spawn(
        Arc::clone(&store),
        SchedulerConfig {
            interval: Duration::from_secs(3_600),
            ..manual_config()
        },
    );

    for i in 0..4 {
        store.apply(&[], &refs(&[quad(&format!("d{i}"))])).unwrap();
    }
    let writer = {
        let store = Arc::clone(&store);
        std::thread::spawn(move || {
            store
                .apply(&[], &refs(&[quad("gated")]))
                .unwrap_err()
                .to_string()
        })
    };
    // Let the writer reach the gate, then detach the merger.
    std::thread::sleep(Duration::from_millis(100));
    drop(sched);
    let err = writer.join().unwrap();
    assert!(err.contains("budget exhausted"), "got: {err}");
    std::fs::remove_dir_all(&dir).ok();
}
