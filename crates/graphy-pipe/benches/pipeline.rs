//! MC C1 exit bar (docs/09 §8): pipeline overhead — `read / scribe` must
//! hold ≥ 90% of the raw parse+write path's throughput on a 1M-quad NQ
//! corpus. Also measures the marginal cost of a pass-through operator.

use criterion::{criterion_group, criterion_main, Criterion, Throughput};
use graphy_pipe::{chain, Event, Flow, NqSink, OpSpec, Sink};
use graphy_turtle::{NQuadsParser, NQuadsWriter, Options};

/// M2-shaped synthetic corpus: ~n/10 subjects, mixed IRI/literal objects,
/// 10 named graphs (mirrors examples/genquads.rs).
fn corpus(n: usize) -> Vec<u8> {
    use std::io::Write as _;
    let mut out = Vec::with_capacity(n * 96);
    for i in 0..n {
        if i % 2 == 0 {
            writeln!(
                out,
                "<http://example.org/s{}> <http://example.org/p{}> \"value {i}\" <http://example.org/g{}> .",
                i % (n / 10).max(1),
                i % 50,
                i % 10,
            )
            .expect("vec write");
        } else {
            writeln!(
                out,
                "<http://example.org/s{}> <http://example.org/p{}> <http://example.org/o{}> <http://example.org/g{}> .",
                i % (n / 10).max(1),
                i % 50,
                i % 977,
                i % 10,
            )
            .expect("vec write");
        }
    }
    out
}

/// The raw path: parser chunks + writer, no pipeline machinery.
fn raw_parse_write(data: &[u8]) -> u64 {
    let mut p = NQuadsParser::new(Options::default()).expect("options valid");
    let mut w = NQuadsWriter::new(std::io::sink());
    let mut n = 0u64;
    for chunk in data.chunks(256 * 1024) {
        p.feed(chunk).expect("corpus is valid");
        for q in p.drain() {
            w.write_quad(&q).expect("io::sink never fails");
            n += 1;
        }
    }
    p.finish().expect("corpus ends cleanly");
    for q in p.drain() {
        w.write_quad(&q).expect("io::sink never fails");
        n += 1;
    }
    n
}

/// The pipeline path: read source → (ops) → scribe terminal.
fn pipeline(data: &[u8], ops: Vec<graphy_pipe::OpSpec>) -> u64 {
    struct Counting<S: Sink> {
        inner: S,
        n: u64,
    }
    impl<S: Sink> std::fmt::Debug for Counting<S> {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("Counting")
        }
    }
    impl<S: Sink> Sink for Counting<S> {
        fn event(&mut self, ev: Event<'_>) -> std::io::Result<Flow> {
            if matches!(ev, Event::Quad(_)) {
                self.n += 1;
            }
            self.inner.event(ev)
        }
        fn finish(&mut self) -> std::io::Result<()> {
            self.inner.finish()
        }
    }

    let built: Vec<Box<dyn graphy_pipe::Op>> = ops
        .iter()
        .map(|spec| match *spec {
            OpSpec::Skip { n, unit } => Box::new(graphy_pipe::Skip::new(n, unit)) as _,
            _ => unreachable!("bench uses skip only"),
        })
        .collect();
    let mut counter = Counting {
        inner: chain(
            built,
            Box::new(NqSink::new(Box::new(std::io::sink()), false)),
        ),
        n: 0,
    };
    let mut input: &[u8] = data;
    graphy_pipe::read_stream(
        &mut input,
        graphy_pipe::Format::Nq,
        Options::default(),
        &mut counter,
        &mut |_| {},
    )
    .expect("corpus is valid");
    counter.finish().expect("io::sink never fails");
    counter.n
}

fn bench(c: &mut Criterion) {
    let data = corpus(1_000_000);
    let mut group = c.benchmark_group("pipe");
    group.sample_size(10);
    group.throughput(Throughput::Bytes(data.len() as u64));
    group.bench_function("raw_parse_write", |b| {
        b.iter(|| raw_parse_write(&data));
    });
    group.bench_function("read_scribe", |b| {
        b.iter(|| pipeline(&data, vec![]));
    });
    group.bench_function("read_skip0_scribe", |b| {
        b.iter(|| {
            pipeline(
                &data,
                vec![OpSpec::Skip {
                    n: 0,
                    unit: graphy_pipe::Unit::Quads,
                }],
            )
        });
    });
    group.finish();
}

criterion_group!(benches, bench);
criterion_main!(benches);
