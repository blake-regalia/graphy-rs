//! M5 merge SLO benchmarks (IMPLEMENTATION_PLAN M5 exit criteria, doc 07
//! §6): fold wall time over a 1M-quad base at several delta sizes, the
//! **swap pause** distribution under concurrent writers (the §6.3(b) gate:
//! incremental shadow remap is only warranted if the pause exceeds ~10 ms),
//! and foreground reader latency while merges run (< 20% degradation bar).
//!
//! `harness = false` with a custom main: the swap-pause report is a plain
//! measured table (per-merge `MergeStats`), not a criterion statistic.
//! Record results in BENCHMARKS.md.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use criterion::{BenchmarkId, Criterion, Throughput};
use graphy_store::{
    BuilderConfig, Durability, MergeConfig, Order, Pattern, Profile, QuadBatch, SegmentBuilder,
    Store,
};

const BASE_QUADS: u64 = 1_000_000;
const N_SUBJECTS: u64 = 100_000;
const N_PREDS: u64 = 17;

type CQuad = (Vec<u8>, Vec<u8>, Vec<u8>, Option<Vec<u8>>);
type QRef<'a> = (&'a [u8], &'a [u8], &'a [u8], Option<&'a [u8]>);

fn subject(i: u64) -> Vec<u8> {
    format!(">http://ex/s{}", i % N_SUBJECTS).into_bytes()
}

fn predicate(i: u64) -> Vec<u8> {
    format!(">http://ex/p{}", i % N_PREDS).into_bytes()
}

fn object(i: u64) -> Vec<u8> {
    match i % 4 {
        0 => format!(">http://ex/s{}", (i * 7) % N_SUBJECTS).into_bytes(),
        1 => format!(">http://ex/o{}", i % 50_000).into_bytes(),
        2 => format!("\"literal value number {}", i % 25_000).into_bytes(),
        _ => format!(
            "^>http://www.w3.org/2001/XMLSchema#integer\"{}",
            i % 100_000
        )
        .into_bytes(),
    }
}

fn graph(i: u64) -> Option<Vec<u8>> {
    match i % 6 {
        0 => None,
        k => Some(format!(">http://ex/g{k}").into_bytes()),
    }
}

/// Churn quad for the read-during-merge stores: a predicate OUTSIDE the
/// measured patterns, so the busy scans' output (and zipper work for the
/// measured predicates) stays identical to idle — what remains is real
/// interference, not extra data. (An earlier version churned under the
/// measured base predicates: the busy scans returned more quads, and a
/// PACED — slower — merge let the delta grow bigger before folding,
/// cancelling the interference reduction in the measurement.)
fn churn_quad(ns: &str, i: u64) -> CQuad {
    (
        format!(">http://w/{ns}/e{i}").into_bytes(),
        b">http://w/churn-p".to_vec(),
        format!("\"payload {ns} {i}").into_bytes(),
        None,
    )
}

/// Delta-workload quad: fresh overlay subject against base predicates.
fn wquad(ns: &str, i: u64) -> CQuad {
    (
        format!(">http://w/{ns}/e{i}").into_bytes(),
        predicate(i),
        format!("\"payload {ns} {i}").into_bytes(),
        graph(i),
    )
}

fn refs(v: &[CQuad]) -> Vec<QRef<'_>> {
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

/// Pristine 1M-quad base segment, built once.
fn pristine() -> &'static PathBuf {
    static DIR: OnceLock<PathBuf> = OnceLock::new();
    DIR.get_or_init(|| {
        let dir =
            std::env::temp_dir().join(format!("graphy-merge-bench-base-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut cfg = BuilderConfig::new(&dir);
        cfg.profile = Profile::Balanced;
        let mut b = SegmentBuilder::new(cfg).unwrap();
        for i in 0..BASE_QUADS {
            b.push_quad(&subject(i), &predicate(i), &object(i), graph(i).as_deref())
                .unwrap();
        }
        b.finish().unwrap();
        dir
    })
}

fn copy_tree(src: &Path, dst: &Path) {
    std::fs::create_dir_all(dst).unwrap();
    for e in std::fs::read_dir(src).unwrap().flatten() {
        let (s, d) = (e.path(), dst.join(e.file_name()));
        if s.is_dir() {
            copy_tree(&s, &d);
        } else {
            std::fs::copy(&s, &d).unwrap();
        }
    }
}

/// Fresh working copy of the pristine base (merges mutate the store dir:
/// generations, CURRENT, WAL — a shared dir would accumulate across
/// iterations and skew the fold size).
fn work_copy(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "graphy-merge-bench-work-{tag}-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    copy_tree(pristine(), &dir);
    dir
}

/// Merge wall time: fold 1M base quads ⊎ E delta events into G+1.
fn merge_fold(c: &mut Criterion) {
    let mut group = c.benchmark_group("merge_fold");
    group.sample_size(10);
    for events in [10_000u64, 100_000] {
        group.throughput(Throughput::Elements(BASE_QUADS + events));
        group.bench_with_input(BenchmarkId::from_parameter(events), &events, |b, &e| {
            b.iter_batched(
                || {
                    let dir = work_copy("fold");
                    let store = Store::open(&dir).unwrap();
                    store.set_delta_budget(u64::MAX, u64::MAX);
                    for k in 0..e / 10_000 {
                        let quads: Vec<CQuad> =
                            (0..10_000).map(|j| wquad("grow", k * 10_000 + j)).collect();
                        store
                            .apply_with(&[], &refs(&quads), Durability::Relaxed)
                            .unwrap();
                    }
                    (dir, store)
                },
                |(dir, store)| {
                    let pace = std::env::var("GRAPHY_BENCH_PACE")
                        .ok()
                        .and_then(|v| v.parse().ok());
                    store
                        .merge_with(&MergeConfig {
                            pace_duty: pace,
                            ..MergeConfig::default()
                        })
                        .unwrap();
                    (dir, store)
                },
                criterion::BatchSize::PerIteration,
            )
        });
    }
    group.finish();
    let _ = std::fs::remove_dir_all(std::env::temp_dir().join(format!(
        "graphy-merge-bench-work-fold-{}",
        std::process::id()
    )));
}

/// Swap-pause distribution (doc 07 §6.3): repeated merges while W writers
/// commit continuously; each merge's exclusive swap section is timed by
/// `MergeStats`. The suffix (commits landed during the fold) is what the
/// swap remaps, so the table reports pause against suffix size at two
/// write rates. Plain report, not a criterion statistic.
fn swap_pause_report() {
    // Writers yield while the delta is over this cap — the bench-side
    // stand-in for the production delta budget. Unthrottled writers outrun
    // ever-slower folds and grow the delta without bound (the §6.4
    // pathological flood; it OOMs the bench box just like it would a
    // deployment without backpressure).
    const DELTA_CAP: u64 = 512_000;
    println!("\n== swap pause vs active-suffix size (1M-quad base, 4 writers, delta capped at {DELTA_CAP}) ==");
    println!(
        "{:>10} {:>14} {:>12} {:>12}",
        "rate", "suffix_events", "swap_ms", "build_ms"
    );
    for (label, batch, sleep_ms) in [("light", 25u64, 2u64), ("heavy", 200, 0)] {
        let dir = work_copy(&format!("swap-{label}"));
        let store = Arc::new(Store::open(&dir).unwrap());
        store.set_delta_budget(u64::MAX, u64::MAX);
        let stop = AtomicBool::new(false);
        let mut rows: Vec<(u64, f64, f64)> = Vec::new();
        std::thread::scope(|scope| {
            for t in 0..4u64 {
                let (store, stop) = (&store, &stop);
                scope.spawn(move || {
                    let mut i = 0u64;
                    while !stop.load(Ordering::Relaxed) {
                        if store.snapshot().delta_events() >= DELTA_CAP {
                            std::thread::sleep(Duration::from_millis(5));
                            continue;
                        }
                        let quads: Vec<CQuad> = (0..batch)
                            .map(|j| wquad(&format!("w{t}"), i * batch + j))
                            .collect();
                        store
                            .apply_with(&[], &refs(&quads), Durability::Relaxed)
                            .unwrap();
                        if sleep_ms > 0 {
                            std::thread::sleep(Duration::from_millis(sleep_ms));
                        }
                        i += 1;
                    }
                });
            }
            for _ in 0..6 {
                store.merge().unwrap();
                let s = store.last_merge_stats().unwrap();
                rows.push((
                    s.suffix_events,
                    s.swap.as_secs_f64() * 1e3,
                    s.build.as_secs_f64() * 1e3,
                ));
            }
            stop.store(true, Ordering::Relaxed);
        });
        for (suffix, swap, build) in &rows {
            println!("{label:>10} {suffix:>14} {swap:>12.2} {build:>12.1}");
        }
        drop(store);
        std::fs::remove_dir_all(&dir).ok();
    }
    println!();
}

/// Foreground reader latency while merges run continuously (< 20% bar).
fn read_during_merge(c: &mut Criterion) {
    let scan_all = |store: &Store, pats: &[Pattern]| {
        let snap = store.snapshot();
        let mut batch = QuadBatch::new();
        let mut n = 0u64;
        for pat in pats {
            let mut scan = snap.scan(pat, Order::Pos).unwrap();
            while scan.next_batch(&mut batch).unwrap() {
                n += batch.len() as u64;
            }
        }
        n
    };
    let patterns = |store: &Store| -> Vec<Pattern> {
        let snap = store.snapshot();
        (0..4u64)
            .map(|p| {
                let bytes = format!(">http://ex/p{p}").into_bytes();
                snap.resolve_pattern(None, Some(&bytes), None, None)
                    .expect("base predicate")
            })
            .collect()
    };

    let mut group = c.benchmark_group("read_during_merge");
    group.sample_size(20);

    let idle_dir = work_copy("read-idle");
    let idle = Store::open(&idle_dir).unwrap();
    let idle_pats = patterns(&idle);
    group.bench_function("bound_p_idle", |b| b.iter(|| scan_all(&idle, &idle_pats)));

    let busy_dir = work_copy("read-busy");
    let busy = Arc::new(Store::open(&busy_dir).unwrap());
    busy.set_delta_budget(u64::MAX, u64::MAX);
    let busy_pats = patterns(&busy);
    let stop = AtomicBool::new(false);
    std::thread::scope(|scope| {
        // A writer keeps the delta non-trivial so merges do real work.
        {
            let (busy, stop) = (&busy, &stop);
            scope.spawn(move || {
                let mut i = 0u64;
                while !stop.load(Ordering::Relaxed) {
                    let quads: Vec<CQuad> =
                        (0..100).map(|j| churn_quad("rm", i * 100 + j)).collect();
                    busy.apply_with(&[], &refs(&quads), Durability::Relaxed)
                        .unwrap();
                    std::thread::sleep(Duration::from_millis(1));
                    i += 1;
                }
            });
        }
        {
            let (busy, stop) = (&busy, &stop);
            scope.spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    busy.merge().unwrap();
                }
            });
        }
        group.bench_function("bound_p_during_merge", |b| {
            b.iter(|| scan_all(&busy, &busy_pats))
        });
        stop.store(true, Ordering::Relaxed);
    });

    // Paced variant (doc 07 §6.4): same disturbance shape, the whole
    // rebuild (fold scan + builder phases) duty-cycled to 50%.
    let paced_dir = work_copy("read-paced");
    let paced = Arc::new(Store::open(&paced_dir).unwrap());
    paced.set_delta_budget(u64::MAX, u64::MAX);
    let paced_pats = patterns(&paced);
    let stop = AtomicBool::new(false);
    std::thread::scope(|scope| {
        {
            let (paced, stop) = (&paced, &stop);
            scope.spawn(move || {
                let mut i = 0u64;
                while !stop.load(Ordering::Relaxed) {
                    let quads: Vec<CQuad> =
                        (0..100).map(|j| churn_quad("rp", i * 100 + j)).collect();
                    paced
                        .apply_with(&[], &refs(&quads), Durability::Relaxed)
                        .unwrap();
                    std::thread::sleep(Duration::from_millis(1));
                    i += 1;
                }
            });
        }
        {
            let (paced, stop) = (&paced, &stop);
            scope.spawn(move || {
                let cfg = MergeConfig {
                    pace_duty: Some(0.5),
                    ..MergeConfig::default()
                };
                while !stop.load(Ordering::Relaxed) {
                    paced.merge_with(&cfg).unwrap();
                }
            });
        }
        group.bench_function("bound_p_during_paced_merge", |b| {
            b.iter(|| scan_all(&paced, &paced_pats))
        });
        stop.store(true, Ordering::Relaxed);
    });
    group.finish();
    drop(idle);
    drop(paced);
    std::fs::remove_dir_all(&idle_dir).ok();
    std::fs::remove_dir_all(&busy_dir).ok();
    std::fs::remove_dir_all(&paced_dir).ok();
}

fn main() {
    swap_pause_report();
    let mut c = Criterion::default().configure_from_args();
    merge_fold(&mut c);
    read_during_merge(&mut c);
    c.final_summary();
}
