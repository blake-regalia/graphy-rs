//! Vectorized-engine execution tests: morsel-parallel equivalence with
//! the reference evaluator, cancellation/deadline/memory-budget error
//! paths, and a no-panic chaos loop (doc 05 §9).

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use graphy_algebra::{rewrite, translate_query, TranslatedQuery};
use graphy_engine::exec::{evaluate_with, ExecOptions};
use graphy_engine::{evaluate_ref, Output};
use graphy_sparql_syntax::parse_query;
use graphy_store::{BuilderConfig, Profile, SegmentBuilder, Store};

/// Chain corpus: s{i} -p1-> m{i} -p2-> t{i%16}, s{i} -val-> i.
fn build_store(dir: &PathBuf, n: usize) -> Store {
    let iri = |s: &str| format!(">http://x/{s}").into_bytes();
    let int = |i: i64| format!("^>http://www.w3.org/2001/XMLSchema#integer\"{i}").into_bytes();
    let mut cfg = BuilderConfig::new(dir);
    cfg.profile = Profile::Balanced;
    cfg.sort_budget = 1 << 16;
    let mut b = SegmentBuilder::new(cfg).unwrap();
    for i in 0..n {
        let s = iri(&format!("s{i}"));
        let m = iri(&format!("m{i}"));
        let t = iri(&format!("t{}", i % 16));
        b.push_quad(&s, &iri("p1"), &m, None).unwrap();
        b.push_quad(&m, &iri("p2"), &t, None).unwrap();
        b.push_quad(&s, &iri("val"), &int(i as i64), None).unwrap();
    }
    b.finish().unwrap();
    Store::open(dir).unwrap()
}

fn store() -> &'static Store {
    use std::sync::OnceLock;
    static STORE: OnceLock<Store> = OnceLock::new();
    STORE.get_or_init(|| {
        let dir = std::env::temp_dir().join(format!("graphy-engine-exec-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        build_store(&dir, 3000)
    })
}

fn translated(src: &str) -> TranslatedQuery {
    let q = parse_query(src).unwrap_or_else(|e| panic!("parse `{src}`: {e}"));
    let mut t = translate_query(&q).unwrap_or_else(|e| panic!("translate `{src}`: {e}"));
    t.root = rewrite(t.root.clone());
    t
}

const CHAIN: &str = "SELECT ?s ?t WHERE { ?s <http://x/p1> ?m . ?m <http://x/p2> ?t . ?s <http://x/val> ?v FILTER(?v >= 100) }";

fn sorted_rows(o: Output) -> Vec<Vec<Option<Vec<u8>>>> {
    let Output::Solutions { mut rows, .. } = o else {
        panic!("expected solutions")
    };
    rows.sort();
    rows
}

#[test]
fn parallel_equivalence() {
    let snap = store().snapshot();
    let t = translated(CHAIN);
    let reference = sorted_rows(evaluate_ref(&snap, &t).unwrap());
    assert_eq!(reference.len(), 2900);
    // Force the morsel pool on (threshold 1) at several widths.
    for threads in [2, 4, 8] {
        let opts = ExecOptions {
            threads,
            parallel_threshold: Some(1),
            ..ExecOptions::default()
        };
        let par = sorted_rows(evaluate_with(&snap, &t, &opts).unwrap());
        assert_eq!(par, reference, "threads={threads}");
    }
    // And confirm the unforced default path agrees too.
    let seq = sorted_rows(evaluate_with(&snap, &t, &ExecOptions::default()).unwrap());
    assert_eq!(seq, reference);
}

#[test]
fn parallel_preserves_order_without_sort() {
    // Morsel outputs reassemble in morsel order — identical row order
    // to the sequential pipeline, no ORDER BY needed.
    let snap = store().snapshot();
    let t = translated("SELECT ?s ?m WHERE { ?s <http://x/p1> ?m }");
    let opts = ExecOptions {
        threads: 4,
        parallel_threshold: Some(1),
        ..ExecOptions::default()
    };
    let Output::Solutions { rows: seq, .. } =
        evaluate_with(&snap, &t, &ExecOptions::default()).unwrap()
    else {
        panic!()
    };
    let Output::Solutions { rows: par, .. } = evaluate_with(&snap, &t, &opts).unwrap() else {
        panic!()
    };
    assert_eq!(seq, par);
}

#[test]
fn cancellation() {
    let snap = store().snapshot();
    let t = translated(CHAIN);
    let flag = Arc::new(AtomicBool::new(true)); // pre-cancelled
    let opts = ExecOptions {
        cancel: Some(flag),
        ..ExecOptions::default()
    };
    let err = evaluate_with(&snap, &t, &opts).unwrap_err();
    assert!(err.to_string().contains("cancelled"), "{err}");
}

#[test]
fn deadline() {
    let snap = store().snapshot();
    let t = translated(CHAIN);
    let opts = ExecOptions {
        deadline: Some(std::time::Instant::now() - std::time::Duration::from_millis(1)),
        ..ExecOptions::default()
    };
    let err = evaluate_with(&snap, &t, &opts).unwrap_err();
    assert!(err.to_string().contains("deadline"), "{err}");
}

#[test]
fn memory_budget() {
    let snap = store().snapshot();
    // ORDER BY materializes every row — a tiny budget must fail cleanly.
    let t = translated("SELECT ?s WHERE { ?s <http://x/p1> ?m . ?m <http://x/p2> ?t } ORDER BY ?s");
    let opts = ExecOptions {
        mem_budget: Some(256),
        ..ExecOptions::default()
    };
    let err = evaluate_with(&snap, &t, &opts).unwrap_err();
    assert!(err.to_string().contains("memory budget"), "{err}");
    // A sane budget passes.
    let opts = ExecOptions {
        mem_budget: Some(64 << 20),
        ..ExecOptions::default()
    };
    evaluate_with(&snap, &t, &opts).unwrap();
}

#[test]
fn chaos_cancellation_no_panic() {
    // Random-delay cancellation racing real queries: every outcome must
    // be a clean Ok or a clean "cancelled" error — never a panic.
    let snap = store().snapshot();
    let t = translated(CHAIN);
    for delay_us in [0u64, 10, 50, 100, 500, 1000, 5000] {
        let flag = Arc::new(AtomicBool::new(false));
        let killer = {
            let flag = flag.clone();
            std::thread::spawn(move || {
                std::thread::sleep(std::time::Duration::from_micros(delay_us));
                flag.store(true, Ordering::Relaxed);
            })
        };
        let opts = ExecOptions {
            cancel: Some(flag),
            threads: 4,
            parallel_threshold: Some(1),
            ..ExecOptions::default()
        };
        match evaluate_with(&snap, &t, &opts) {
            Ok(out) => {
                assert_eq!(sorted_rows(out).len(), 2900, "completed run must be exact");
            }
            Err(e) => assert!(e.to_string().contains("cancelled"), "{e}"),
        }
        killer.join().unwrap();
    }
}

#[test]
fn explain_renders_plan() {
    let snap = store().snapshot();
    let t = translated(CHAIN);
    let plan = graphy_engine::exec::explain(&snap, &t).unwrap();
    // Project → Filter → BindJoin chain over the driving scan, with
    // exact estimates on the leaves.
    assert!(plan.contains("Project"), "{plan}");
    assert!(plan.contains("BindJoin"), "{plan}");
    assert!(plan.contains("Scan"), "{plan}");
    assert!(plan.contains("(est 3000)"), "{plan}");
}

#[test]
fn explain_analyze_reports_actuals() {
    let snap = store().snapshot();
    let t = translated(CHAIN);
    let (out, plan) =
        graphy_engine::exec::explain_analyze(&snap, &t, &ExecOptions::default()).unwrap();
    assert_eq!(sorted_rows(out).len(), 2900);
    assert!(plan.contains("[rows "), "{plan}");
    assert!(plan.contains("ms]"), "{plan}");
    // The filter's output cardinality is the query's row count.
    let filter_line = plan.lines().find(|l| l.contains("Filter")).unwrap();
    assert!(filter_line.contains("rows 2900"), "{plan}");
}

#[test]
fn plan_cache_hits_within_snapshot() {
    let snap = store().snapshot();
    let t = translated(CHAIN);
    // Two runs on the same snapshot: second uses the cached plan (a
    // behavioral check — results identical either way; this pins the
    // cache path executing correctly).
    let a = sorted_rows(evaluate_with(&snap, &t, &ExecOptions::default()).unwrap());
    let b = sorted_rows(evaluate_with(&snap, &t, &ExecOptions::default()).unwrap());
    assert_eq!(a, b);
    assert_eq!(a.len(), 2900);
}
