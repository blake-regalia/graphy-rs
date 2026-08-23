//! M3 read-path exit-criteria benchmarks (IMPLEMENTATION_PLAN M3):
//! warm point lookups (< 100 µs incl. decode), scan throughput
//! (≥ 50 M quads/s/core on packed-aligned orderings), and reader scaling
//! (linear to core count on a read-only workload).
//!
//! Corpus: the M2 synthetic shape (100k subjects, 17 predicates, mixed
//! IRI/plain/inline-integer objects, 5 named graphs + default), 1M quads,
//! built once per bench run. Record results in BENCHMARKS.md.

use std::path::PathBuf;
use std::sync::OnceLock;
use std::time::Instant;

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use graphy_store::{
    BuilderConfig, Order, Pattern, Profile, QuadBatch, Segment, SegmentBuilder, TermPos,
};

const N_QUADS: u64 = 1_000_000;
const N_SUBJECTS: u64 = 100_000;
const N_PREDS: u64 = 17;

fn subject(i: u64) -> Vec<u8> {
    format!(">http://ex/s{}", i % N_SUBJECTS).into_bytes()
}

fn predicate(i: u64) -> Vec<u8> {
    format!(">http://ex/p{}", i % N_PREDS).into_bytes()
}

fn object(i: u64) -> Vec<u8> {
    match i % 4 {
        0 => format!(">http://ex/s{}", (i * 7) % N_SUBJECTS).into_bytes(), // shared
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

fn build(name: &str, with_graphs: bool) -> (PathBuf, Segment) {
    let dir = std::env::temp_dir().join(format!("graphy-read-bench-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let mut cfg = BuilderConfig::new(&dir);
    cfg.profile = Profile::Balanced;
    let mut b = SegmentBuilder::new(cfg).unwrap();
    for i in 0..N_QUADS {
        let g = if with_graphs { graph(i) } else { None };
        b.push_quad(&subject(i), &predicate(i), &object(i), g.as_deref())
            .unwrap();
    }
    b.finish().unwrap();
    let seg = Segment::open(&dir).unwrap();
    (dir, seg)
}

fn corpus() -> &'static (PathBuf, Segment) {
    static CORPUS: OnceLock<(PathBuf, Segment)> = OnceLock::new();
    CORPUS.get_or_init(|| build("quads", true))
}

fn corpus_triples_only() -> &'static (PathBuf, Segment) {
    static CORPUS: OnceLock<(PathBuf, Segment)> = OnceLock::new();
    CORPUS.get_or_init(|| build("triples", false))
}

/// Warm point lookup: resolve a fully-bound quad, exact-count it, and decode
/// all four columns back to concise bytes.
fn point_lookup(c: &mut Criterion) {
    let (_, seg) = corpus();
    // Pre-encode a rotation of existing quads (terms only; resolution and
    // decoding are the measured work).
    let keys: Vec<_> = (0..512u64)
        .map(|k| {
            let i = k * 1_711; // spread across the corpus
            (subject(i), predicate(i), object(i), graph(i))
        })
        .collect();
    let mut k = 0usize;
    c.bench_function("point_lookup_warm", |b| {
        b.iter(|| {
            let (s, p, o, g) = &keys[k % keys.len()];
            k += 1;
            let pat = seg
                .resolve_pattern(Some(s), Some(p), Some(o), Some(g.as_deref()))
                .expect("quad exists");
            let n = seg.count(&pat).unwrap();
            assert!(n >= 1);
            let mut decoded = 0usize;
            decoded += seg
                .decode_value(pat.s.unwrap(), TermPos::Subject)
                .unwrap()
                .len();
            decoded += seg
                .decode_value(pat.p.unwrap(), TermPos::Predicate)
                .unwrap()
                .len();
            decoded += seg
                .decode_value(pat.o.unwrap(), TermPos::Object)
                .unwrap()
                .len();
            if let Some(gv) = pat.g {
                if gv > 0 {
                    decoded += seg.decode_value(gv - 1, TermPos::Graph).unwrap().len();
                }
            }
            decoded
        })
    });
}

/// Batched scan throughput over the POS ordering (predicate-bound) and the
/// full SPO ordering.
fn scan_throughput(c: &mut Criterion) {
    let (_, seg) = corpus();
    let mut group = c.benchmark_group("scan");

    let full = Pattern::default();
    let n_full = seg.count(&full).unwrap();
    group.throughput(Throughput::Elements(n_full));
    group.bench_function("full_spo", |b| {
        let mut batch = QuadBatch::new();
        b.iter(|| {
            let mut scan = seg.scan_order(&full, Order::Spo).unwrap();
            let mut n = 0u64;
            while scan.next_batch(&mut batch).unwrap() {
                n += batch.len() as u64;
            }
            assert_eq!(n, n_full);
            n
        })
    });

    // Pure packed-aligned index walk: no graph layer at all.
    let (_, seg_t) = corpus_triples_only();
    let n_t = seg_t.count(&full).unwrap();
    group.throughput(Throughput::Elements(n_t));
    group.bench_function("full_spo_triples_only", |b| {
        let mut batch = QuadBatch::new();
        b.iter(|| {
            let mut scan = seg_t.scan_order(&full, Order::Spo).unwrap();
            let mut n = 0u64;
            while scan.next_batch(&mut batch).unwrap() {
                n += batch.len() as u64;
            }
            assert_eq!(n, n_t);
            n
        })
    });

    let p3 = format!(">http://ex/p{}", 3).into_bytes();
    let bound_p = seg
        .resolve_pattern(None, Some(&p3), None, None)
        .expect("predicate exists");
    let n_p = seg.count(&bound_p).unwrap();
    group.throughput(Throughput::Elements(n_p));
    group.bench_function("bound_p_pos", |b| {
        let mut batch = QuadBatch::new();
        b.iter(|| {
            let mut scan = seg.scan_order(&bound_p, Order::Pos).unwrap();
            let mut n = 0u64;
            while scan.next_batch(&mut batch).unwrap() {
                n += batch.len() as u64;
            }
            assert_eq!(n, n_p);
            n
        })
    });
    group.finish();
}

/// Concurrent readers over one segment: T threads split the 17 predicate
/// scans; aggregate throughput should scale ~linearly with T.
fn reader_scaling(c: &mut Criterion) {
    let (_, seg) = corpus();
    let pats: Vec<Pattern> = (0..N_PREDS)
        .map(|p| {
            let bytes = format!(">http://ex/p{p}").into_bytes();
            seg.resolve_pattern(None, Some(&bytes), None, None)
                .expect("predicate exists")
        })
        .collect();
    let total: u64 = pats.iter().map(|p| seg.count(p).unwrap()).sum();

    let mut group = c.benchmark_group("reader_scaling");
    group.throughput(Throughput::Elements(total));
    group.sample_size(10);
    for threads in [1usize, 2, 4, 8] {
        group.bench_with_input(
            BenchmarkId::from_parameter(threads),
            &threads,
            |b, &threads| {
                b.iter_custom(|iters| {
                    let start = Instant::now();
                    for _ in 0..iters {
                        std::thread::scope(|scope| {
                            for t in 0..threads {
                                let pats = &pats;
                                scope.spawn(move || {
                                    let mut batch = QuadBatch::new();
                                    let mut n = 0u64;
                                    for pat in pats.iter().skip(t).step_by(threads) {
                                        let mut scan = seg.scan_order(pat, Order::Pos).unwrap();
                                        while scan.next_batch(&mut batch).unwrap() {
                                            n += batch.len() as u64;
                                        }
                                    }
                                    n
                                });
                            }
                        });
                    }
                    start.elapsed()
                })
            },
        );
    }
    group.finish();
}

criterion_group!(benches, point_lookup, scan_throughput, reader_scaling);
criterion_main!(benches);
