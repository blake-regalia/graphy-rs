//! Read/write/merge/GC soak harness (M5 exit criterion, doc 07 §8: the
//! 24 h invariant soak with a flat RSS ceiling — duration is a knob, so
//! the same binary serves short validation runs and the full run).
//!
//! Topology mirrors production: one `Store` + one `MergeScheduler`
//! (budget-scaled, wait-for-merge backpressure active), W writer threads,
//! R reader threads, a housekeeping thread calling `Store::gc()`.
//!
//! Writers keep an exactly computable live set by construction — writer
//! `t`'s commit `i` adds quad `f(t, i)`, deletes `f(t, i − K)`, and every
//! 4th commit also adds a PERMANENT quad (net growth: without it, epoch GC
//! alone absorbs bounded churn and the delta never reaches the merge
//! budgets — measured, and worth knowing, but a soak must exercise
//! merges). Expected live per writer at commit floor `i`:
//! `min(i, K) + ceil(i / 4)`. Sweeps assert: exact count, sampled
//! membership (present and absent), generation-directory and WAL-size
//! boundedness (retirement + rotation working), and periodically a deep
//! `Segment::verify`. RSS is sampled throughout; the report compares the
//! first- and last-quartile means (leak check) alongside reader latency
//! percentiles and merge stats.
//!
//! Usage:
//!   cargo run --release -p graphy-store --example soak -- <dir> \
//!     [--duration-secs 600] [--base-quads 1000000] [--writers 4] \
//!     [--readers 4] [--soft 500000] [--hard 2000000]

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use graphy_store::{
    resolve_segment_dir, BuilderConfig, MergeScheduler, Order, Profile, QuadBatch, SchedulerConfig,
    Segment, SegmentBuilder, Store,
};

/// Live-set window per writer.
const K: u64 = 5_000;

fn base_subject(i: u64) -> Vec<u8> {
    format!(">http://ex/s{}", i % 100_000).into_bytes()
}

fn base_predicate(i: u64) -> Vec<u8> {
    format!(">http://ex/p{}", i % 17).into_bytes()
}

fn base_object(i: u64) -> Vec<u8> {
    match i % 3 {
        0 => format!(">http://ex/o{}", i % 50_000).into_bytes(),
        1 => format!("\"literal value {}", i % 25_000).into_bytes(),
        _ => format!("^>http://www.w3.org/2001/XMLSchema#integer\"{i}").into_bytes(),
    }
}

/// Writer `t`'s `j`-th PERMANENT quad (never deleted — the net growth
/// that drives merges).
fn perm_quad(t: u64, j: u64) -> (Vec<u8>, Vec<u8>, Vec<u8>, Option<Vec<u8>>) {
    (
        format!(">http://soak/perm{t}/e{j}").into_bytes(),
        base_predicate(j),
        format!("\"perm {t} {j}").into_bytes(),
        None,
    )
}

/// Writer `t`'s `i`-th soak quad (unique per (t, i)).
fn soak_quad(t: u64, i: u64) -> (Vec<u8>, Vec<u8>, Vec<u8>, Option<Vec<u8>>) {
    (
        format!(">http://soak/w{t}/e{i}").into_bytes(),
        base_predicate(i),
        format!("\"soak {t} {i}").into_bytes(),
        (i % 5 == 0).then(|| format!(">http://soak/g{}", i % 3).into_bytes()),
    )
}

fn rss_kb() -> u64 {
    let out = std::process::Command::new("ps")
        .args(["-o", "rss=", "-p", &std::process::id().to_string()])
        .output()
        .expect("ps");
    String::from_utf8_lossy(&out.stdout)
        .trim()
        .parse()
        .unwrap_or(0)
}

fn parse_flag(args: &[String], name: &str, default: u64) -> u64 {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1))
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let dir = PathBuf::from(args.first().expect("usage: soak <dir> [flags]"));
    let duration = Duration::from_secs(parse_flag(&args, "--duration-secs", 600));
    let base_quads = parse_flag(&args, "--base-quads", 1_000_000);
    let writers = parse_flag(&args, "--writers", 4);
    let readers = parse_flag(&args, "--readers", 4);
    let soft = parse_flag(&args, "--soft", 100_000);
    let hard = parse_flag(&args, "--hard", 2_000_000);

    // ---- Base segment.
    let _ = std::fs::remove_dir_all(&dir);
    println!("building {base_quads}-quad base at {} …", dir.display());
    let mut cfg = BuilderConfig::new(&dir);
    cfg.profile = Profile::Balanced;
    let mut b = SegmentBuilder::new(cfg).unwrap();
    for i in 0..base_quads {
        b.push_quad(
            &base_subject(i),
            &base_predicate(i),
            &base_object(i),
            (i % 6 != 0)
                .then(|| format!(">http://ex/g{}", i % 6).into_bytes())
                .as_deref(),
        )
        .unwrap();
    }
    b.finish().unwrap();

    let store = Arc::new(Store::open(&dir).unwrap());
    store.set_delta_budget(soft, hard);
    let sched = MergeScheduler::spawn(
        Arc::clone(&store),
        SchedulerConfig {
            interval: Duration::from_millis(200),
            ..SchedulerConfig::default()
        },
    );

    let stop = AtomicBool::new(false);
    let commits = AtomicU64::new(0);
    // Writer progress: next i per writer (i-K.. are live).
    let progress: Vec<AtomicU64> = (0..writers).map(|_| AtomicU64::new(0)).collect();
    let read_lat_us: Mutex<Vec<u64>> = Mutex::new(Vec::new());
    let rss_series: Mutex<Vec<u64>> = Mutex::new(Vec::new());
    let sweep_failures = AtomicU64::new(0);

    let base_count = {
        let snap = store.snapshot();
        let pat = snap.resolve_pattern(None, None, None, None).unwrap();
        snap.count(&pat).unwrap()
    };
    println!(
        "soak: {}s, {writers} writers (window {K}), {readers} readers, budgets {soft}/{hard}",
        duration.as_secs()
    );
    let started = Instant::now();

    std::thread::scope(|scope| {
        // ---- Writers.
        for t in 0..writers {
            let (store, stop, commits, progress) = (&store, &stop, &commits, &progress);
            scope.spawn(move || {
                let mut i = 0u64;
                while !stop.load(Ordering::Relaxed) {
                    let add = soak_quad(t, i);
                    let perm = (i % 4 == 0).then(|| perm_quad(t, i / 4));
                    let del = (i >= K).then(|| soak_quad(t, i - K));
                    let dels: Vec<_> = del
                        .iter()
                        .map(|q| {
                            (
                                q.0.as_slice(),
                                q.1.as_slice(),
                                q.2.as_slice(),
                                q.3.as_deref(),
                            )
                        })
                        .collect();
                    let mut adds = vec![(
                        add.0.as_slice(),
                        add.1.as_slice(),
                        add.2.as_slice(),
                        add.3.as_deref(),
                    )];
                    if let Some(q) = &perm {
                        adds.push((
                            q.0.as_slice(),
                            q.1.as_slice(),
                            q.2.as_slice(),
                            q.3.as_deref(),
                        ));
                    }
                    // Mixed durability: every 16th commit is Strict.
                    let durability = if i % 16 == 0 {
                        graphy_store::Durability::Strict
                    } else {
                        graphy_store::Durability::Relaxed
                    };
                    store
                        .apply_with(&dels, &adds, durability)
                        .expect("soak apply");
                    i += 1;
                    // Publish AFTER the commit landed: sweeps read a floor.
                    progress[t as usize].store(i, Ordering::Release);
                    commits.fetch_add(1, Ordering::Relaxed);
                    // ~1k commits/s per writer target: stay realistic, not
                    // budget-saturating.
                    std::thread::sleep(Duration::from_millis(1));
                }
            });
        }

        // ---- Readers: random bound-p scans, latency sampled.
        for r in 0..readers {
            let (store, stop, read_lat_us) = (&store, &stop, &read_lat_us);
            scope.spawn(move || {
                let mut n = r; // decorrelate
                let mut batch = QuadBatch::new();
                while !stop.load(Ordering::Relaxed) {
                    let snap = store.snapshot();
                    let p = base_predicate(n);
                    n += 1;
                    let t0 = Instant::now();
                    let Some(pat) = snap.resolve_pattern(None, Some(&p), None, None) else {
                        continue;
                    };
                    let mut scan = snap.scan(&pat, Order::Pos).unwrap();
                    let mut seen = 0u64;
                    while scan.next_batch(&mut batch).unwrap() {
                        seen += batch.len() as u64;
                    }
                    assert!(seen > 0, "bound-p scan returned nothing");
                    read_lat_us
                        .lock()
                        .unwrap()
                        .push(t0.elapsed().as_micros() as u64);
                    std::thread::sleep(Duration::from_millis(5));
                }
            });
        }

        // ---- Housekeeping: gc + RSS sampling.
        {
            let (store, stop, rss_series) = (&store, &stop, &rss_series);
            scope.spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    store.gc();
                    rss_series.lock().unwrap().push(rss_kb());
                    std::thread::sleep(Duration::from_secs(2));
                }
            });
        }

        // ---- Sweeps: every 15 s, exact-count + sampled membership +
        // boundedness; deep verify every 4th sweep.
        {
            let (store, stop, progress, sweep_failures) =
                (&store, &stop, &progress, &sweep_failures);
            let dir = &dir;
            scope.spawn(move || {
                let mut sweep = 0u64;
                while !stop.load(Ordering::Relaxed) {
                    std::thread::sleep(Duration::from_secs(15));
                    if stop.load(Ordering::Relaxed) {
                        break;
                    }
                    sweep += 1;
                    // Read floors first, snapshot second: the snapshot then
                    // contains AT LEAST floor commits per writer; membership
                    // of quads below the floor window is guaranteed.
                    let floors: Vec<u64> = progress
                        .iter()
                        .map(|p| p.load(Ordering::Acquire))
                        .collect();
                    let snap = store.snapshot();
                    let pat = snap.resolve_pattern(None, None, None, None).unwrap();
                    let count = snap.count(&pat).unwrap();
                    // Exact-count bound: every writer's live window is
                    // [floor−K, ceiling) where ceiling read AFTER the
                    // snapshot bounds late commits.
                    let ceilings: Vec<u64> = progress
                        .iter()
                        .map(|p| p.load(Ordering::Acquire))
                        .collect();
                    let live_at = |i: u64| i.min(K) + i.div_ceil(4);
                    let min_live: u64 = floors.iter().map(|&f| live_at(f)).sum();
                    let max_live: u64 = ceilings.iter().map(|&c| live_at(c)).sum();
                    let ok_count =
                        count >= base_count + min_live && count <= base_count + max_live;
                    // Sampled membership: for each writer, a quad safely
                    // inside the floor window must be present; one far below
                    // must be gone.
                    let mut ok_member = true;
                    for (t, &f) in floors.iter().enumerate() {
                        if f == 0 {
                            continue;
                        }
                        let probe = |i: u64, want: bool| -> bool {
                            let q = soak_quad(t as u64, i);
                            let got = snap
                                .resolve_pattern(
                                    Some(&q.0),
                                    Some(&q.1),
                                    Some(&q.2),
                                    Some(q.3.as_deref()),
                                )
                                .map(|p| snap.count(&p).unwrap() > 0)
                                .unwrap_or(false);
                            got == want
                        };
                        // Newest committed add is present (its delete
                        // comes K commits later; ceilings stay far below
                        // f + K in the ms between floor read and snapshot).
                        if ceilings[t] < f + K {
                            ok_member &= probe(f - 1, true);
                        }
                        // Quad f−K−1 was deleted at commit f−1 ≤ floor.
                        if f > K {
                            ok_member &= probe(f - K - 1, false);
                        }
                    }
                    // Boundedness: retired generations unlink; WAL rotates.
                    let gens = std::fs::read_dir(dir)
                        .unwrap()
                        .flatten()
                        .filter(|e| e.file_name().to_string_lossy().starts_with("gen-"))
                        .count();
                    let wal_mb = std::fs::metadata(dir.join("wal.log"))
                        .map(|m| m.len() >> 20)
                        .unwrap_or(0);
                    let ok_bounded = gens <= 3 && wal_mb < 2_048;
                    let mut ok_verify = true;
                    if sweep % 4 == 0 {
                        let seg = resolve_segment_dir(dir).unwrap();
                        ok_verify = Segment::verify(&seg).is_ok();
                    }
                    let ok = ok_count && ok_member && ok_bounded && ok_verify;
                    if !ok {
                        sweep_failures.fetch_add(1, Ordering::Relaxed);
                    }
                    println!(
                        "[{:>5.0}s] sweep {sweep}: count={count} (base {base_count} + live ≤{max_live}) \
                         gen={} dirs={gens} wal={wal_mb}MiB delta={} {}",
                        started.elapsed().as_secs_f64(),
                        snap.generation(),
                        snap.delta_events(),
                        if ok { "OK" } else { "FAIL" },
                    );
                }
            });
        }

        std::thread::sleep(duration);
        stop.store(true, Ordering::Relaxed);
    });

    // ---- Report.
    let merges = sched_stats(&sched);
    drop(sched); // joins the merge thread; nothing mutates below
    let total = commits.load(Ordering::Relaxed);
    let mut lats = read_lat_us.into_inner().unwrap();
    lats.sort_unstable();
    let pct = |p: f64| -> u64 {
        if lats.is_empty() {
            0
        } else {
            lats[((lats.len() as f64 - 1.0) * p) as usize]
        }
    };
    let rss = rss_series.into_inner().unwrap();
    let quart = rss.len() / 4;
    let mean = |s: &[u64]| s.iter().sum::<u64>() / s.len().max(1) as u64;
    let (rss_first, rss_last) = if quart > 0 {
        (mean(&rss[..quart]), mean(&rss[rss.len() - quart..]))
    } else {
        (mean(&rss), mean(&rss))
    };
    let failures = sweep_failures.load(Ordering::Relaxed);

    // Final exact model check: writers stopped, so live sets are exact.
    let snap = store.snapshot();
    let pat = snap.resolve_pattern(None, None, None, None).unwrap();
    let final_count = snap.count(&pat).unwrap();
    let live: u64 = (0..writers)
        .map(|t| {
            let i = progress[t as usize].load(Ordering::Acquire);
            i.min(K) + i.div_ceil(4)
        })
        .sum();
    let exact = final_count == base_count + live;

    println!("\n== soak report ==");
    println!(
        "duration            {:.0}s",
        started.elapsed().as_secs_f64()
    );
    println!(
        "commits             {total} ({:.0}/s)",
        total as f64 / started.elapsed().as_secs_f64()
    );
    println!("merges completed    {merges}");
    println!("final generation    {}", snap.generation());
    println!(
        "reader p50 / p99    {} µs / {} µs ({} samples)",
        pct(0.5),
        pct(0.99),
        lats.len()
    );
    println!(
        "rss first→last Q    {} → {} MiB ({:+.1}%)",
        rss_first >> 10,
        rss_last >> 10,
        (rss_last as f64 - rss_first as f64) / rss_first.max(1) as f64 * 100.0
    );
    println!("sweep failures      {failures}");
    println!("final exact count   {}", if exact { "OK" } else { "FAIL" });
    let verdict = failures == 0 && exact;
    println!(
        "verdict             {}",
        if verdict { "PASS" } else { "FAIL" }
    );
    std::process::exit(if verdict { 0 } else { 1 });
}

fn sched_stats(s: &MergeScheduler) -> String {
    match s.last_error() {
        None => format!("{}", s.merges_completed()),
        Some(e) => format!("{} (last error: {e})", s.merges_completed()),
    }
}
