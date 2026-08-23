//! M7 query-engine benchmarks (BENCHMARKS.md M7 section): point
//! lookup, star and chain joins (sequential vs morsel-parallel),
//! filter scans with the inline-ID fast path, and aggregation — over a
//! 100k-quad synthetic corpus.

use criterion::{criterion_group, criterion_main, Criterion};
use graphy_algebra::{rewrite, translate_query, TranslatedQuery};
use graphy_engine::exec::{evaluate_with, ExecOptions};
use graphy_sparql_syntax::parse_query;
use graphy_store::{BuilderConfig, Profile, SegmentBuilder, Store};

const N: usize = 100_000;

fn build_store(dir: &std::path::Path) -> Store {
    let iri = |s: String| format!(">http://b/{s}").into_bytes();
    let int = |i: i64| format!("^>http://www.w3.org/2001/XMLSchema#integer\"{i}").into_bytes();
    let mut cfg = BuilderConfig::new(dir);
    cfg.profile = Profile::Balanced;
    cfg.sort_budget = 1 << 22;
    let mut b = SegmentBuilder::new(cfg).unwrap();
    for i in 0..N {
        let s = iri(format!("s{i}"));
        b.push_quad(
            &s,
            &iri("knows".into()),
            &iri(format!("s{}", (i * 7) % N)),
            None,
        )
        .unwrap();
        b.push_quad(&s, &iri("type".into()), &iri(format!("T{}", i % 20)), None)
            .unwrap();
        b.push_quad(&s, &iri("val".into()), &int((i % 1000) as i64), None)
            .unwrap();
    }
    b.finish().unwrap();
    Store::open(dir).unwrap()
}

fn tq(src: &str) -> TranslatedQuery {
    let q = parse_query(src).unwrap();
    let mut t = translate_query(&q).unwrap();
    t.root = rewrite(t.root.clone());
    t
}

fn bench(c: &mut Criterion) {
    let dir = std::env::temp_dir().join(format!("graphy-bench-query-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let store = build_store(&dir);
    let snap = store.snapshot();
    let seq = ExecOptions {
        threads: 1,
        ..ExecOptions::default()
    };
    let par = ExecOptions {
        parallel_threshold: Some(1024),
        ..ExecOptions::default()
    };

    let point = tq("ASK { <http://b/s777> <http://b/knows> <http://b/s5439> }");
    c.bench_function("point_ask", |b| {
        b.iter(|| evaluate_with(&snap, &point, &seq).unwrap())
    });

    let star = tq("SELECT ?s ?o ?v WHERE { ?s <http://b/type> <http://b/T3> . ?s <http://b/knows> ?o . ?s <http://b/val> ?v }");
    c.bench_function("star_join_seq", |b| {
        b.iter(|| evaluate_with(&snap, &star, &seq).unwrap())
    });
    c.bench_function("star_join_par", |b| {
        b.iter(|| evaluate_with(&snap, &star, &par).unwrap())
    });

    let chain = tq("SELECT ?a ?c WHERE { ?a <http://b/knows> ?b . ?b <http://b/knows> ?c . ?c <http://b/type> <http://b/T7> }");
    c.bench_function("chain_join_seq", |b| {
        b.iter(|| evaluate_with(&snap, &chain, &seq).unwrap())
    });
    c.bench_function("chain_join_par", |b| {
        b.iter(|| evaluate_with(&snap, &chain, &par).unwrap())
    });

    let filter = tq("SELECT ?s WHERE { ?s <http://b/val> ?v FILTER(?v > 990) }");
    c.bench_function("filter_scan_inline", |b| {
        b.iter(|| evaluate_with(&snap, &filter, &seq).unwrap())
    });

    let agg = tq("SELECT ?t (COUNT(?s) AS ?n) WHERE { ?s <http://b/type> ?t } GROUP BY ?t");
    c.bench_function("group_count", |b| {
        b.iter(|| evaluate_with(&snap, &agg, &seq).unwrap())
    });
}

criterion_group! {
    name = benches;
    config = Criterion::default().sample_size(20);
    targets = bench
}
criterion_main!(benches);
