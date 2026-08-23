//! Data-parallel N-Triples / N-Quads parsing (doc 03 §4.1).
//!
//! NT/NQ statements never span a newline (raw `\n`/`\r` are forbidden in
//! literals, there are no long strings, comments end at newline), so the
//! input splits **exactly**: pick fractional byte offsets, advance each to
//! the next newline, and run one independent sans-io parser per segment —
//! no speculation, no re-parsing, and no changes to the serial hot path.
//!
//! Blank-node labels are file-scoped, so workers use **content-derived**
//! internal labels (`s{surface}` instead of first-seen `b{n}`): output is
//! deterministic across thread counts and schedules, and isomorphic (not
//! byte-equal) to a serial parse of the same input.
//!
//! The quad callback runs concurrently from every worker, tagged with the
//! segment index; segment indexes follow document order. Errors fail fast
//! (other workers stop at their next chunk) and are reported with
//! **global** offset/line positions — the correction pass over the segment
//! prefix runs only on the error path.

/// Thread backend switch (docs/11 §6): std natively; `wasm_thread`
/// (web-worker backed) under the `wasm-threads` build, so the data-parallel
/// parse runs in the browser too. Callers on wasm without the feature must
/// pass `threads <= 1` (the serial parsers) — std spawn traps there.
mod graphy_thread {
    #[cfg(not(all(target_arch = "wasm32", feature = "wasm-threads")))]
    pub(super) use std::thread::scope;
    #[cfg(all(target_arch = "wasm32", feature = "wasm-threads"))]
    pub(super) use wasm_thread::scope;
}

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

use memchr::memchr;

use crate::{NQuadsParser, NTriplesParser, Options, ParseError, QuadRef};

/// Worker feed granularity: small enough for prompt cancellation, large
/// enough that per-feed overhead vanishes.
const CHUNK: usize = 256 * 1024;

/// Parse N-Quads from `data` using up to `threads` workers. `on_quad` is
/// invoked concurrently as `(segment_index, quad)`; segment indexes are in
/// document order. See the module docs for the blank-label convention.
pub fn nquads<F>(
    data: &[u8],
    options: &Options,
    threads: usize,
    on_quad: F,
) -> Result<(), ParseError>
where
    F: Fn(usize, QuadRef<'_>) + Sync,
{
    run::<NQuadsParser, F>(data, options, threads, &on_quad)
}

/// N-Triples variant of [`nquads`].
pub fn ntriples<F>(
    data: &[u8],
    options: &Options,
    threads: usize,
    on_quad: F,
) -> Result<(), ParseError>
where
    F: Fn(usize, QuadRef<'_>) + Sync,
{
    run::<NTriplesParser, F>(data, options, threads, &on_quad)
}

/// The slice of parser API the worker loop needs (both wrappers expose it,
/// but through a macro rather than a trait — this closes the gap privately).
trait NxLike: Sized {
    fn build(options: Options) -> Result<Self, ParseError>;
    fn feed_chunk(&mut self, chunk: &[u8]) -> Result<(), ParseError>;
    fn finish_input(&mut self) -> Result<(), ParseError>;
    fn drain_into(&mut self, seg: usize, f: &dyn Fn(usize, QuadRef<'_>));
}

macro_rules! nx_like {
    ($ty:ty) => {
        impl NxLike for $ty {
            fn build(options: Options) -> Result<Self, ParseError> {
                let mut p = <$ty>::new(options)?;
                p.set_content_labels();
                Ok(p)
            }

            fn feed_chunk(&mut self, chunk: &[u8]) -> Result<(), ParseError> {
                self.feed(chunk)
            }

            fn finish_input(&mut self) -> Result<(), ParseError> {
                self.finish()
            }

            fn drain_into(&mut self, seg: usize, f: &dyn Fn(usize, QuadRef<'_>)) {
                for q in self.drain() {
                    f(seg, q);
                }
            }
        }
    };
}

nx_like!(NQuadsParser);
nx_like!(NTriplesParser);

/// Segment boundaries: fractional offsets advanced to just past the next
/// newline (exact statement starts); strictly increasing by construction.
fn boundaries(data: &[u8], threads: usize) -> Vec<usize> {
    let mut cuts = vec![0];
    for i in 1..threads {
        let target = data.len() * i / threads;
        let cut = match memchr(b'\n', &data[target..]) {
            Some(j) => target + j + 1,
            None => data.len(),
        };
        if cut > *cuts.last().expect("nonempty") && cut < data.len() {
            cuts.push(cut);
        }
    }
    cuts.push(data.len());
    cuts
}

fn run<P, F>(data: &[u8], options: &Options, threads: usize, on_quad: &F) -> Result<(), ParseError>
where
    P: NxLike,
    F: Fn(usize, QuadRef<'_>) + Sync,
{
    let cuts = boundaries(data, threads.max(1));
    let failed = AtomicBool::new(false);
    // Lowest-global-offset error wins: invalid input yields a stable,
    // earliest-known diagnostic regardless of worker timing.
    let first_err: Mutex<Option<(u64, ParseError)>> = Mutex::new(None);

    graphy_thread::scope(|scope| {
        for (seg, w) in cuts.windows(2).enumerate() {
            let (lo, hi) = (w[0], w[1]);
            let (failed, first_err) = (&failed, &first_err);
            scope.spawn(move || {
                if let Err(e) = segment::<P, F>(data, lo, hi, seg, options, on_quad, failed) {
                    record(first_err, failed, data, lo, e);
                }
            });
        }
    });

    match first_err.into_inner().expect("no poisoned workers") {
        Some((_, e)) => Err(e),
        None => Ok(()),
    }
}

/// Parse one segment, draining to the callback and honoring cancellation
/// between chunks.
fn segment<P, F>(
    data: &[u8],
    lo: usize,
    hi: usize,
    seg: usize,
    options: &Options,
    on_quad: &F,
    failed: &AtomicBool,
) -> Result<(), ParseError>
where
    P: NxLike,
    F: Fn(usize, QuadRef<'_>) + Sync,
{
    let mut p = P::build(options.clone())?;
    for chunk in data[lo..hi].chunks(CHUNK) {
        if failed.load(Ordering::Relaxed) {
            return Ok(());
        }
        p.feed_chunk(chunk)?;
        p.drain_into(seg, &|s, q| on_quad(s, q));
    }
    p.finish_input()?;
    p.drain_into(seg, &|s, q| on_quad(s, q));
    Ok(())
}

/// Correct a worker error's segment-relative positions to global ones
/// (offset shift; line via a newline count over the prefix — segments start
/// at line starts, so columns are already global) and keep the lowest-offset
/// error.
fn record(
    first_err: &Mutex<Option<(u64, ParseError)>>,
    failed: &AtomicBool,
    data: &[u8],
    lo: usize,
    mut e: ParseError,
) {
    failed.store(true, Ordering::Relaxed);
    let prefix_lines = memchr::memchr_iter(b'\n', &data[..lo]).count() as u64;
    e.offset += lo as u64;
    e.line += prefix_lines;
    let mut slot = first_err.lock().expect("no poisoned workers");
    if slot.as_ref().is_none_or(|(off, _)| e.offset < *off) {
        *slot = Some((e.offset, e));
    }
}
