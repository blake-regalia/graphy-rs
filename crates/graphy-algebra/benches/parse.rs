//! M6 exit-criteria benchmark (doc 04 §6 / IMPLEMENTATION_PLAN M6): parse
//! throughput over a realistic query mix — the W3C positive-syntax corpus
//! (sparql11 + sparql12 query tests) — for `parse_query` alone and for
//! parse + §18.2 translation + rewrites. Bar: ≥ 50k queries/s single-
//! threaded on the mix. Record results in BENCHMARKS.md.

use std::path::PathBuf;

use criterion::{criterion_group, criterion_main, Criterion, Throughput};
use graphy_algebra::{rewrite, translate_query};
use graphy_sparql_syntax::parse_query;

/// Every `.rq` under the syntax suites that parses (the positive corpus;
/// negative files are filtered by the parse attempt itself).
fn corpus() -> Vec<String> {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../testdata/rdf-tests/sparql");
    let mut out = Vec::new();
    for suite in [
        "sparql11/syntax-query",
        "sparql11/syntax-fed",
        "sparql12/syntax",
        "sparql12/syntax-triple-terms-positive",
    ] {
        let dir = root.join(suite);
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for e in entries.flatten() {
            let path = e.path();
            if path.extension().is_some_and(|x| x == "rq") {
                if let Ok(src) = std::fs::read_to_string(&path) {
                    if parse_query(&src).is_ok() {
                        out.push(src);
                    }
                }
            }
        }
    }
    assert!(
        out.len() >= 100,
        "corpus too small ({}) — is testdata/rdf-tests present?",
        out.len()
    );
    out
}

fn bench_parse(c: &mut Criterion) {
    let corpus = corpus();
    let n = corpus.len() as u64;
    let bytes: usize = corpus.iter().map(String::len).sum();
    eprintln!(
        "corpus: {n} queries, {bytes} bytes, mean {:.0} B/query",
        bytes as f64 / n as f64
    );

    let mut g = c.benchmark_group("parse");
    g.throughput(Throughput::Elements(n));
    g.bench_function("parse_query/w3c-mix", |b| {
        b.iter(|| {
            let mut ok = 0u64;
            for src in &corpus {
                ok += parse_query(std::hint::black_box(src)).is_ok() as u64;
            }
            ok
        })
    });
    g.bench_function("parse+translate+rewrite/w3c-mix", |b| {
        b.iter(|| {
            let mut ok = 0u64;
            for src in &corpus {
                let q = parse_query(std::hint::black_box(src)).expect("positive corpus");
                let t = translate_query(&q).expect("translates");
                ok += matches!(rewrite(t.root), graphy_algebra::Algebra::Bgp(_)) as u64;
            }
            ok
        })
    });
    g.finish();

    // One large analytic query (~2.4 KB): 24 patterns across OPTIONALs,
    // paths, a subquery with aggregation, and filters.
    let mut big =
        String::from("PREFIX ex: <http://example.org/ns#>\nSELECT ?s (COUNT(?w) AS ?n) WHERE {\n");
    for i in 0..8 {
        big.push_str(&format!(
            "  ?s ex:p{i} ?v{i} . OPTIONAL {{ ?v{i} ex:q{i}/ex:r{i}* ?w }}\n               FILTER(?v{i} != ex:x{i} && STRLEN(STR(?v{i})) > {i})\n",
        ));
    }
    big.push_str(
        "  { SELECT ?s (MAX(?d) AS ?m) WHERE { ?s ex:date ?d } GROUP BY ?s }\n         }\nGROUP BY ?s HAVING(COUNT(?w) > 2) ORDER BY DESC(?n) LIMIT 100",
    );
    eprintln!("big query: {} bytes", big.len());
    let mut g = c.benchmark_group("parse-large");
    g.bench_function("parse+translate+rewrite/2.4KB-analytic", |b| {
        b.iter(|| {
            let q = parse_query(std::hint::black_box(&big)).expect("parses");
            let t = translate_query(&q).expect("translates");
            rewrite(t.root)
        })
    });
    g.finish();
}

criterion_group!(benches, bench_parse);
criterion_main!(benches);
