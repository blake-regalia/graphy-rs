//! Background merge scheduler (M5, doc 07 §6.4): a thread that watches the
//! store's pressure signals and runs [`Store::merge_with`] when one fires.
//!
//! Triggers, evaluated every [`SchedulerConfig::interval`] (and immediately
//! on [`MergeScheduler::request_merge`]):
//! - **soft budget** — the store's sticky [`Store::needs_merge`] signal;
//! - **hard pressure** — delta events at/over the hard budget (writers may
//!   already be blocked on the backpressure gate); always on;
//! - **tombstone ratio** — delta tombstones vs. base quads (the
//!   read-amplification guard: every tombstone is zipper work on scans);
//! - **explicit** — `request_merge()` runs one merge unconditionally (the
//!   library-level `graphy compact`).
//!
//! While a scheduler is attached, hard-budget pressure means writers *wait*
//! for the merge (bounded by [`Store::set_backpressure_timeout`]) instead
//! of failing — doc 07 §6.4's predictable degradation. Detaching (drop)
//! restores fail-fast and wakes any gated writer. Merge errors are recorded
//! ([`MergeScheduler::last_error`]) and retried at the next trigger; the
//! self-pacing feedback loop (read-p99-driven bandwidth limits) is deferred
//! to the M5 SLO runs.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use crate::store::{MergeConfig, Store};

/// Knobs for [`MergeScheduler::spawn`].
#[derive(Debug, Clone)]
pub struct SchedulerConfig {
    /// Trigger-evaluation cadence (also the retry backoff after a failed
    /// merge).
    pub interval: Duration,
    /// Merge when the store signals [`Store::needs_merge`] (soft budget).
    pub on_soft_budget: bool,
    /// Merge when `delta tombstones ≥ ratio × base quads`. `None` disables.
    pub tombstone_ratio: Option<f64>,
    /// Settings handed to every [`Store::merge_with`] run.
    pub merge: MergeConfig,
}

impl Default for SchedulerConfig {
    fn default() -> SchedulerConfig {
        SchedulerConfig {
            interval: Duration::from_millis(200),
            on_soft_budget: true,
            tombstone_ratio: Some(0.10),
            merge: MergeConfig::default(),
        }
    }
}

/// Handle to the background merge thread. Dropping it shuts the thread
/// down cleanly (any in-flight merge completes first) and detaches the
/// merger from the store.
#[derive(Debug)]
pub struct MergeScheduler {
    shared: Arc<Shared>,
    thread: Option<std::thread::JoinHandle<()>>,
}

#[derive(Debug, Default)]
struct Shared {
    stop: AtomicBool,
    /// Explicit merge request pending (kicks the trigger loop).
    kicked: Mutex<bool>,
    cv: Condvar,
    merges: AtomicU64,
    last_error: Mutex<Option<String>>,
}

impl MergeScheduler {
    /// Attach a background merger to `store` and start its trigger loop.
    /// One merger per store — a second spawn panics (programming error).
    pub fn spawn(store: Arc<Store>, cfg: SchedulerConfig) -> MergeScheduler {
        store.attach_merger();
        let shared = Arc::new(Shared::default());
        let thread = {
            let shared = Arc::clone(&shared);
            std::thread::Builder::new()
                .name("graphy-merge".into())
                .spawn(move || run(&store, &cfg, &shared))
                .expect("spawn merge scheduler thread")
        };
        MergeScheduler {
            shared,
            thread: Some(thread),
        }
    }

    /// Run one merge as soon as possible, regardless of triggers (the
    /// library-level `graphy compact`). Returns immediately; watch
    /// [`MergeScheduler::merges_completed`] or the store's generation.
    pub fn request_merge(&self) {
        *self.shared.kicked.lock().expect("scheduler kick") = true;
        self.shared.cv.notify_all();
    }

    /// Merges completed by this scheduler so far.
    pub fn merges_completed(&self) -> u64 {
        self.shared.merges.load(Ordering::Relaxed)
    }

    /// The most recent merge error, if any (sticky until the next success).
    pub fn last_error(&self) -> Option<String> {
        self.shared
            .last_error
            .lock()
            .expect("scheduler error")
            .clone()
    }
}

impl Drop for MergeScheduler {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::Relaxed);
        self.shared.cv.notify_all();
        if let Some(t) = self.thread.take() {
            t.join().expect("merge scheduler thread");
        }
    }
}

fn run(store: &Store, cfg: &SchedulerConfig, shared: &Shared) {
    loop {
        // Sleep until the interval elapses or an explicit kick arrives.
        let explicit = {
            let mut k = shared.kicked.lock().expect("scheduler kick");
            if !*k && !shared.stop.load(Ordering::Relaxed) {
                k = shared.cv.wait_timeout(k, cfg.interval).expect("kick").0;
            }
            std::mem::take(&mut *k)
        };
        if shared.stop.load(Ordering::Relaxed) {
            break;
        }
        // Merge while pressure remains (a merge can re-arm needs_merge from
        // its own active suffix); an explicit kick merges at least once.
        let mut forced = explicit;
        while (forced || should_merge(store, cfg)) && !shared.stop.load(Ordering::Relaxed) {
            forced = false;
            match store.merge_with(&cfg.merge) {
                Ok(_) => {
                    shared.merges.fetch_add(1, Ordering::Relaxed);
                    *shared.last_error.lock().expect("scheduler error") = None;
                }
                Err(e) => {
                    // Record and retry at the next trigger evaluation; a
                    // poisoned handle keeps failing here, harmlessly.
                    *shared.last_error.lock().expect("scheduler error") = Some(e.to_string());
                    break;
                }
            }
        }
    }
    store.detach_merger();
}

fn should_merge(store: &Store, cfg: &SchedulerConfig) -> bool {
    let snap = store.snapshot();
    let events = snap.delta_events();
    if events == 0 {
        return false;
    }
    // Hard pressure is never configurable away: gated writers depend on it.
    let (_, hard) = store.delta_budget();
    if events >= hard {
        return true;
    }
    if cfg.on_soft_budget && store.needs_merge() {
        return true;
    }
    if let Some(ratio) = cfg.tombstone_ratio {
        let base = snap.segment().manifest.counts.quads.max(1);
        if snap.delta_tombstones() as f64 >= ratio * base as f64 {
            return true;
        }
    }
    false
}
