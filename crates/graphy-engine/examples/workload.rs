//! Corpus workload runner (M10 benchmark dashboard): run a directory or
//! file of SPARQL queries against a store and report per-query (or
//! per-file) row counts and timings. Complements `graphy query` for
//! one-offs — this amortizes store open + parse across a whole workload
//! and reports min-of-N wall times.
//!
//!   cargo run --release -p graphy-engine --example workload -- \
//!       <store-dir> <query-path>... [--lines] [--repeat N] \
//!       [--threads N] [--timeout SECS]
//!
//! A query path is a .sparql/.rq file (whole file = one query) or a
//! directory (all .sparql/.rq files, sorted). With --lines, every
//! non-empty line of each file is a separate query (WatDiv stress
//! workload format) and results aggregate per file.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use graphy_algebra::{rewrite, translate_query};
use graphy_engine::exec::{evaluate_with, ExecOptions};
use graphy_engine::Output;
use graphy_sparql_syntax::parse_query;
use graphy_store::Store;

fn rows_of(out: &Output) -> usize {
    match out {
        Output::Solutions { rows, .. } => rows.len(),
        Output::Boolean(_) => 1,
        Output::Triples(t) => t.len(),
    }
}

struct QueryStat {
    label: String,
    queries: usize,
    failures: usize,
    rows: usize,
    /// Sum over queries of the min-of-N wall time.
    wall: Duration,
}

fn run_queries(
    store: &Store,
    label: &str,
    queries: &[(String, String)],
    repeat: usize,
    opts_base: &ExecOptions,
    timeout: Option<u64>,
    per_query: bool,
) -> QueryStat {
    let mut stat = QueryStat {
        label: label.to_owned(),
        queries: 0,
        failures: 0,
        rows: 0,
        wall: Duration::ZERO,
    };
    for (name, src) in queries {
        stat.queries += 1;
        let parsed = parse_query(src)
            .map_err(|e| e.to_string())
            .and_then(|q| translate_query(&q).map_err(|e| e.to_string()));
        let mut t = match parsed {
            Ok(t) => t,
            Err(e) => {
                eprintln!("{name}: parse/translate: {e}");
                stat.failures += 1;
                continue;
            }
        };
        t.root = rewrite(t.root.clone());
        let snap = store.snapshot();
        let mut best: Option<(Duration, usize)> = None;
        let mut failed = false;
        for _ in 0..repeat.max(1) {
            let opts = ExecOptions {
                deadline: timeout.map(|s| Instant::now() + Duration::from_secs(s)),
                threads: opts_base.threads,
                ..ExecOptions::default()
            };
            let t0 = Instant::now();
            match evaluate_with(&snap, &t, &opts) {
                Ok(out) => {
                    let d = t0.elapsed();
                    let r = rows_of(&out);
                    if best.is_none_or(|(b, _)| d < b) {
                        best = Some((d, r));
                    }
                }
                Err(e) => {
                    eprintln!("{name}: {e}");
                    failed = true;
                    break;
                }
            }
        }
        match best {
            Some((d, r)) if !failed => {
                stat.rows += r;
                stat.wall += d;
                if per_query {
                    println!("{name}\t{r}\t{:.3}", d.as_secs_f64() * 1e3);
                }
            }
            _ => stat.failures += 1,
        }
    }
    stat
}

fn collect_files(path: &Path) -> Vec<PathBuf> {
    if path.is_dir() {
        let mut files: Vec<PathBuf> = std::fs::read_dir(path)
            .expect("read query dir")
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| {
                matches!(
                    p.extension().and_then(|e| e.to_str()),
                    Some("sparql" | "rq")
                )
            })
            .collect();
        files.sort();
        files
    } else {
        vec![path.to_path_buf()]
    }
}

fn main() {
    let mut args = std::env::args().skip(1);
    let mut store_dir: Option<PathBuf> = None;
    let mut paths: Vec<PathBuf> = Vec::new();
    let mut lines = false;
    let mut repeat = 3usize;
    let mut threads = 0usize;
    let mut timeout: Option<u64> = None;
    while let Some(a) = args.next() {
        match a.as_str() {
            "--lines" => lines = true,
            "--repeat" => repeat = args.next().expect("--repeat N").parse().expect("repeat"),
            "--threads" => threads = args.next().expect("--threads N").parse().expect("threads"),
            "--timeout" => {
                timeout = Some(
                    args.next()
                        .expect("--timeout SECS")
                        .parse()
                        .expect("timeout"),
                )
            }
            _ if store_dir.is_none() => store_dir = Some(PathBuf::from(a)),
            _ => paths.push(PathBuf::from(a)),
        }
    }
    let store_dir = store_dir.expect("usage: workload <store-dir> <query-path>... [--lines]");
    assert!(!paths.is_empty(), "no query paths given");

    let store = Store::open(&store_dir).expect("open store");
    let opts = ExecOptions {
        threads,
        ..ExecOptions::default()
    };

    let mut stats: Vec<QueryStat> = Vec::new();
    for path in paths.iter().flat_map(|p| collect_files(p)) {
        let text = std::fs::read_to_string(&path).expect("read query file");
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        if lines {
            let queries: Vec<(String, String)> = text
                .lines()
                .map(str::trim)
                .filter(|l| !l.is_empty())
                .enumerate()
                .map(|(i, l)| (format!("{name}:{}", i + 1), l.to_owned()))
                .collect();
            stats.push(run_queries(
                &store, &name, &queries, repeat, &opts, timeout, false,
            ));
        } else {
            let queries = vec![(name.clone(), text)];
            stats.push(run_queries(
                &store, &name, &queries, repeat, &opts, timeout, true,
            ));
        }
    }

    println!();
    println!("file\tqueries\tfailures\trows\ttotal_ms\tmean_ms");
    let (mut q, mut f, mut w) = (0usize, 0usize, Duration::ZERO);
    for s in &stats {
        let ok = s.queries - s.failures;
        println!(
            "{}\t{}\t{}\t{}\t{:.3}\t{:.3}",
            s.label,
            s.queries,
            s.failures,
            s.rows,
            s.wall.as_secs_f64() * 1e3,
            if ok > 0 {
                s.wall.as_secs_f64() * 1e3 / ok as f64
            } else {
                0.0
            }
        );
        q += s.queries;
        f += s.failures;
        w += s.wall;
    }
    println!(
        "TOTAL\t{q}\t{f}\t-\t{:.3}\t{:.3}",
        w.as_secs_f64() * 1e3,
        if q > f {
            w.as_secs_f64() * 1e3 / (q - f) as f64
        } else {
            0.0
        }
    );
}
