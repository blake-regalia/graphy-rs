//! Parser throughput benches (M1 exit criteria: ≥ 300 MB/s N-Quads,
//! ≥ 150 MB/s Turtle single-thread). Synthetic corpora shaped like real
//! data: IRI-heavy quads with a mix of plain/lang/typed literals.

use std::fmt::Write as _;
use std::hint::black_box;

use criterion::{criterion_group, criterion_main, Criterion, Throughput};
use graphy_turtle::{NQuadsParser, Options, TurtleParser};

fn nq_corpus(statements: usize) -> String {
    let mut s = String::with_capacity(statements * 120);
    for i in 0..statements {
        let g = i % 5;
        match i % 4 {
            0 => writeln!(
                s,
                "<http://ex.example/res/{i}> <http://ex.example/vocab/p{}> <http://ex.example/res/{}> <http://ex.example/graph/{g}> .",
                i % 17,
                i * 7 % statements
            ),
            1 => writeln!(
                s,
                "<http://ex.example/res/{i}> <http://ex.example/vocab/label> \"resource number {i} with some text payload\" <http://ex.example/graph/{g}> .",
            ),
            2 => writeln!(
                s,
                "<http://ex.example/res/{i}> <http://ex.example/vocab/comment> \"un commentaire numéro {i}\"@fr <http://ex.example/graph/{g}> .",
            ),
            _ => writeln!(
                s,
                "_:b{i} <http://ex.example/vocab/count> \"{i}\"^^<http://www.w3.org/2001/XMLSchema#integer> <http://ex.example/graph/{g}> .",
            ),
        }
        .expect("String write");
    }
    s
}

fn ttl_corpus(statements: usize) -> String {
    let mut s = String::with_capacity(statements * 60);
    s.push_str(
        "@prefix ex: <http://ex.example/res/> .\n@prefix v: <http://ex.example/vocab/> .\n\n",
    );
    for i in (0..statements).step_by(4) {
        writeln!(
            s,
            "ex:r{i} a v:Thing ;\n    v:p{} ex:r{} ;\n    v:label \"resource number {i} with some text payload\", \"un commentaire numéro {i}\"@fr ;\n    v:count {i} .",
            i % 17,
            i * 7 % statements
        )
        .expect("String write");
    }
    s
}

fn bench_parsers(c: &mut Criterion) {
    let nq = nq_corpus(200_000);
    let ttl = ttl_corpus(200_000);

    let mut group = c.benchmark_group("parse");
    group.sample_size(20);

    for trusted in [false, true] {
        let mode = if trusted { "trusted" } else { "validated" };
        let opts = || Options {
            trusted,
            ..Options::default()
        };

        group.throughput(Throughput::Bytes(nq.len() as u64));
        group.bench_function(format!("nquads/{mode}/{}B", nq.len()), |b| {
            b.iter(|| {
                let mut p = NQuadsParser::new(opts()).unwrap();
                let mut n = 0usize;
                for chunk in nq.as_bytes().chunks(64 * 1024) {
                    p.feed(black_box(chunk)).unwrap();
                    n += p.drain().count();
                }
                p.finish().unwrap();
                black_box(n)
            })
        });

        group.throughput(Throughput::Bytes(ttl.len() as u64));
        group.bench_function(format!("turtle/{mode}/{}B", ttl.len()), |b| {
            b.iter(|| {
                let mut p = TurtleParser::new(opts()).unwrap();
                let mut n = 0usize;
                for chunk in ttl.as_bytes().chunks(64 * 1024) {
                    p.feed(black_box(chunk)).unwrap();
                    n += p.drain().count();
                }
                p.finish().unwrap();
                black_box(n)
            })
        });
    }

    group.finish();
}

/// Data-parallel NQ parsing (doc 03 §4.1): same corpus, worker counts
/// 1/2/4/8; the callback is a plain atomic count so the measurement is the
/// parse itself, not a downstream consumer.
fn bench_parallel(c: &mut Criterion) {
    use std::sync::atomic::{AtomicUsize, Ordering};

    let nq = nq_corpus(200_000);
    let mut group = c.benchmark_group("parse-par");
    group.sample_size(20);
    group.throughput(Throughput::Bytes(nq.len() as u64));
    for threads in [1usize, 2, 4, 8] {
        group.bench_function(format!("nquads/t{threads}"), |b| {
            b.iter(|| {
                let n = AtomicUsize::new(0);
                graphy_turtle::par::nquads(
                    black_box(nq.as_bytes()),
                    &Options::default(),
                    threads,
                    |_, _| {
                        n.fetch_add(1, Ordering::Relaxed);
                    },
                )
                .unwrap();
                black_box(n.into_inner())
            })
        });
    }
    group.finish();
}

criterion_group!(benches, bench_parsers, bench_parallel);
criterion_main!(benches);
