//! Kill-9 fault injection (M4 exit criterion, doc 07 §4): a child process
//! commits continuously (3 concurrent writers → real group commits) and is
//! SIGKILLed at varied points; every recovery must land on a committed
//! prefix. The parent asserts, per round:
//!
//! - **Durability** (`Strict` rounds): every commit the child acknowledged
//!   (ack printed only after `apply` returned, i.e. post-fsync) survives.
//! - **Consistency** (all rounds, incl. `Relaxed` where the OS-buffered
//!   tail may be lost): recovered epoch `E` ⇒ exactly `E` writes survive
//!   (each commit adds exactly one unique quad), per-writer survival is
//!   prefix-closed (WAL order preserves each thread's commit order), and
//!   epochs never regress across rounds.
//!
//! The child is the hidden `kill9_child_writer` "test" below, re-invoked
//! from the parent via `--exact` — a no-op unless `GRAPHY_KILL9_DIR` is set.
//! This covers process-kill *timing*; file-level torn-tail/checksum damage
//! is covered by tests/wal.rs.

use std::collections::BTreeSet;
use std::io::{BufRead, BufReader, Write as _};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use graphy_store::{
    BuilderConfig, Durability, Order, Profile, QuadBatch, SegmentBuilder, Store, TermPos,
};

const WRITER_THREADS: u64 = 3;
const BASE_QUADS: u64 = 3;

fn subject(round: u64, thread: u64, i: u64) -> Vec<u8> {
    format!(">http://k/r{round}t{thread}i{i}").into_bytes()
}

/// The child's writer loop. A no-op under a normal `cargo test` run; the
/// parent re-invokes this binary with `GRAPHY_KILL9_DIR` set.
#[test]
fn kill9_child_writer() {
    let Ok(dir) = std::env::var("GRAPHY_KILL9_DIR") else {
        return;
    };
    let round: u64 = std::env::var("GRAPHY_KILL9_ROUND")
        .expect("round set")
        .parse()
        .expect("round number");
    let durability = if std::env::var("GRAPHY_KILL9_RELAXED").is_ok() {
        Durability::Relaxed
    } else {
        Durability::Strict
    };
    let store = Store::open(Path::new(&dir)).expect("child open");
    println!("ready {}", store.snapshot().epoch());
    std::io::stdout().flush().expect("flush");

    std::thread::scope(|scope| {
        for t in 0..WRITER_THREADS {
            let store = &store;
            scope.spawn(move || {
                for i in 0u64.. {
                    let s = subject(round, t, i);
                    store
                        .apply_with(&[], &[(&s, b">http://k/p", b"\"v", None)], durability)
                        .expect("child apply");
                    // Ack only after apply returned (post-fsync in Strict).
                    let mut out = std::io::stdout().lock();
                    writeln!(out, "acked {round} {t} {i}").expect("ack");
                    out.flush().expect("ack flush");
                }
            });
        }
    });
}

fn full_dump(store: &Store) -> BTreeSet<Vec<u8>> {
    let snap = store.snapshot();
    let pat = snap.resolve_pattern(None, None, None, None).unwrap();
    let mut scan = snap.scan(&pat, Order::Spo).unwrap();
    let mut batch = QuadBatch::new();
    let mut subjects = BTreeSet::new();
    while scan.next_batch(&mut batch).unwrap() {
        for i in 0..batch.len() {
            subjects.insert(snap.decode_value(batch.s[i], TermPos::Subject).unwrap());
        }
    }
    subjects
}

#[test]
fn kill9_recovers_to_a_committed_prefix() {
    let dir: PathBuf =
        std::env::temp_dir().join(format!("graphy-store-kill9-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let mut cfg = BuilderConfig::new(&dir);
    cfg.profile = Profile::Balanced;
    let mut b = SegmentBuilder::new(cfg).unwrap();
    for i in 0..BASE_QUADS {
        let s = format!(">http://k/base{i}").into_bytes();
        b.push_quad(&s, b">http://k/p", b"\"base", None).unwrap();
    }
    b.finish().unwrap();

    // (delay after first signal, kill on "ready" vs first ack, relaxed).
    // Early kills target open/recovery and the first commits; later ones
    // land mid-stream with all three writers grouping.
    let rounds: &[(u64, bool, bool)] = &[
        (0, true, false),  // kill right at ready: recovery-of-recovery
        (5, false, false), // strict, just past the first ack
        (25, false, false),
        (25, false, true), // relaxed: consistency only
        (80, false, false),
    ];

    let exe = std::env::current_exe().unwrap();
    let mut last_epoch = 0u64;
    let mut strict_acked: Vec<(u64, u64, u64)> = Vec::new();
    for (round, &(delay_ms, kill_at_ready, relaxed)) in rounds.iter().enumerate() {
        let mut cmd = Command::new(&exe);
        cmd.args(["kill9_child_writer", "--exact", "--nocapture"])
            .env("GRAPHY_KILL9_DIR", &dir)
            .env("GRAPHY_KILL9_ROUND", round.to_string())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        if relaxed {
            cmd.env("GRAPHY_KILL9_RELAXED", "1");
        }
        let mut child = cmd.spawn().expect("spawn child");
        let stdout = child.stdout.take().expect("child stdout");

        let acks: Arc<Mutex<Vec<(u64, u64, u64)>>> = Arc::default();
        let ready = Arc::new(AtomicBool::new(false));
        let reader = {
            let (acks, ready) = (Arc::clone(&acks), Arc::clone(&ready));
            std::thread::spawn(move || {
                for line in BufReader::new(stdout).lines() {
                    let Ok(line) = line else { break };
                    let mut w = line.split_whitespace();
                    match w.next() {
                        Some("ready") => ready.store(true, Ordering::Release),
                        Some("acked") => {
                            // Defensive parse: the final line can be torn.
                            let f: Vec<u64> = w.filter_map(|x| x.parse().ok()).collect();
                            if let [r, t, i] = f[..] {
                                acks.lock().unwrap().push((r, t, i));
                            }
                        }
                        _ => {}
                    }
                }
            })
        };

        // Arm on the requested signal, then let it run for `delay_ms`.
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            let armed = if kill_at_ready {
                ready.load(Ordering::Acquire)
            } else {
                !acks.lock().unwrap().is_empty()
            };
            if armed {
                break;
            }
            assert!(Instant::now() < deadline, "child produced no signal");
            std::thread::sleep(Duration::from_millis(1));
        }
        std::thread::sleep(Duration::from_millis(delay_ms));
        child.kill().expect("SIGKILL child"); // SIGKILL on unix
        child.wait().expect("reap child");
        reader.join().expect("reader thread");

        // Recover and validate.
        let store = Store::open(&dir).expect("recovery");
        let epoch = store.snapshot().epoch();
        assert!(
            epoch >= last_epoch,
            "epoch regressed: {epoch} < {last_epoch} (round {round})"
        );
        let subjects = full_dump(&store);
        // Every commit adds exactly one unique quad: E commits ⇒ E quads.
        assert_eq!(
            subjects.len() as u64,
            BASE_QUADS + epoch,
            "recovered quads disagree with recovered epoch (round {round})"
        );
        // Durability: everything acked under Strict must have survived.
        if !relaxed {
            strict_acked.extend(acks.lock().unwrap().iter().copied());
        }
        for &(r, t, i) in &strict_acked {
            assert!(
                subjects.contains(&subject(r, t, i)),
                "strict-acked commit lost: round {r} writer {t} #{i}"
            );
        }
        // Consistency: per-writer survival is prefix-closed.
        for r in 0..=round as u64 {
            for t in 0..WRITER_THREADS {
                let n = (0u64..)
                    .take_while(|&i| subjects.contains(&subject(r, t, i)))
                    .count() as u64;
                let stray = (n..n + 50).find(|&i| subjects.contains(&subject(r, t, i)));
                assert_eq!(
                    stray,
                    None,
                    "round {r} writer {t}: gap before surviving commit #{}",
                    stray.unwrap_or(0)
                );
            }
        }
        println!(
            "round {round}: killed after {} acks → recovered epoch {epoch}",
            acks.lock().unwrap().len()
        );
        // Store must be writable after every recovery.
        let s = format!(">http://k/post{round}").into_bytes();
        let snap = store
            .apply(&[], &[(&s, b">http://k/p", b"\"post", None)])
            .expect("write after recovery");
        last_epoch = snap.epoch();
    }
    std::fs::remove_dir_all(&dir).ok();
}
