//! Merge fault matrix (M5, doc 07 §8.2): the swap's fsync schedule is
//! crash-safe at every step. Two harnesses over the same self-reexec child
//! (writers churning Strict commits while merges run):
//!
//! - **Deterministic matrix**: `GRAPHY_FAILPOINT` aborts the child inside
//!   `merge_with` at each named point — after the `MergeStart` bracket,
//!   after the G+1 build, after the rotated WAL is staged, after the
//!   `CURRENT` flip, and after the rotated log activates. Recovery must
//!   land on the expected generation (old before the flip, new at/after
//!   it) with a committed-prefix state.
//! - **Randomized race**: the child runs a continuous merge loop against 3
//!   writers and is SIGKILLed at varied delays, so kills land inside
//!   builds, swaps, and commit groups nondeterministically.
//!
//! Recovery assertions (both): every Strict-acked commit survives,
//! per-writer survival is prefix-closed, recovered quad count equals the
//! recovered epoch (each commit adds exactly one unique quad; merges
//! preserve counts and epochs), the resolved segment deep-verifies, crash
//! debris is gone after reopen, and the store takes writes *and a full
//! merge* afterwards.

use std::collections::BTreeSet;
use std::io::{BufRead, BufReader, Write as _};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use graphy_store::{
    resolve_segment_dir, BuilderConfig, Order, Profile, QuadBatch, Segment, SegmentBuilder, Store,
    TermPos,
};

const BASE_QUADS: u64 = 3;

fn subject(ns: &str, round: u64, thread: u64, i: u64) -> Vec<u8> {
    format!(">http://mf/{ns}{round}t{thread}i{i}").into_bytes()
}

/// The child's writer+merge loop. A no-op under a normal `cargo test` run;
/// the parents below re-invoke this binary with the env set.
#[test]
fn merge_fault_child() {
    let Ok(dir) = std::env::var("GRAPHY_MERGE_FAULT_DIR") else {
        return;
    };
    let ns = std::env::var("GRAPHY_MERGE_FAULT_NS").expect("ns set");
    let round: u64 = std::env::var("GRAPHY_MERGE_FAULT_ROUND")
        .expect("round set")
        .parse()
        .expect("round number");
    let race = std::env::var("GRAPHY_MERGE_FAULT_RACE").is_ok();
    let writers: u64 = if race { 3 } else { 2 };

    let store = Store::open(Path::new(&dir)).expect("child open");
    {
        let snap = store.snapshot();
        println!("ready {} {}", snap.epoch(), snap.generation());
        std::io::stdout().flush().expect("flush");
    }

    let stop = AtomicBool::new(false);
    let acked = AtomicU64::new(0);
    std::thread::scope(|scope| {
        for t in 0..writers {
            let (store, ns, stop, acked) = (&store, &ns, &stop, &acked);
            scope.spawn(move || {
                for i in 0u64.. {
                    if stop.load(Ordering::Relaxed) {
                        break;
                    }
                    let s = subject(ns, round, t, i);
                    store
                        .apply(&[], &[(&s, b">http://mf/p", b"\"v", None)])
                        .expect("child apply");
                    // Ack only after apply returned (post-fsync, Strict).
                    let mut out = std::io::stdout().lock();
                    writeln!(out, "acked {round} {t} {i}").expect("ack");
                    out.flush().expect("ack flush");
                    acked.fetch_add(1, Ordering::Relaxed);
                }
            });
        }

        if race {
            // Continuous merge (+ occasional GC) until the parent kills us.
            let (store, stop) = (&store, &stop);
            scope.spawn(move || {
                let mut n = 0u64;
                while !stop.load(Ordering::Relaxed) {
                    let snap = store.merge().expect("race merge");
                    let mut out = std::io::stdout().lock();
                    writeln!(out, "merged {}", snap.generation()).expect("merged");
                    out.flush().expect("merged flush");
                    if n % 4 == 3 {
                        store.gc();
                    }
                    n += 1;
                }
            });
        } else {
            // Matrix mode: one merge once commits are flowing. With a
            // failpoint armed this aborts mid-merge and never returns.
            while acked.load(Ordering::Relaxed) < 2 {
                std::thread::sleep(Duration::from_millis(1));
            }
            let snap = store.merge().expect("matrix merge");
            println!("merge-completed {}", snap.generation());
            std::io::stdout().flush().expect("flush");
            stop.store(true, Ordering::Relaxed);
        }
    });
}

// ---------------------------------------------------------------- parents

struct ChildRun {
    acks: Vec<(u64, u64, u64)>,
    ready: Option<(u64, u64)>,
    merged_gen: Option<u64>,
    status: std::process::ExitStatus,
}

/// Spawn the child, arm `kill_after` (delay from the first ack) when given,
/// and collect its output until exit.
fn run_child(
    dir: &Path,
    ns: &str,
    round: usize,
    failpoint: Option<&str>,
    race: bool,
    kill_after: Option<Duration>,
) -> ChildRun {
    let exe = std::env::current_exe().unwrap();
    let mut cmd = Command::new(&exe);
    cmd.args(["merge_fault_child", "--exact", "--nocapture"])
        .env("GRAPHY_MERGE_FAULT_DIR", dir)
        .env("GRAPHY_MERGE_FAULT_NS", ns)
        .env("GRAPHY_MERGE_FAULT_ROUND", round.to_string())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    if let Some(p) = failpoint {
        cmd.env("GRAPHY_FAILPOINT", p);
    }
    if race {
        cmd.env("GRAPHY_MERGE_FAULT_RACE", "1");
    }
    let mut child = cmd.spawn().expect("spawn child");
    let stdout = child.stdout.take().expect("child stdout");

    type Seen = (Vec<(u64, u64, u64)>, Option<(u64, u64)>, Option<u64>);
    let seen: Arc<Mutex<Seen>> = Arc::default();
    let reader = {
        let seen = Arc::clone(&seen);
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                let mut w = line.split_whitespace();
                let head = w.next();
                // Defensive parse: the final line can be torn mid-write.
                let nums: Vec<u64> = w.filter_map(|x| x.parse().ok()).collect();
                let mut g = seen.lock().unwrap();
                match (head, &nums[..]) {
                    (Some("acked"), &[r, t, i]) => g.0.push((r, t, i)),
                    (Some("ready"), &[e, gen]) => g.1 = Some((e, gen)),
                    (Some("merge-completed"), &[gen]) | (Some("merged"), &[gen]) => {
                        g.2 = Some(gen);
                    }
                    _ => {}
                }
            }
        })
    };

    if let Some(delay) = kill_after {
        let deadline = Instant::now() + Duration::from_secs(30);
        while seen.lock().unwrap().0.is_empty() {
            assert!(Instant::now() < deadline, "child produced no ack");
            std::thread::sleep(Duration::from_millis(1));
        }
        std::thread::sleep(delay);
        child.kill().expect("SIGKILL child");
    }
    let status = child.wait().expect("reap child");
    reader.join().expect("reader thread");
    let (acks, ready, merged_gen) = Arc::try_unwrap(seen)
        .expect("reader joined")
        .into_inner()
        .unwrap();
    ChildRun {
        acks,
        ready,
        merged_gen,
        status,
    }
}

fn setup(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "graphy-store-merge-fault-{}-{name}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    let mut cfg = BuilderConfig::new(&dir);
    cfg.profile = Profile::Balanced;
    let mut b = SegmentBuilder::new(cfg).unwrap();
    for i in 0..BASE_QUADS {
        let s = format!(">http://mf/base{i}").into_bytes();
        b.push_quad(&s, b">http://mf/p", b"\"base", None).unwrap();
    }
    b.finish().unwrap();
    dir
}

fn all_subjects(store: &Store) -> BTreeSet<Vec<u8>> {
    let snap = store.snapshot();
    let pat = snap.resolve_pattern(None, None, None, None).unwrap();
    let mut scan = snap.scan(&pat, Order::Spo).unwrap();
    let mut batch = QuadBatch::new();
    let mut out = BTreeSet::new();
    while scan.next_batch(&mut batch).unwrap() {
        for i in 0..batch.len() {
            out.insert(snap.decode_value(batch.s[i], TermPos::Subject).unwrap());
        }
    }
    out
}

/// Everything a recovery must satisfy, then prove the store fully usable
/// (a write and a full merge). Returns (epoch, generation) after those.
fn assert_recovered(
    dir: &Path,
    ns: &str,
    rounds_so_far: usize,
    min_epoch: u64,
    expect_gen: u64,
    strict_acked: &BTreeSet<(u64, u64, u64)>,
    label: &str,
) -> (u64, u64) {
    // Crash debris must not survive an open.
    let store = Store::open(dir).expect("recovery");
    for f in ["wal.log.tmp", "CURRENT.tmp"] {
        assert!(!dir.join(f).exists(), "{label}: {f} survived reopen");
    }

    let snap = store.snapshot();
    let epoch = snap.epoch();
    assert!(
        epoch >= min_epoch,
        "{label}: epoch regressed ({epoch} < {min_epoch})"
    );
    assert_eq!(snap.generation(), expect_gen, "{label}: wrong generation");

    // Committed prefix: E commits ⇒ E unique quads.
    let subjects = all_subjects(&store);
    assert_eq!(
        subjects.len() as u64,
        BASE_QUADS + epoch,
        "{label}: quads disagree with epoch"
    );
    // Durability: every Strict-acked commit survived.
    for &(r, t, i) in strict_acked {
        assert!(
            subjects.contains(&subject(ns, r, t, i)),
            "{label}: strict-acked commit lost: round {r} writer {t} #{i}"
        );
    }
    // Consistency: per-writer survival is prefix-closed.
    for r in 0..rounds_so_far as u64 {
        for t in 0..3 {
            let n = (0u64..)
                .take_while(|&i| subjects.contains(&subject(ns, r, t, i)))
                .count() as u64;
            let stray = (n..n + 50).find(|&i| subjects.contains(&subject(ns, r, t, i)));
            assert_eq!(
                stray, None,
                "{label}: round {r} writer {t} has a gap before a survivor"
            );
        }
    }

    // The live segment deep-verifies.
    let seg_dir = resolve_segment_dir(dir).unwrap();
    Segment::verify(&seg_dir).unwrap_or_else(|e| panic!("{label}: verify failed: {e}"));

    // Fully usable: take a write, then a full merge, and stay consistent.
    let post = format!(">http://mf/{ns}post{rounds_so_far}").into_bytes();
    let snap = store
        .apply(&[], &[(&post, b">http://mf/p", b"\"post", None)])
        .expect("write after recovery");
    let epoch = snap.epoch();
    drop(snap);
    let snap = store.merge().expect("merge after recovery");
    assert_eq!(snap.epoch(), epoch, "{label}: merge changed the epoch");
    let gen = snap.generation();
    assert_eq!(
        all_subjects(&store).len() as u64,
        BASE_QUADS + epoch,
        "{label}: post-merge quads disagree with epoch"
    );
    (epoch, gen)
}

/// Deterministic matrix: abort inside `merge_with` at each fsync-schedule
/// point; recovery lands on the expected side of the `CURRENT` flip.
#[test]
fn merge_failpoint_matrix() {
    // (failpoint, does the flip land before the abort?)
    let points: &[(Option<&str>, bool)] = &[
        (None, true), // control: full merge completes
        (Some("merge:start-logged"), false),
        (Some("merge:built"), false),
        (Some("merge:staged"), false),
        (Some("merge:flipped"), true),
        (Some("merge:activated"), true),
        (None, true), // control again on the accumulated store
    ];
    let dir = setup("matrix");
    let ns = "m";
    let mut strict_acked: BTreeSet<(u64, u64, u64)> = BTreeSet::new();
    let (mut epoch, mut gen) = (0u64, 0u64);

    for (round, &(point, flips)) in points.iter().enumerate() {
        let run = run_child(&dir, ns, round, point, false, None);
        let (child_epoch, child_gen) = run.ready.expect("ready line");
        assert_eq!((child_epoch, child_gen), (epoch, gen), "round {round}");
        strict_acked.extend(run.acks.iter().copied());

        let label = format!("round {round} ({})", point.unwrap_or("control"));
        if point.is_some() {
            // Aborted mid-merge (SIGABRT = 6), never completed.
            use std::os::unix::process::ExitStatusExt as _;
            assert_eq!(run.status.signal(), Some(6), "{label}: expected abort");
            assert_eq!(run.merged_gen, None, "{label}: merge reported success");
        } else {
            assert!(run.status.success(), "{label}: child failed");
            assert_eq!(run.merged_gen, Some(gen + 1), "{label}: bad merged gen");
        }

        let expect_gen = if flips { gen + 1 } else { gen };
        (epoch, gen) = assert_recovered(
            &dir,
            ns,
            round + 1,
            epoch,
            expect_gen,
            &strict_acked,
            &label,
        );
    }
    std::fs::remove_dir_all(&dir).ok();
}

/// Liveness + consistency of the swap-priority handoff: under sustained
/// write load a waiting merge swap takes leadership after at most one
/// in-flight commit group (`swap_waiting` — commits queue as followers
/// instead of barging, and a draining leader yields). Without the flag the
/// wait was only probabilistically bounded (the condvar waiter kept losing
/// to bargers while the queue stayed hot), which the doc 07 §6 swap-pause
/// target can't tolerate. This exercises the flag's deadlock-freedom and
/// checks nothing is lost across the swap.
#[test]
fn merge_completes_under_sustained_writes() {
    let dir = setup("starve");
    let store = Store::open(&dir).unwrap();
    let stop = AtomicBool::new(false);
    std::thread::scope(|scope| {
        for t in 0..2u64 {
            let (store, stop) = (&store, &stop);
            scope.spawn(move || {
                for i in 0u64.. {
                    if stop.load(Ordering::Relaxed) {
                        break;
                    }
                    let s = subject("s", 9, t, i);
                    store
                        .apply_with(
                            &[],
                            &[(&s, b">http://mf/p", b"\"v", None)],
                            graphy_store::Durability::Relaxed,
                        )
                        .unwrap();
                }
            });
        }
        let snap = store.merge().expect("merge under sustained load");
        assert_eq!(snap.generation(), 1);
        stop.store(true, Ordering::Relaxed);
    });
    // Everything the writers got in — before, during, and after the swap —
    // is present exactly once.
    let snap = store.snapshot();
    assert_eq!(all_subjects(&store).len() as u64, BASE_QUADS + snap.epoch());
    std::fs::remove_dir_all(&dir).ok();
}

/// Randomized race: SIGKILL a child running 3 writers against a continuous
/// merge loop, at varied delays; every recovery is a committed prefix.
#[test]
fn merge_kill9_race() {
    let dir = setup("race");
    let ns = "r";
    // Early kills land inside the first build/swap; later ones let several
    // merge cycles complete first, so kills sweep every internal phase.
    let delays_ms: &[u64] = &[10, 40, 120, 300, 800];
    let mut strict_acked: BTreeSet<(u64, u64, u64)> = BTreeSet::new();
    let (mut epoch, mut gen) = (0u64, 0u64);

    for (round, &delay) in delays_ms.iter().enumerate() {
        let run = run_child(
            &dir,
            ns,
            round,
            None,
            true,
            Some(Duration::from_millis(delay)),
        );
        let (child_epoch, child_gen) = run.ready.expect("ready line");
        assert_eq!(child_epoch, epoch, "round {round}: child epoch");
        assert!(child_gen >= gen, "round {round}: child generation");
        strict_acked.extend(run.acks.iter().copied());

        // Killed mid-flight: recovery may land on any generation ≥ the
        // child's start (its merge loop advances it), so learn it from
        // CURRENT rather than expecting one.
        let recovered_gen = {
            let seg = resolve_segment_dir(&dir).unwrap();
            Segment::open(&seg).unwrap().manifest.generation
        };
        assert!(recovered_gen >= child_gen, "round {round}: gen regressed");
        println!(
            "race round {round}: killed after {delay} ms / {} acks → gen {child_gen}→{recovered_gen}, epoch {epoch}→?",
            run.acks.len()
        );
        let label = format!("race round {round} (kill after {delay} ms)");
        (epoch, gen) = assert_recovered(
            &dir,
            ns,
            round + 1,
            epoch,
            recovered_gen,
            &strict_acked,
            &label,
        );
    }
    std::fs::remove_dir_all(&dir).ok();
}
