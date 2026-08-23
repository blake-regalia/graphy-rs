//! `graphy` — bulk load, verify, and export segments (M2 surface; the
//! server subcommands arrive with M9). Run `graphy --help` or
//! `graphy help <command>` for the full interface.

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Instant;

use graphy_store::{
    resolve_segment_dir, BuilderConfig, MergeConfig, OpenMode, Pattern, Profile, Segment,
    SegmentBuilder, Store, TermPos,
};
use graphy_turtle::{NQuadsParser, NTriplesParser, Options, QuadRef, TriGParser, TurtleParser};

mod pipeline;

const HELP: &str = "\
graphy — RDF quad-store segment tool (parse, pipeline, load, verify, export)

USAGE:
    graphy <COMMAND> [ARGS]
    graphy <PIPE-COMMAND> [ARGS] [ / <PIPE-COMMAND> [ARGS] ]... [--inputs <FILE>...]

STORE COMMANDS:
    load      Parse RDF files and build an immutable base segment
    verify    Check a segment's checksums and structural invariants
    export    Write a segment back out as canonical N-Quads
    compact   Fold a store's delta/WAL into a new base generation
    query     Run a SPARQL query against a store (local, no server)
    serve     Serve a store over the SPARQL 1.1 Protocol (HTTP)
    help      Show help for a command (same as <COMMAND> --help)

PIPELINE COMMANDS (chained with `/`; quads flow in memory between stages):
    read, scan            deserialize        [details: graphy help pipeline]
    scribe, write         serialize
    skip, head, tail      slice the stream
    tree                  dedup + regroup for pretty output
    concat, merge         join several inputs
    count, distinct       result values

OPTIONS:
    -h, --help       Show this help
    -V, --version    Show version

Formats are chosen by file extension: .nt/.ntriples, .nq/.nquads,
.ttl/.turtle, .trig (RDF 1.1 + 1.2; all 1045 W3C rdf-tests pass), and
.hdt/.hdtq (binary import — no parsing; single-file loads skip
re-interning entirely).
See DEMO.md for a worked walkthrough.
";

const HELP_LOAD: &str = "\
graphy load — parse RDF files and build an immutable base segment

USAGE:
    graphy load <out-dir> <input>... [OPTIONS]

ARGS:
    <out-dir>     Segment directory to create (must not already hold one)
    <input>...    One or more .nt/.nq/.ttl/.trig/.hdt/.hdtq files (mixable)

OPTIONS:
    --profile <compact|balanced|covering>
                  Index profile [default: balanced]
                    compact   = SPO + FoQ accessors (smallest)
                    balanced  = SPO, POS, OSP (general serving)
                    covering  = all six orderings (analytical)
    --base <iri>  Base IRI for resolving relative references in Turtle/TriG
                  (N-Triples/N-Quads require absolute IRIs and ignore it)
    --sort-budget <MiB>
                  External-sort memory budget [default: 256]
    --intern-budget <MiB>
                  Intern memory budget: switches to a two-pass load that
                  spills terms and quads to disk instead of holding an
                  id table (output identical) [default: unbounded]
    --trusted     Skip character-level validation for input known to be
                  syntactically valid (previously validated dumps, this
                  tool's own exports). ~30% faster on N-Quads. Invalid
                  input may be accepted or misparsed instead of rejected —
                  never unsafely
    --threads <n> Parse and intern with n parallel workers (0 = one per
                  CPU) [default: 1]. Applies to .nt/.nq inputs (statements
                  never span newlines, so files split exactly); Turtle/TriG
                  inputs stream through a single lane. Parallel loads use
                  content-derived blank labels, so they are isomorphic (not
                  byte-identical) to serial loads of the same data
    -h, --help    Show this help

Blank labels are document-scoped: with more than one input file, each
file's labels get a distinct namespace (f0…, f1…, …) so identical surface
labels never unify across files.
";

const HELP_VERIFY: &str = "\
graphy verify — check a segment's checksums and structural invariants

USAGE:
    graphy verify <segment-dir>

Deep verification: every component digest against the manifest, index order
walks, graph-layer cross-checks, dictionary sidecar lookups, FoQ
permutations. Any flipped byte fails. A store directory resolves through
its CURRENT pointer to the live generation; write-ahead-log contents are
not covered (compact first to fold them into the verified base).

OPTIONS:
    -h, --help    Show this help
";

const HELP_EXPORT: &str = "\
graphy export — write a segment back out (N-Quads, HDT, or HDTQ)

USAGE:
    graphy export <segment-dir> [OPTIONS]

OPTIONS:
    -o <file>     Write to a file instead of stdout
    --format <nq|hdt|hdtq>
                  Output format [default: nq]. hdt = the standard binary
                  triples format (graph components drop — the triples
                  view); hdtq = quads via the qEndpoint HDTQ dialect
                  (graphs dictionary + per-graph triple annotations)
    --mmap        Open the segment via memory-mapped zero-copy views
                  instead of heap reads (headers validated; payload digests
                  are verify's job)
    -h, --help    Show this help

A store directory (one with a write-ahead log) resolves through its
CURRENT pointer and exports the full committed state — the live generation
plus any not-yet-compacted writes. A bare segment directory exports as-is.
";

const HELP_COMPACT: &str = "\
graphy compact — fold a store's delta/WAL into a new base generation

USAGE:
    graphy compact <store-dir> [OPTIONS]

Rebuilds the base segment with every committed write folded in (doc 07 §6:
the same deterministic builders as load), placed in a gen-NNNNNN/ directory
under the store root with the CURRENT pointer flipped atomically; the
write-ahead log rotates down to a checkpoint. Old generation files are
removed once no reader references them. A store that is merely loaded (no
writes) has nothing to fold and is left untouched — unless --profile asks
for a different profile, which always rebuilds.

OPTIONS:
    --profile <compact|balanced|covering>
                  Rebuild into this storage profile (doc 07 §6.4 profile
                  change) [default: keep the store's current profile and
                  its materialized orderings]
    --sort-budget <MiB>
                  External-sort memory budget [default: 256]
    -h, --help    Show this help
";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let result = match args.first().map(String::as_str) {
        Some("load") => cmd_load(&args[1..]),
        Some("verify") => cmd_verify(&args[1..]),
        Some("export") => cmd_export(&args[1..]),
        Some("compact") => cmd_compact(&args[1..]),
        Some("query") => cmd_query(&args[1..]),
        Some("serve") => cmd_serve(&args[1..]),
        Some(v) if pipeline::is_pipeline_verb(v) => pipeline::cmd_pipeline(&args),
        Some("help") => {
            let text = match args.get(1).map(String::as_str) {
                Some("load") => HELP_LOAD,
                Some("verify") => HELP_VERIFY,
                Some("export") => HELP_EXPORT,
                Some("compact") => HELP_COMPACT,
                Some("query") => HELP_QUERY,
                Some("serve") => HELP_SERVE,
                None => HELP,
                Some(other) => match pipeline::help_for(other) {
                    Some(text) => text,
                    None => {
                        eprintln!("graphy: unknown command {other:?}\n\n{HELP}");
                        return ExitCode::from(2);
                    }
                },
            };
            print!("{text}");
            return ExitCode::SUCCESS;
        }
        Some("-h" | "--help") => {
            print!("{HELP}");
            return ExitCode::SUCCESS;
        }
        Some("-V" | "--version") => {
            println!("graphy {}", env!("CARGO_PKG_VERSION"));
            return ExitCode::SUCCESS;
        }
        Some(other) => {
            eprintln!("graphy: unknown command {other:?}\n\n{HELP}");
            return ExitCode::from(2);
        }
        None => {
            eprint!("{HELP}");
            return ExitCode::from(2);
        }
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(Failure::Usage(msg, help)) => {
            eprintln!("graphy: {msg}\n\n{help}");
            ExitCode::from(2)
        }
        Err(Failure::Run(msg)) => {
            eprintln!("graphy: {msg}");
            ExitCode::FAILURE
        }
    }
}

/// Usage errors (exit 2, help appended) vs. runtime failures (exit 1).
pub(crate) enum Failure {
    Usage(String, &'static str),
    Run(String),
}

impl From<String> for Failure {
    fn from(msg: String) -> Failure {
        Failure::Run(msg)
    }
}

impl From<&str> for Failure {
    fn from(msg: &str) -> Failure {
        Failure::Run(msg.to_owned())
    }
}

fn cmd_load(args: &[String]) -> Result<(), Failure> {
    let usage = |msg: String| Failure::Usage(msg, HELP_LOAD);
    let mut inputs: Vec<PathBuf> = Vec::new();
    let mut out: Option<PathBuf> = None;
    let mut profile = Profile::Balanced;
    let mut base: Option<String> = None;
    let mut sort_budget = 256usize << 20;
    let mut intern_budget: Option<usize> = None;
    let mut trusted = false;
    let mut threads = 1usize;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "-h" | "--help" => {
                print!("{HELP_LOAD}");
                return Ok(());
            }
            "--profile" => {
                let v = it
                    .next()
                    .ok_or_else(|| usage("--profile needs a value".into()))?;
                profile = Profile::from_name(v).ok_or_else(|| {
                    usage(format!(
                        "unknown profile {v:?} (expected compact, balanced, or covering)"
                    ))
                })?;
            }
            "--base" => {
                base = Some(
                    it.next()
                        .ok_or_else(|| usage("--base needs a value".into()))?
                        .clone(),
                );
            }
            "--trusted" => trusted = true,
            "--threads" => {
                let v = it
                    .next()
                    .ok_or_else(|| usage("--threads needs a value".into()))?;
                threads = v
                    .parse::<usize>()
                    .map_err(|_| usage("--threads must be an integer".into()))?;
                if threads == 0 {
                    threads = std::thread::available_parallelism().map_or(1, |n| n.get());
                }
            }
            "--sort-budget" => {
                let v = it
                    .next()
                    .ok_or_else(|| usage("--sort-budget needs a value (MiB)".into()))?;
                sort_budget = v
                    .parse::<usize>()
                    .map_err(|_| usage("--sort-budget must be an integer (MiB)".into()))?
                    << 20;
            }
            "--intern-budget" => {
                let v = it
                    .next()
                    .ok_or_else(|| usage("--intern-budget needs a value (MiB)".into()))?;
                intern_budget = Some(
                    v.parse::<usize>()
                        .map_err(|_| usage("--intern-budget must be an integer (MiB)".into()))?
                        << 20,
                );
            }
            flag if flag.starts_with('-') && flag.len() > 1 => {
                return Err(usage(format!("unknown option {flag:?}")));
            }
            _ if out.is_none() => out = Some(PathBuf::from(a)),
            _ => inputs.push(PathBuf::from(a)),
        }
    }
    let out = out.ok_or_else(|| usage("missing <out-dir>".into()))?;
    if inputs.is_empty() {
        return Err(usage(format!(
            "no input files (got out-dir {:?} and nothing to load)",
            out.display()
        )));
    }

    let mut cfg = BuilderConfig::new(&out);
    cfg.profile = profile;
    cfg.sort_budget = sort_budget;
    cfg.intern_budget = intern_budget;

    // Single-.hdt loads take the fast path (doc 03): the file's sections
    // feed the builders directly — no parsing, no re-interning. Mixed
    // inputs fall back to the term-level path below.
    if inputs.len() == 1 && format_of(&inputs[0])? == "hdt" {
        let started = Instant::now();
        let r = graphy_hdt::HdtReader::open(&inputs[0])
            .map_err(|e| format!("{}: {e}", inputs[0].display()))?;
        let manifest = graphy_hdt::import_segment(&r, &cfg)
            .map_err(|e| format!("{}: {e}", inputs[0].display()))?;
        let secs = started.elapsed().as_secs_f64();
        let c = &manifest.counts;
        println!(
            "segment: {} quads, {} triples, {} shared + {} subjects + {} objects terms, profile {} ({} quads/s, hdt fast path, {secs:.2}s)",
            c.quads,
            c.triples,
            c.shared,
            c.subjects,
            c.objects,
            manifest.profile,
            (c.quads as f64 / secs) as u64,
        );
        return Ok(());
    }

    let mut builder = SegmentBuilder::new(cfg).map_err(|e| e.to_string())?;
    let started = Instant::now();
    let mut total_bytes = 0u64;
    let mut n_quads = 0u64;
    // Blank labels are document-scoped: namespace per file when several
    // inputs combine into one dataset.
    let multi = inputs.len() > 1;
    let file_opts = |i: usize| Options {
        base: base.clone(),
        trusted,
        label_ns: multi.then_some(i as u128),
        ..Options::default()
    };
    if threads > 1 {
        let lanes: Vec<std::sync::Mutex<graphy_store::IngestLane>> = builder
            .lanes(threads)
            .map_err(|e| e.to_string())?
            .into_iter()
            .map(std::sync::Mutex::new)
            .collect();
        for (i, input) in inputs.iter().enumerate() {
            let bytes = std::fs::metadata(input).map(|m| m.len()).unwrap_or(0);
            total_bytes += bytes;
            n_quads += load_file_parallel(input, file_opts(i), threads, &lanes)?;
        }
        for lane in lanes {
            builder
                .join(lane.into_inner().expect("no poisoned lanes"))
                .map_err(|e| e.to_string())?;
        }
    } else {
        for (i, input) in inputs.iter().enumerate() {
            let bytes = std::fs::metadata(input).map(|m| m.len()).unwrap_or(0);
            total_bytes += bytes;
            n_quads += load_file(input, file_opts(i), &mut builder)?;
        }
    }
    let parse_secs = started.elapsed().as_secs_f64();
    let manifest = builder.finish().map_err(|e| e.to_string())?;
    let total_secs = started.elapsed().as_secs_f64();
    println!(
        "loaded {n_quads} quads ({:.1} MiB) in {total_secs:.2}s \
         (parse+intern {parse_secs:.2}s, {:.0}k quads/s overall)",
        total_bytes as f64 / (1 << 20) as f64,
        n_quads as f64 / total_secs / 1e3,
    );
    println!(
        "segment: {} quads, {} triples, {} shared + {} subjects + {} objects terms, profile {}",
        manifest.counts.quads,
        manifest.counts.triples,
        manifest.counts.shared,
        manifest.counts.subjects,
        manifest.counts.objects,
        manifest.profile,
    );
    Ok(())
}

/// File format by extension.
fn format_of(path: &Path) -> Result<&'static str, String> {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    Ok(match ext.as_str() {
        "nq" | "nquads" => "nq",
        "nt" | "ntriples" => "nt",
        "ttl" | "turtle" => "ttl",
        "trig" => "trig",
        "hdt" | "hdtq" => "hdt",
        other => return Err(format!("{}: unknown extension {other:?}", path.display())),
    })
}

/// Parse one file by extension and feed the builder. Returns quads pushed.
fn load_file(path: &Path, opts: Options, builder: &mut SegmentBuilder) -> Result<u64, String> {
    if format_of(path)? == "hdt" {
        // Binary import (doc 03): triples arrive pre-sorted and already
        // split into dictionary sections — no text parsing at all.
        let r =
            graphy_hdt::HdtReader::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
        let mut n = 0u64;
        r.each_quad(|s, p, o, g| {
            builder
                .push_quad(s, p, o, g)
                .map_err(|e| graphy_hdt::HdtError::Format(e.to_string()))?;
            n += 1;
            Ok(())
        })
        .map_err(|e| format!("{}: {e}", path.display()))?;
        return Ok(n);
    }
    let file = std::fs::File::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let mut n = 0u64;
    let mut push_err: Option<String> = None;
    {
        let mut sink = |q: QuadRef<'_>| {
            if push_err.is_some() {
                return;
            }
            if let Err(e) = builder.push_quad(q.s, q.p, q.o, q.g) {
                push_err = Some(e.to_string());
            }
            n += 1;
        };
        let io_err = |e: graphy_turtle::Error| format!("{}: {e}", path.display());
        match format_of(path)? {
            "nq" => {
                let mut p = NQuadsParser::new(opts).map_err(|e| e.to_string())?;
                p.read_from(file, &mut sink).map_err(io_err)?;
            }
            "nt" => {
                let mut p = NTriplesParser::new(opts).map_err(|e| e.to_string())?;
                p.read_from(file, &mut sink).map_err(io_err)?;
            }
            "ttl" => {
                let mut p = TurtleParser::new(opts).map_err(|e| e.to_string())?;
                p.read_from(file, &mut sink).map_err(io_err)?;
            }
            "trig" => {
                let mut p = TriGParser::new(opts).map_err(|e| e.to_string())?;
                p.read_from(file, &mut sink).map_err(io_err)?;
            }
            _ => unreachable!("format_of covers all"),
        }
    }
    match push_err {
        Some(e) => Err(e),
        None => Ok(n),
    }
}

/// Parallel variant (doc 03 §4.1 + doc 07 §7): N-Triples/N-Quads split at
/// newline boundaries across `threads` parse workers, each interning into
/// its own ingest lane; Turtle/TriG are inherently sequential and stream
/// through lane 0.
fn load_file_parallel(
    path: &Path,
    opts: Options,
    threads: usize,
    lanes: &[std::sync::Mutex<graphy_store::IngestLane>],
) -> Result<u64, String> {
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

    let format = format_of(path)?;
    if format == "hdt" {
        // Binary import: sequential decode through lane 0.
        let r =
            graphy_hdt::HdtReader::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
        let lane = &mut *lanes[0].lock().expect("no poisoned lanes");
        let before = lane.pushed();
        r.each_quad(|s, p, o, g| {
            lane.push_quad(s, p, o, g)
                .map_err(|e| graphy_hdt::HdtError::Format(e.to_string()))?;
            Ok(())
        })
        .map_err(|e| format!("{}: {e}", path.display()))?;
        return Ok(lane.pushed() - before);
    }
    if matches!(format, "ttl" | "trig") {
        // Sequential formats: parse serially, intern through one lane.
        let file = std::fs::File::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
        let lane = &mut *lanes[0].lock().expect("no poisoned lanes");
        let before = lane.pushed();
        let mut push_err: Option<String> = None;
        let mut sink = |q: QuadRef<'_>| {
            if push_err.is_some() {
                return;
            }
            if let Err(e) = lane.push_quad(q.s, q.p, q.o, q.g) {
                push_err = Some(e.to_string());
            }
        };
        let io_err = |e: graphy_turtle::Error| format!("{}: {e}", path.display());
        if format == "ttl" {
            let mut p = TurtleParser::new(opts).map_err(|e| e.to_string())?;
            p.read_from(file, &mut sink).map_err(io_err)?;
        } else {
            let mut p = TriGParser::new(opts).map_err(|e| e.to_string())?;
            p.read_from(file, &mut sink).map_err(io_err)?;
        }
        return match push_err {
            Some(e) => Err(e),
            None => Ok(lane.pushed() - before),
        };
    }

    let file = std::fs::File::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
    // SAFETY: mapping a file that changes underneath is undefined behavior
    // at the OS level. Load inputs are assumed stable for the duration of
    // the load (the same assumption every streaming reader of the file
    // makes, made explicit here by the mapping).
    let mmap =
        unsafe { memmap2::Mmap::map(&file) }.map_err(|e| format!("{}: {e}", path.display()))?;
    let n = AtomicU64::new(0);
    let failed = AtomicBool::new(false);
    let push_err = std::sync::Mutex::new(None::<String>);
    let sink = |seg: usize, q: QuadRef<'_>| {
        if failed.load(Ordering::Relaxed) {
            return;
        }
        let mut lane = lanes[seg].lock().expect("no poisoned lanes");
        if let Err(e) = lane.push_quad(q.s, q.p, q.o, q.g) {
            failed.store(true, Ordering::Relaxed);
            *push_err.lock().expect("no poisoned error slot") = Some(e.to_string());
        }
        n.fetch_add(1, Ordering::Relaxed);
    };
    let parse = if format == "nq" {
        graphy_turtle::par::nquads(&mmap, &opts, threads, sink)
    } else {
        graphy_turtle::par::ntriples(&mmap, &opts, threads, sink)
    };
    parse.map_err(|e| format!("{}: {e}", path.display()))?;
    if let Some(e) = push_err.into_inner().expect("no poisoned error slot") {
        return Err(e);
    }
    Ok(n.into_inner())
}

fn cmd_verify(args: &[String]) -> Result<(), Failure> {
    let usage = |msg: String| Failure::Usage(msg, HELP_VERIFY);
    let mut dir: Option<&String> = None;
    for a in args {
        match a.as_str() {
            "-h" | "--help" => {
                print!("{HELP_VERIFY}");
                return Ok(());
            }
            flag if flag.starts_with('-') && flag.len() > 1 => {
                return Err(usage(format!("unknown option {flag:?}")));
            }
            _ if dir.is_none() => dir = Some(a),
            other => return Err(usage(format!("unexpected argument {other:?}"))),
        }
    }
    let dir = dir.ok_or_else(|| usage("missing <segment-dir>".into()))?;
    let seg_dir = resolve_segment_dir(Path::new(dir)).map_err(|e| e.to_string())?;
    let started = Instant::now();
    let manifest = Segment::verify(&seg_dir).map_err(|e| e.to_string())?;
    println!(
        "ok: {} quads, {} triples, {} components verified in {:.2}s",
        manifest.counts.quads,
        manifest.counts.triples,
        manifest.components.len(),
        started.elapsed().as_secs_f64(),
    );
    Ok(())
}

fn cmd_export(args: &[String]) -> Result<(), Failure> {
    let usage = |msg: String| Failure::Usage(msg, HELP_EXPORT);
    let mut dir: Option<PathBuf> = None;
    let mut out: Option<PathBuf> = None;
    let mut mode = OpenMode::Heap;
    let mut format = "nq".to_owned();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "-h" | "--help" => {
                print!("{HELP_EXPORT}");
                return Ok(());
            }
            "-o" => {
                out = Some(PathBuf::from(
                    it.next().ok_or_else(|| usage("-o needs a value".into()))?,
                ));
            }
            "--mmap" => mode = OpenMode::Mmap,
            "--format" => {
                let v = it
                    .next()
                    .ok_or_else(|| usage("--format needs a value".into()))?;
                match v.as_str() {
                    "nq" | "hdt" | "hdtq" => format = v.clone(),
                    other => return Err(usage(format!("unknown format {other:?}"))),
                }
            }
            flag if flag.starts_with('-') && flag.len() > 1 => {
                return Err(usage(format!("unknown option {flag:?}")));
            }
            _ if dir.is_none() => dir = Some(PathBuf::from(a)),
            other => return Err(usage(format!("unexpected argument {other:?}"))),
        }
    }
    let dir = dir.ok_or_else(|| usage("missing <segment-dir>".into()))?;
    let mut sink: Box<dyn std::io::Write> = match &out {
        Some(p) => Box::new(std::fs::File::create(p).map_err(|e| format!("{}: {e}", p.display()))?),
        None => Box::new(std::io::stdout().lock()),
    };
    // --format hdt = the triples-only view (doc 03): graph components
    // drop, duplicates collapse inside the writer. --format hdtq keeps
    // graphs (qEndpoint-dialect HDTQ).
    let keep_graphs = format == "hdtq";
    let mut hdt = (format == "hdt" || keep_graphs).then(graphy_hdt::HdtWriter::new);
    let mut nq = if format == "nq" {
        Some(graphy_turtle::NQuadsWriter::new(std::io::BufWriter::new(
            std::mem::replace(&mut sink, Box::new(std::io::sink())),
        )))
    } else {
        None
    };
    let e = |e: graphy_store::StoreError| e.to_string();
    let mut write = |s: &[u8], p: &[u8], o: &[u8], g: Option<&[u8]>| -> Result<(), Failure> {
        if let Some(h) = &mut hdt {
            h.add_quad(s, p, o, if keep_graphs { g } else { None })
                .map_err(|e| e.to_string())?;
            return Ok(());
        }
        nq.as_mut()
            .expect("nq writer")
            .write_quad(&QuadRef {
                s,
                p,
                o,
                g,
                shorthand: None,
            })
            .map_err(|e| e.to_string())?;
        Ok(())
    };
    if dir.join("wal.log").exists() {
        // A store: export the full committed state (base ∪ delta) through
        // a snapshot, so not-yet-compacted writes are included.
        let store = Store::open_with(&dir, mode).map_err(e)?;
        let snap = store.snapshot();
        let mut scan = snap
            .scan(&Pattern::default(), graphy_store::Order::Spo)
            .map_err(e)?;
        let mut batch = graphy_store::QuadBatch::new();
        while scan.next_batch(&mut batch).map_err(e)? {
            for i in 0..batch.len() {
                let s = snap.decode_value(batch.s[i], TermPos::Subject).map_err(e)?;
                let p = snap
                    .decode_value(batch.p[i], TermPos::Predicate)
                    .map_err(e)?;
                let o = snap.decode_value(batch.o[i], TermPos::Object).map_err(e)?;
                let g = if batch.g[i] == 0 {
                    None
                } else {
                    Some(snap.decode_value(batch.g[i], TermPos::Graph).map_err(e)?)
                };
                write(&s, &p, &o, g.as_deref())?;
            }
        }
    } else {
        let seg = Segment::open_with(&dir, mode).map_err(e)?;
        for q in seg.scan(&Pattern::default()).map_err(e)? {
            let s = seg.decode_value(q[0], TermPos::Subject).map_err(e)?;
            let p = seg.decode_value(q[1], TermPos::Predicate).map_err(e)?;
            let o = seg.decode_value(q[2], TermPos::Object).map_err(e)?;
            let g = if q[3] == 0 {
                None
            } else {
                Some(seg.decode_value(q[3] - 1, TermPos::Graph).map_err(e)?)
            };
            write(&s, &p, &o, g.as_deref())?;
        }
    }
    if let Some(w) = nq {
        w.into_inner().flush().map_err(|e| e.to_string())?;
    }
    if let Some(h) = hdt {
        h.write_to(&mut sink).map_err(|e| e.to_string())?;
        sink.flush().map_err(|e| e.to_string())?;
    }
    Ok(())
}

fn cmd_compact(args: &[String]) -> Result<(), Failure> {
    let usage = |msg: String| Failure::Usage(msg, HELP_COMPACT);
    let mut dir: Option<PathBuf> = None;
    let mut sort_budget = 256usize << 20;
    let mut profile: Option<Profile> = None;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "-h" | "--help" => {
                print!("{HELP_COMPACT}");
                return Ok(());
            }
            "--profile" => {
                let v = it
                    .next()
                    .ok_or_else(|| usage("--profile needs a value".into()))?;
                profile = Some(Profile::from_name(v).ok_or_else(|| {
                    usage(format!(
                        "unknown profile {v:?} (expected compact, balanced, or covering)"
                    ))
                })?);
            }
            "--sort-budget" => {
                let v = it
                    .next()
                    .ok_or_else(|| usage("--sort-budget needs a value".into()))?;
                let mib: usize = v
                    .parse()
                    .map_err(|_| usage(format!("invalid --sort-budget {v:?}")))?;
                sort_budget = mib << 20;
            }
            flag if flag.starts_with('-') && flag.len() > 1 => {
                return Err(usage(format!("unknown option {flag:?}")));
            }
            _ if dir.is_none() => dir = Some(PathBuf::from(a)),
            other => return Err(usage(format!("unexpected argument {other:?}"))),
        }
    }
    let dir = dir.ok_or_else(|| usage("missing <store-dir>".into()))?;
    // Check before opening: `Store::open` creates an (empty) log. A
    // profile change always rebuilds, writes or not.
    if profile.is_none() && !dir.join("wal.log").exists() {
        println!("nothing to compact: no write-ahead log (no writes yet)");
        return Ok(());
    }
    let store = Store::open(&dir).map_err(|e| e.to_string())?;
    let before = store.snapshot();
    let events = before.delta_events();
    let old_gen = before.generation();
    let same_profile = profile.is_none_or(|p| p.name() == before.segment().manifest.profile);
    if events == 0 && same_profile {
        println!("nothing to compact: delta is empty");
        return Ok(());
    }
    drop(before);

    let started = Instant::now();
    let cfg = MergeConfig {
        sort_budget,
        profile,
        // Explicit offline compaction: nothing to pace for.
        ..MergeConfig::default()
    };
    let snap = store.merge_with(&cfg).map_err(|e| e.to_string())?;
    let quads = snap.segment().manifest.counts.quads;
    let new_gen = snap.generation();
    drop(snap);
    store.gc(); // retire the old generation now that nothing reads it
    let secs = started.elapsed().as_secs_f64();
    println!(
        "compacted: generation {old_gen} → {new_gen}, {quads} quads \
         ({events} delta events folded) in {secs:.2}s"
    );
    Ok(())
}

const HELP_QUERY: &str = "\
graphy query — run a SPARQL query against a store (local, no server)

USAGE:
    graphy query <store-dir> (<query-file> | -e <QUERY>) [OPTIONS]

ARGS:
    <store-dir>     Store (or segment) directory
    <query-file>    File containing the SPARQL query text

OPTIONS:
    -e <QUERY>        Inline query text instead of a file
    --explain         Print the physical plan instead of executing
    --analyze         Execute and print the plan with actual rows/timing
    --timeout <SECS>  Per-query deadline
    --threads <N>     Worker threads for the morsel pool (0 = all cores)
    -h, --help        Show this help

Solutions print as a tab-separated table (terms in SPARQL surface
syntax); ASK prints true/false; CONSTRUCT/DESCRIBE print N-Triples.
";

/// A decoded concise term in SPARQL surface syntax.
fn term_text(bytes: &[u8]) -> String {
    fn fmt(t: &graphy_core::TermRef<'_>) -> String {
        use graphy_core::TermRef;
        match t {
            TermRef::Iri(i) => format!("<{i}>"),
            TermRef::BlankNode(l) => format!("_:{l}"),
            TermRef::Literal(l) => {
                if let Some((tag, dir)) = l.lang() {
                    let dir = match dir {
                        Some(graphy_core::Dir::Ltr) => "--ltr",
                        Some(graphy_core::Dir::Rtl) => "--rtl",
                        None => "",
                    };
                    format!("{:?}@{tag}{dir}", l.lexical())
                } else if l.datatype() == graphy_core::vocab::XSD_STRING {
                    format!("{:?}", l.lexical())
                } else {
                    format!("{:?}^^<{}>", l.lexical(), l.datatype())
                }
            }
            TermRef::TripleTerm(v) => format!(
                "<<( {} {} {} )>>",
                fmt(&v.subject()),
                fmt(&v.predicate()),
                fmt(&v.object())
            ),
        }
    }
    match graphy_core::concise::decode(bytes) {
        Ok(t) => fmt(&t),
        Err(_) => String::from_utf8_lossy(bytes).into_owned(),
    }
}

fn cmd_query(args: &[String]) -> Result<(), Failure> {
    let usage = |msg: String| Failure::Usage(msg, HELP_QUERY);
    let mut dir: Option<PathBuf> = None;
    let mut file: Option<PathBuf> = None;
    let mut inline: Option<String> = None;
    let mut explain = false;
    let mut analyze = false;
    let mut timeout: Option<u64> = None;
    let mut threads: usize = 0;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "-h" | "--help" => {
                print!("{HELP_QUERY}");
                return Ok(());
            }
            "-e" => {
                inline = Some(
                    it.next()
                        .ok_or_else(|| usage("-e needs a query".into()))?
                        .clone(),
                )
            }
            "--explain" => explain = true,
            "--analyze" => analyze = true,
            "--timeout" => {
                timeout = Some(
                    it.next()
                        .and_then(|v| v.parse().ok())
                        .ok_or_else(|| usage("--timeout needs seconds".into()))?,
                )
            }
            "--threads" => {
                threads = it
                    .next()
                    .and_then(|v| v.parse().ok())
                    .ok_or_else(|| usage("--threads needs a count".into()))?
            }
            other if other.starts_with('-') => {
                return Err(usage(format!("unknown option `{other}`")))
            }
            _ => {
                if dir.is_none() {
                    dir = Some(PathBuf::from(a));
                } else if file.is_none() && inline.is_none() {
                    file = Some(PathBuf::from(a));
                } else {
                    return Err(usage(format!("unexpected argument `{a}`")));
                }
            }
        }
    }
    let dir = dir.ok_or_else(|| usage("missing <store-dir>".into()))?;
    let text = match (inline, file) {
        (Some(t), None) => t,
        (None, Some(f)) => std::fs::read_to_string(&f)
            .map_err(|e| Failure::Run(format!("read {}: {e}", f.display())))?,
        _ => return Err(usage("provide a query file or -e <QUERY>".into())),
    };

    let parsed = graphy_sparql_syntax::parse_query(&text)
        .map_err(|e| Failure::Run(format!("parse error: {e}")))?;
    let mut tq = graphy_algebra::translate_query(&parsed)
        .map_err(|e| Failure::Run(format!("translate error: {e}")))?;
    tq.root = graphy_algebra::rewrite(tq.root.clone());

    let store = Store::open(&dir).map_err(|e| Failure::Run(format!("open store: {e}")))?;
    let snap = store.snapshot();

    if explain {
        let plan =
            graphy_engine::exec::explain(&snap, &tq).map_err(|e| Failure::Run(e.to_string()))?;
        print!("{plan}");
        return Ok(());
    }

    let opts = graphy_engine::exec::ExecOptions {
        deadline: timeout.map(|s| Instant::now() + std::time::Duration::from_secs(s)),
        threads,
        ..Default::default()
    };
    let started = Instant::now();
    let (out, plan) = if analyze {
        let (out, plan) = graphy_engine::exec::explain_analyze(&snap, &tq, &opts)
            .map_err(|e| Failure::Run(e.to_string()))?;
        (out, Some(plan))
    } else {
        (
            graphy_engine::exec::evaluate_with(&snap, &tq, &opts)
                .map_err(|e| Failure::Run(e.to_string()))?,
            None,
        )
    };
    let elapsed = started.elapsed();

    let stdout = std::io::stdout();
    let mut w = stdout.lock();
    let n = match out {
        graphy_engine::Output::Boolean(b) => {
            writeln!(w, "{b}").ok();
            1
        }
        graphy_engine::Output::Solutions { vars, rows } => {
            writeln!(w, "{}", vars.join("\t")).ok();
            let n = rows.len();
            for row in rows {
                let line: Vec<String> = row
                    .iter()
                    .map(|c| c.as_deref().map(term_text).unwrap_or_default())
                    .collect();
                writeln!(w, "{}", line.join("\t")).ok();
            }
            n
        }
        graphy_engine::Output::Triples(triples) => {
            let n = triples.len();
            for (s, p, o) in triples {
                writeln!(w, "{} {} {} .", term_text(&s), term_text(&p), term_text(&o)).ok();
            }
            n
        }
    };
    if let Some(plan) = plan {
        eprintln!("\n{plan}");
    }
    eprintln!("{n} result(s) in {:.1} ms", elapsed.as_secs_f64() * 1e3);
    Ok(())
}

const HELP_SERVE: &str = "\
graphy serve — serve a store over the SPARQL 1.1 Protocol (HTTP)

USAGE:
    graphy serve <store-dir> [OPTIONS]

OPTIONS:
    --bind <ADDR>       Listen address (default 127.0.0.1:7878)
    --read-only         Reject updates with 403
    --allow-network     Permit outbound requests such as SPARQL LOAD
                        (also requires build feature `outbound-http`)
    --timeout <SECS>    Default per-request deadline (default 60)
    -h, --help          Show this help

ENDPOINTS:
    GET|POST /sparql            SPARQL Protocol query; POST update
    GET      /sparql/service    Service description
    GET      /health            Liveness

Results conneg: sparql-results+json (default) / +xml / text/csv /
text/tab-separated-values; CONSTRUCT/DESCRIBE: text/turtle /
application/n-triples. Query responses carry ETag \"G<gen>.E<epoch>\".
";

fn cmd_serve(args: &[String]) -> Result<(), Failure> {
    let usage = |msg: String| Failure::Usage(msg, HELP_SERVE);
    let mut dir: Option<PathBuf> = None;
    let mut bind = "127.0.0.1:7878".to_owned();
    let mut cfg = graphy_server::Config::default();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "-h" | "--help" => {
                print!("{HELP_SERVE}");
                return Ok(());
            }
            "--bind" => {
                bind = it
                    .next()
                    .ok_or_else(|| usage("--bind needs an address".into()))?
                    .clone()
            }
            "--read-only" => cfg.read_only = true,
            "--allow-network" => cfg.allow_network = true,
            "--timeout" => {
                let secs: u64 = it
                    .next()
                    .and_then(|v| v.parse().ok())
                    .ok_or_else(|| usage("--timeout needs seconds".into()))?;
                cfg.default_timeout = std::time::Duration::from_secs(secs);
            }
            other if other.starts_with('-') => {
                return Err(usage(format!("unknown option `{other}`")))
            }
            _ => {
                if dir.is_none() {
                    dir = Some(PathBuf::from(a));
                } else {
                    return Err(usage(format!("unexpected argument `{a}`")));
                }
            }
        }
    }
    let dir = dir.ok_or_else(|| usage("missing <store-dir>".into()))?;
    if cfg.allow_network && !graphy_server::outbound_network_compiled() {
        return Err(usage(
            "--allow-network requires a binary built with `--features outbound-http`".into(),
        ));
    }
    let store = Store::open(&dir).map_err(|e| Failure::Run(format!("open store: {e}")))?;
    serve(&bind, store, cfg)
}

#[cfg(not(target_arch = "wasm32"))]
fn serve(bind: &str, store: Store, cfg: graphy_server::Config) -> Result<(), Failure> {
    graphy_server::serve_blocking(bind, store, cfg).map_err(Failure::Run)
}

// wasm32 hosts have no listening sockets; every other command works, so
// the CLI builds for wasi with only this verb reporting unavailability.
#[cfg(target_arch = "wasm32")]
fn serve(_bind: &str, _store: Store, _cfg: graphy_server::Config) -> Result<(), Failure> {
    Err(Failure::Run(
        "serve is not available in wasm builds (no listening sockets)".into(),
    ))
}
