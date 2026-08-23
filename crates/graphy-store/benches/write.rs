//! M4 write-path exit-criteria benchmarks (IMPLEMENTATION_PLAN M4):
//! sustained commit throughput with fsync group commit (bar: ≥ 100k quads/s),
//! reader latency under write load (bar: < 5% degradation), and the
//! delta-structure baseline curves (doc 07 §3 defers the structure choice to
//! this benchmark: RwLock + BTreeMap event maps vs. anything fancier).
//!
//! Base corpus: the M2 synthetic shape at 100k quads. Write workloads add
//! overlay subjects/objects against base predicates (the realistic update
//! mix: new entities, known schema). Record results in BENCHMARKS.md.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::OnceLock;
use std::time::Instant;

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use graphy_store::{
    BuilderConfig, Durability, Order, Pattern, Profile, QuadBatch, SegmentBuilder, Store,
};

const BASE_QUADS: u64 = 100_000;
const N_SUBJECTS: u64 = 10_000;
const N_PREDS: u64 = 17;

type CQuad = (Vec<u8>, Vec<u8>, Vec<u8>, Option<Vec<u8>>);
type QRef<'a> = (&'a [u8], &'a [u8], &'a [u8], Option<&'a [u8]>);

fn base_subject(i: u64) -> Vec<u8> {
    format!(">http://ex/s{}", i % N_SUBJECTS).into_bytes()
}

fn predicate(i: u64) -> Vec<u8> {
    format!(">http://ex/p{}", i % N_PREDS).into_bytes()
}

fn base_object(i: u64) -> Vec<u8> {
    match i % 4 {
        0 => format!(">http://ex/s{}", (i * 7) % N_SUBJECTS).into_bytes(),
        1 => format!(">http://ex/o{}", i % 5_000).into_bytes(),
        2 => format!("\"literal value number {}", i % 2_500).into_bytes(),
        _ => format!("^>http://www.w3.org/2001/XMLSchema#integer\"{}", i % 10_000).into_bytes(),
    }
}

fn graph(i: u64) -> Option<Vec<u8>> {
    match i % 6 {
        0 => None,
        k => Some(format!(">http://ex/g{k}").into_bytes()),
    }
}

/// Write-workload quad `i`: fresh overlay subject, base predicate, mixed
/// fresh/base object. `ns` namespaces streams so workloads never collide.
fn wquad(ns: &str, i: u64) -> CQuad {
    (
        format!(">http://w/{ns}/e{i}").into_bytes(),
        predicate(i),
        match i % 3 {
            0 => base_object(i),
            1 => format!("\"payload {ns} {i}").into_bytes(),
            _ => format!("^>http://www.w3.org/2001/XMLSchema#integer\"{i}").into_bytes(),
        },
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

fn build_base(name: &str) -> PathBuf {
    let dir =
        std::env::temp_dir().join(format!("graphy-write-bench-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let mut cfg = BuilderConfig::new(&dir);
    cfg.profile = Profile::Balanced;
    let mut b = SegmentBuilder::new(cfg).unwrap();
    for i in 0..BASE_QUADS {
        b.push_quad(
            &base_subject(i),
            &predicate(i),
            &base_object(i),
            graph(i).as_deref(),
        )
        .unwrap();
    }
    b.finish().unwrap();
    dir
}

fn commit_dir() -> &'static PathBuf {
    static DIR: OnceLock<PathBuf> = OnceLock::new();
    DIR.get_or_init(|| build_base("commit"))
}

/// Fresh store on `dir`: drop any WAL from a previous iteration so open
/// replays nothing and the delta starts empty.
fn fresh_store(dir: &std::path::Path) -> Store {
    let _ = std::fs::remove_file(dir.join("wal.log"));
    let store = Store::open(dir).unwrap();
    store.set_delta_budget(u64::MAX, u64::MAX);
    store
}

/// Single-writer commit throughput: batch size × durability. The Strict
/// rows are the M4 exit bar (fsync per commit — group commit degenerates to
/// solo leaders here); Relaxed isolates the non-fsync pipeline cost.
fn commit_throughput(c: &mut Criterion) {
    let dir = commit_dir();
    let mut group = c.benchmark_group("commit");
    group.sample_size(10);
    for (label, batch, commits, durability) in [
        ("strict_1q", 1u64, 50u64, Durability::Strict),
        ("relaxed_1q", 1, 200, Durability::Relaxed),
        ("strict_1000q", 1_000, 20, Durability::Strict),
        ("relaxed_1000q", 1_000, 20, Durability::Relaxed),
    ] {
        // Pre-generate every commit's quads (generation is untimed).
        let payload: Vec<Vec<CQuad>> = (0..commits)
            .map(|k| (0..batch).map(|j| wquad(label, k * batch + j)).collect())
            .collect();
        group.throughput(Throughput::Elements(batch * commits));
        group.bench_function(label, |b| {
            b.iter_batched(
                || fresh_store(dir),
                |store| {
                    for quads in &payload {
                        store.apply_with(&[], &refs(quads), durability).unwrap();
                    }
                    store
                },
                criterion::BatchSize::PerIteration,
            )
        });
    }
    group.finish();
}

/// Concurrent writers, Strict durability: T threads each commit 200-quad
/// batches until the shared 40k-quad workload is done. Group commit should
/// amortize fsyncs as T grows.
fn group_commit_scaling(c: &mut Criterion) {
    let dir = commit_dir();
    const TOTAL: u64 = 40_000;
    const BATCH: u64 = 200;
    let mut group = c.benchmark_group("group_commit");
    group.sample_size(10);
    group.throughput(Throughput::Elements(TOTAL));
    for threads in [1u64, 2, 4, 8] {
        let per_thread = TOTAL / threads / BATCH; // commits per thread
        let payload: Vec<Vec<Vec<CQuad>>> = (0..threads)
            .map(|t| {
                (0..per_thread)
                    .map(|k| {
                        (0..BATCH)
                            .map(|j| wquad(&format!("g{t}"), k * BATCH + j))
                            .collect()
                    })
                    .collect()
            })
            .collect();
        group.bench_with_input(BenchmarkId::from_parameter(threads), &threads, |b, _| {
            b.iter_batched(
                || fresh_store(dir),
                |store| {
                    std::thread::scope(|scope| {
                        for commits in &payload {
                            let store = &store;
                            scope.spawn(move || {
                                for quads in commits {
                                    store
                                        .apply_with(&[], &refs(quads), Durability::Strict)
                                        .unwrap();
                                }
                            });
                        }
                    });
                    store
                },
                criterion::BatchSize::PerIteration,
            )
        });
    }
    group.finish();
}

/// Reader latency with and without a concurrent writer (M4 exit bar: < 5%
/// degradation). The reader work is the M3 bound-p POS scan through the
/// snapshot seam; the busy variant runs against a store whose writer
/// continuously churns 500-quad add/delete rounds (Relaxed — maximum delta
/// pressure) with periodic epoch GC holding the delta near steady state.
fn read_under_writes(c: &mut Criterion) {
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

    let mut group = c.benchmark_group("read_under_writes");
    group.sample_size(20);

    // Baseline: no writer, empty delta.
    let idle_dir = build_base("read-idle");
    let idle = fresh_store(&idle_dir);
    let idle_pats = patterns(&idle);
    group.bench_function("bound_p_idle", |b| b.iter(|| scan_all(&idle, &idle_pats)));

    // Busy: continuous churn on a separate store.
    let busy_dir = build_base("read-busy");
    let busy = fresh_store(&busy_dir);
    let busy_pats = patterns(&busy);
    let stop = AtomicBool::new(false);
    std::thread::scope(|scope| {
        scope.spawn(|| {
            let mut round = 0u64;
            let mut prev: Vec<CQuad> = Vec::new();
            while !stop.load(Ordering::Relaxed) {
                let quads: Vec<CQuad> = (0..500).map(|j| wquad("churn", round * 500 + j)).collect();
                busy.apply_with(&refs(&prev), &refs(&quads), Durability::Relaxed)
                    .unwrap();
                if round % 8 == 7 {
                    busy.gc();
                }
                prev = quads;
                round += 1;
            }
        });
        group.bench_function("bound_p_busy", |b| b.iter(|| scan_all(&busy, &busy_pats)));
        stop.store(true, Ordering::Relaxed);
    });
    group.finish();
    drop(idle);
    drop(busy);
    std::fs::remove_dir_all(&idle_dir).ok();
    std::fs::remove_dir_all(&busy_dir).ok();
}

/// Delta-structure baseline curves (doc 07 §3): commit and scan cost as the
/// resident delta grows. `churn_1k` = delete+re-add 1000 quads (Relaxed, two
/// commits) against a delta of E events; `scan_bound_p` = the busy-read scan
/// against the same deltas. These curves decide RwLock+BTreeMap vs. a
/// fancier structure.
fn delta_scale(c: &mut Criterion) {
    let mut group = c.benchmark_group("delta_scale");
    group.sample_size(10);
    for events in [10_000u64, 100_000, 1_000_000] {
        let dir = build_base(&format!("scale-{events}"));
        let store = fresh_store(&dir);
        for k in 0..events / 10_000 {
            let quads: Vec<CQuad> = (0..10_000).map(|j| wquad("grow", k * 10_000 + j)).collect();
            store
                .apply_with(&[], &refs(&quads), Durability::Relaxed)
                .unwrap();
        }
        assert_eq!(store.snapshot().delta_events(), events);

        let churn: Vec<CQuad> = (0..1_000).map(|j| wquad("grow", j)).collect();
        let churn_refs = refs(&churn);
        group.throughput(Throughput::Elements(2_000));
        group.bench_with_input(BenchmarkId::new("churn_1k", events), &events, |b, _| {
            b.iter(|| {
                store
                    .apply_with(&churn_refs, &[], Durability::Relaxed)
                    .unwrap();
                store
                    .apply_with(&[], &churn_refs, Durability::Relaxed)
                    .unwrap();
            })
        });

        let snap = store.snapshot();
        let p_bytes = format!(">http://ex/p{}", 3).into_bytes();
        let pat = snap
            .resolve_pattern(None, Some(&p_bytes), None, None)
            .expect("base predicate");
        let n = snap.count(&pat).unwrap();
        group.throughput(Throughput::Elements(n));
        group.bench_with_input(BenchmarkId::new("scan_bound_p", events), &events, |b, _| {
            let mut batch = QuadBatch::new();
            b.iter_custom(|iters| {
                let start = Instant::now();
                for _ in 0..iters {
                    let snap = store.snapshot();
                    let mut scan = snap.scan(&pat, Order::Pos).unwrap();
                    let mut got = 0u64;
                    while scan.next_batch(&mut batch).unwrap() {
                        got += batch.len() as u64;
                    }
                    assert!(got >= n);
                }
                start.elapsed()
            })
        });
        std::fs::remove_dir_all(&dir).ok();
    }
    group.finish();
}

criterion_group!(
    benches,
    commit_throughput,
    group_commit_scaling,
    read_under_writes,
    delta_scale
);
criterion_main!(benches);
