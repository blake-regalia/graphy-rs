//! Pipeline plan + execution (docs/09 §3). The unary prefix of the chain
//! replicates once per input ("legs"), joins at the junction (`concat` =
//! sequential, order-preserving; `merge` = concurrent legs on scoped
//! threads, arrival order), and the shared tail runs once. Single-input
//! pipelines run inline: no threads, no channels, no copies.

use std::fmt;
use std::fs::File;
use std::io;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Mutex};

use graphy_turtle::Options;

use crate::event::{Event, EventBatch, Flow, Sink};
use crate::ops::{chain, Head, Op, Skip, Tail, Tree, Unit};
use crate::sink::Out;
use crate::source::{read_stream, scan_stream, Format};

/// One unary operator (a fresh instance is built per leg).
#[derive(Debug, Clone, Copy)]
pub enum OpSpec {
    Skip { n: u64, unit: Unit },
    Head { n: u64, unit: Unit },
    Tail { n: u64, unit: Unit },
    Tree,
}

impl OpSpec {
    fn build(&self) -> Box<dyn Op> {
        match *self {
            OpSpec::Skip { n, unit } => Box::new(Skip::new(n, unit)),
            OpSpec::Head { n, unit } => Box::new(Head::new(n, unit)),
            OpSpec::Tail { n, unit } => Box::new(Tail::new(n, unit)),
            OpSpec::Tree => Box::new(Tree::new()),
        }
    }
}

fn build_ops(specs: &[OpSpec]) -> Vec<Box<dyn Op>> {
    specs.iter().map(OpSpec::build).collect()
}

/// The `read`/`scan` stage configuration.
#[derive(Debug, Clone, Default)]
pub struct SourceSpec {
    /// `None` = serial `read`; `Some(n)` = data-parallel `scan` with n
    /// workers (0 = one per CPU).
    pub par_threads: Option<usize>,
    /// Forced content type; `None` sniffs each input's extension (stdin
    /// defaults to TriG, matching the original).
    pub format: Option<Format>,
    pub base: Option<String>,
    /// `-r/--relax`: collect parse errors and resynchronize (warnings).
    pub lenient: bool,
    pub trusted: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Junction {
    Concat,
    Merge,
}

/// Terminal stage. `Write` covers the pretty serializers; `write -c nq/nt`
/// routes to `Scribe` at parse time (canonical is canonical).
#[derive(Debug, Clone, Copy)]
pub enum TerminalSpec {
    Scribe { triples_only: bool },
    Write { trig: bool },
    Count,
    Distinct { by: crate::sink::DistinctBy },
}

#[derive(Debug, Clone)]
pub enum Input {
    Stdin,
    File(PathBuf),
}

impl Input {
    fn display(&self) -> String {
        match self {
            Input::Stdin => "<stdin>".to_owned(),
            Input::File(p) => p.display().to_string(),
        }
    }
}

/// A validated pipeline. With no junction, every unary op lives in `before`
/// and `after` is empty (the planner guarantees it; `run` asserts).
#[derive(Debug, Clone)]
pub struct PipelineSpec {
    pub source: SourceSpec,
    pub before: Vec<OpSpec>,
    pub junction: Option<Junction>,
    pub after: Vec<OpSpec>,
    pub terminal: TerminalSpec,
}

/// Merge-leg channel depth (batches in flight per pipeline).
const MERGE_CHANNEL_DEPTH: usize = 4;

/// Leg terminal that borrows the shared downstream and defers its `finish`
/// to the junction owner; records whether the *shared* stage stopped (a
/// leg-local `head` stopping its own leg must not cancel other inputs).
struct Defer<'a> {
    inner: &'a mut dyn Sink,
    downstream_stopped: &'a AtomicBool,
}

impl fmt::Debug for Defer<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Defer")
    }
}

impl Sink for Defer<'_> {
    fn event(&mut self, ev: Event<'_>) -> io::Result<Flow> {
        let flow = self.inner.event(ev)?;
        if flow == Flow::Stop {
            self.downstream_stopped.store(true, Ordering::Relaxed);
        }
        Ok(flow)
    }

    fn finish(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Merge-leg terminal: batches events across the thread boundary. A send
/// failure means the junction hung up (downstream stopped) — report `Stop`
/// so the leg's source cancels its read loop.
struct BatchSender {
    tx: mpsc::SyncSender<EventBatch>,
    batch: EventBatch,
}

impl fmt::Debug for BatchSender {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("BatchSender")
    }
}

impl BatchSender {
    const ITEMS: usize = 1024;
    const BYTES: usize = 128 * 1024;

    fn ship(&mut self) -> Flow {
        if self.batch.is_empty() {
            return Flow::Continue;
        }
        match self.tx.send(std::mem::take(&mut self.batch)) {
            Ok(()) => Flow::Continue,
            Err(_) => Flow::Stop,
        }
    }
}

impl Sink for BatchSender {
    fn event(&mut self, ev: Event<'_>) -> io::Result<Flow> {
        self.batch.push(&ev);
        if self.batch.len() >= Self::ITEMS || self.batch.byte_len() >= Self::BYTES {
            return Ok(self.ship());
        }
        Ok(Flow::Continue)
    }

    fn finish(&mut self) -> io::Result<()> {
        let _ = self.ship();
        Ok(())
    }
}

/// Run one input through a freshly-built leg (source + `before` ops).
fn run_leg(
    spec: &PipelineSpec,
    input: &Input,
    index: usize,
    n_inputs: usize,
    sink: &mut dyn Sink,
    on_warn: &mut dyn FnMut(String),
) -> io::Result<Flow> {
    let options = Options {
        base: spec.source.base.clone(),
        lenient: spec.source.lenient,
        trusted: spec.source.trusted,
        // Blank labels are document-scoped: namespace per input (`f{i}…`)
        // whenever several inputs combine, per the M1 convention.
        label_ns: (n_inputs > 1).then_some(index as u128),
        ..Options::default()
    };
    let name = input.display();
    let with_path = |e: io::Error| io::Error::new(e.kind(), format!("{name}: {e}"));
    let format = match (spec.source.format, input) {
        (Some(f), _) => f,
        (None, Input::Stdin) => Format::Trig,
        (None, Input::File(p)) => Format::from_path(p).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{name}: cannot infer format from extension (use read -c)"),
            )
        })?,
    };
    let mut warn = |e: &graphy_turtle::ParseError| {
        on_warn(format!(
            "{name}: line {}, column {}: {}",
            e.line, e.column, e.message
        ));
    };
    match (spec.source.par_threads, input) {
        (None, Input::Stdin) => {
            let stdin = io::stdin();
            let mut lock = stdin.lock();
            read_stream(&mut lock, format, options, sink, &mut warn).map_err(with_path)
        }
        (None, Input::File(p)) => {
            let mut file = File::open(p).map_err(with_path)?;
            read_stream(&mut file, format, options, sink, &mut warn).map_err(with_path)
        }
        (Some(threads), Input::File(p)) => {
            let file = File::open(p).map_err(with_path)?;
            // SAFETY: mapping a file that changes underneath is undefined
            // behavior at the OS level. Pipeline inputs are assumed stable
            // for the run — the same assumption every streaming reader
            // makes, made explicit by the mapping (mirrors `load --threads`).
            let mmap = unsafe { memmap2::Mmap::map(&file) }.map_err(with_path)?;
            let threads = if threads == 0 {
                std::thread::available_parallelism().map_or(1, |n| n.get())
            } else {
                threads
            };
            scan_stream(&mmap, format, options, threads, sink).map_err(with_path)
        }
        (Some(_), Input::Stdin) => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "scan requires file inputs (stdin cannot be memory-mapped); use read",
        )),
    }
}

fn build_terminal(spec: &TerminalSpec, out: Out) -> Box<dyn Sink> {
    match *spec {
        TerminalSpec::Scribe { triples_only } => {
            Box::new(crate::sink::NqSink::new(out, triples_only))
        }
        TerminalSpec::Write { trig } => Box::new(crate::sink::PrettySink::new(out, trig)),
        TerminalSpec::Count => Box::new(crate::sink::CountSink::new(out)),
        TerminalSpec::Distinct { by } => Box::new(crate::sink::DistinctSink::new(by, out)),
    }
}

/// Execute a pipeline over `inputs`, writing terminal output to `out`.
/// Warnings (lenient-mode parse errors) go to `on_warn` in input order.
pub fn run(
    spec: &PipelineSpec,
    inputs: &[Input],
    out: Out,
    on_warn: &mut dyn FnMut(String),
) -> io::Result<()> {
    assert!(!inputs.is_empty(), "planner supplies at least stdin");
    let terminal = build_terminal(&spec.terminal, out);
    // wasm32 has no std threads. merge's contract is arrival order —
    // unspecified — so the sequential concat schedule is a legal merge.
    let junction = if cfg!(target_arch = "wasm32") && spec.junction == Some(Junction::Merge) {
        Some(Junction::Concat)
    } else {
        spec.junction
    };
    match junction {
        None => {
            debug_assert!(inputs.len() == 1 && spec.after.is_empty());
            let mut all = chain(build_ops(&spec.before), terminal);
            run_leg(spec, &inputs[0], 0, inputs.len(), &mut *all, on_warn)?;
            all.finish()
        }
        Some(Junction::Concat) => {
            let mut tail = chain(build_ops(&spec.after), terminal);
            let downstream_stopped = AtomicBool::new(false);
            for (i, input) in inputs.iter().enumerate() {
                let mut leg = chain(
                    build_ops(&spec.before),
                    Box::new(Defer {
                        inner: &mut *tail,
                        downstream_stopped: &downstream_stopped,
                    }),
                );
                run_leg(spec, input, i, inputs.len(), &mut *leg, on_warn)?;
                // Flush the leg's buffered ops (tail/tree) into the shared
                // downstream before the next input starts.
                leg.finish()?;
                if downstream_stopped.load(Ordering::Relaxed) {
                    break;
                }
            }
            tail.finish()
        }
        Some(Junction::Merge) => {
            let mut tail = chain(build_ops(&spec.after), terminal);
            let mut first_err: Option<io::Error> = None;
            let warnings: Mutex<Vec<String>> = Mutex::new(Vec::new());
            let n_inputs = inputs.len();
            std::thread::scope(|scope| {
                let (tx, rx) = mpsc::sync_channel::<EventBatch>(MERGE_CHANNEL_DEPTH);
                let handles: Vec<_> = inputs
                    .iter()
                    .enumerate()
                    .map(|(i, input)| {
                        let tx = tx.clone();
                        let warnings = &warnings;
                        scope.spawn(move || -> io::Result<()> {
                            let mut leg = chain(
                                build_ops(&spec.before),
                                Box::new(BatchSender {
                                    tx,
                                    batch: EventBatch::default(),
                                }),
                            );
                            let mut warn = |w: String| {
                                warnings.lock().expect("warning lock").push(w);
                            };
                            run_leg(spec, input, i, n_inputs, &mut *leg, &mut warn)?;
                            leg.finish()
                        })
                    })
                    .collect();
                drop(tx);
                'outer: while let Ok(batch) = rx.recv() {
                    for ev in batch.events() {
                        match tail.event(ev) {
                            Ok(Flow::Continue) => {}
                            Ok(Flow::Stop) => break 'outer,
                            Err(e) => {
                                first_err = Some(e);
                                break 'outer;
                            }
                        }
                    }
                }
                // Hang up: pending sends fail, legs cancel their sources.
                drop(rx);
                for handle in handles {
                    match handle.join().expect("leg threads do not panic") {
                        Err(e) if first_err.is_none() => first_err = Some(e),
                        _ => {}
                    }
                }
            });
            for w in warnings.into_inner().expect("warning lock") {
                on_warn(w);
            }
            if let Some(e) = first_err {
                return Err(e);
            }
            tail.finish()
        }
    }
}
