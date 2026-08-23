//! Merge SLO driver (M5 exit-criteria runs): open an existing segment as
//! a store, apply a delta of the requested size, run one merge, and
//! report `MergeStats` (fold wall, exclusive swap pause, suffix) plus the
//! process's peak RSS. Usage:
//!
//!   merge_slo <store-dir> [--delta-quads 2000000] [--sort-budget-mib 1024]

use std::sync::Arc;
use std::time::Instant;

use graphy_store::{Durability, MergeConfig, Store};

fn flag(args: &[String], name: &str, default: u64) -> u64 {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1))
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

fn rss_mb() -> u64 {
    let out = std::process::Command::new("ps")
        .args(["-o", "rss=", "-p", &std::process::id().to_string()])
        .output()
        .expect("ps");
    String::from_utf8_lossy(&out.stdout)
        .trim()
        .parse::<u64>()
        .unwrap_or(0)
        >> 10
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let dir = std::path::PathBuf::from(args.first().expect("usage: merge_slo <store-dir>"));
    let delta_quads = flag(&args, "--delta-quads", 2_000_000);
    let sort_budget = (flag(&args, "--sort-budget-mib", 1024) as usize) << 20;

    let store = Arc::new(Store::open(&dir).expect("open store"));
    store.set_delta_budget(u64::MAX, u64::MAX);
    let base = {
        let snap = store.snapshot();
        snap.segment().manifest.counts.quads
    };
    println!("base: {base} quads; applying {delta_quads}-quad delta …");

    let t0 = Instant::now();
    let batch = 10_000u64;
    for k in 0..delta_quads / batch {
        let quads: Vec<(Vec<u8>, Vec<u8>, Vec<u8>)> = (0..batch)
            .map(|j| {
                let i = k * batch + j;
                (
                    format!(">http://slo/e{i}").into_bytes(),
                    format!(">http://ex/p{}", i % 17).into_bytes(),
                    format!("\"slo payload {i}").into_bytes(),
                )
            })
            .collect();
        let refs: Vec<_> = quads
            .iter()
            .map(|q| (q.0.as_slice(), q.1.as_slice(), q.2.as_slice(), None))
            .collect();
        store
            .apply_with(&[], &refs, Durability::Relaxed)
            .expect("apply");
    }
    println!(
        "delta applied in {:.1}s (rss {} MiB)",
        t0.elapsed().as_secs_f64(),
        rss_mb()
    );

    let cfg = MergeConfig {
        sort_budget,
        ..MergeConfig::default()
    };
    let t1 = Instant::now();
    let snap = store.merge_with(&cfg).expect("merge");
    let wall = t1.elapsed();
    let s = store.last_merge_stats().expect("stats");
    println!("\n== merge SLO report ==");
    println!("folded              {} quads", s.folded_quads);
    println!(
        "merge wall          {:.1}s ({:.2}M quads/s)",
        wall.as_secs_f64(),
        s.folded_quads as f64 / wall.as_secs_f64() / 1e6
    );
    println!("build (concurrent)  {:.1}s", s.build.as_secs_f64());
    println!(
        "swap pause          {:.1} ms ({} suffix events)",
        s.swap.as_secs_f64() * 1e3,
        s.suffix_events
    );
    println!("new generation      {}", snap.generation());
    println!("peak-ish rss        {} MiB", rss_mb());
}
