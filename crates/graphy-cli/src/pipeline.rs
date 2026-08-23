//! `graphy <verb> [/ <verb>]…` — the internal pipeline (docs/09, milestone
//! MC): commands chained with `/` exchange quads in memory as concise-bytes
//! events; nothing re-serializes between stages. This module owns the argv
//! grammar and help surface; execution lives in `graphy-pipe`.

use std::path::PathBuf;

use graphy_pipe::{
    DistinctBy, Format, Input, Junction, OpSpec, PipelineSpec, SourceSpec, TerminalSpec, Unit,
};

use crate::Failure;

/// Verbs implemented by this increment (C1).
pub const VERBS: &[&str] = &[
    "read", "scan", "scribe", "write", "skip", "head", "tail", "tree", "concat", "merge", "count",
    "distinct",
];

/// Parity verbs that arrive with later MC increments — recognized so the
/// error names the increment instead of pretending the verb doesn't exist.
const FUTURE_VERBS: &[(&str, &str)] = &[
    ("filter", "C2 (Quad Filter Expressions)"),
    ("transform", "C5 (Quad Transform Expressions)"),
    ("union", "C3 (set algebra)"),
    ("intersect", "C3 (set algebra)"),
    ("intersection", "C3 (set algebra)"),
    ("diff", "C3 (set algebra)"),
    ("difference", "C3 (set algebra)"),
    ("minus", "C3 (set algebra)"),
    ("subtract", "C3 (set algebra)"),
    ("equals", "C3 (set algebra)"),
    ("equal", "C3 (set algebra)"),
    ("disjoint", "C3 (set algebra)"),
    ("contains", "C3 (set algebra)"),
    ("canonical", "C4 (RDFC-1.0)"),
    ("canonicalize", "C4 (RDFC-1.0)"),
];

pub fn is_pipeline_verb(v: &str) -> bool {
    VERBS.contains(&v) || FUTURE_VERBS.iter().any(|(name, _)| *name == v)
}

pub const HELP_PIPELINE: &str = "\
graphy pipeline — chain commands with `/`; quads flow between them in
memory as concise-term events (no serialization between stages)

USAGE:
    graphy <COMMAND> [ARGS] [ / <COMMAND> [ARGS] ]... [--inputs <FILE>...]

A pipeline starts with a deserializer (read or scan), runs quads through
zero or more operators, and ends in a serializer or a result value; a
missing final serializer appends `scribe -c nq`. Without --inputs the
pipeline reads stdin (content-type defaults to trig). With several inputs,
the commands before the joining command run once per input (blank labels
get per-input namespaces so they never unify), and exactly one of
concat/merge joins the streams.

COMMANDS:
    read       Deserialize RDF (single-threaded)     [see: graphy help read]
    scan       Deserialize N-Triples/N-Quads on several threads
    scribe     Serialize canonical N-Quads/N-Triples [see: graphy help write]
    write      Serialize pretty Turtle/TriG
    skip       Drop the first n quads or subject groups   [help skip]
    head       Forward only the first n, then stop reading input
    tail       Forward only the last n
    tree       Dedup + regroup into a graph/subject/predicate tree
    concat     Join inputs sequentially, preserving order [help concat]
    merge      Join inputs concurrently, arrival order
    count      Output the quad count as a number          [help count]
    distinct   Output the count of unique quads/terms

Coming in later increments: filter (C2), set algebra union/intersect/
diff/minus/equals/disjoint/contains (C3), canonical (C4), transform (C5).

EXAMPLES:
    graphy read -c ttl / write -c trig < in.ttl > out.trig
    graphy read / head 100 -s / write --inputs big.trig
    graphy read / tree / count --inputs dup.nq
    graphy read / merge / scribe --inputs a.nq b.nq c.nq
";

pub const HELP_READ: &str = "\
graphy read / scan — deserialize RDF into the pipeline

USAGE:
    graphy read [OPTIONS] / ...
    graphy scan [OPTIONS] / ...

OPTIONS:
    -c, --content-type <fmt>
                  nt, nq, ttl, trig (or the equivalent media types).
                  Default: by file extension; stdin defaults to trig
    -b, --base-uri <iri>
                  Base IRI for resolving relative references (Turtle/TriG)
    -r, --relax   Keep going past parse errors (reported as warnings)
    --trusted     Skip character-level validation for input known valid
                  (~30% faster N-Quads; invalid input may misparse, never
                  unsafely)
    --threads <n> (scan only) parse workers; 0 = one per CPU [default: 0]
    -h, --help    Show this help

read streams any supported format and stops reading the moment downstream
stops (head bounds I/O, not just output). scan memory-maps N-Triples/
N-Quads files and parses segments in parallel; quads arrive in worker
order (within a segment: document order), so use read when order matters.
";

pub const HELP_WRITE: &str = "\
graphy scribe / write — serialize the pipeline to stdout

USAGE:
    graphy ... / scribe [-c nq|nt]
    graphy ... / write [-c trig|ttl|nq|nt]

OPTIONS:
    -c, --content-type <fmt>
                  scribe: nq (default) or nt (rejects named-graph quads).
                  write: trig (default), ttl (rejects named-graph quads),
                  or nq/nt (canonical — same as scribe)
    -h, --help    Show this help

scribe is the fast canonical serializer. write pretty-prints: same-subject
quads group with `;`/`,`, rdf:type prints as `a`, IRIs compact against the
prefixes declared by the input (declarations seen before the first quad
form the @prefix header), TriG wraps graph runs in blocks. A pipeline
without a final serializer appends `scribe -c nq`.
";

pub const HELP_SLICE: &str = "\
graphy skip / head / tail — slice the quad stream

USAGE:
    graphy ... / skip [n] [-q|-s] / ...
    graphy ... / head [n] [-q|-s] / ...
    graphy ... / tail [n] [-q|-s] / ...

ARGS:
    [n]           How many to skip/keep [default: 1]; plain or scientific
                  notation (skip 4e6)

OPTIONS:
    -q, --quads     Count quads [default]
    -s, --subjects  Count subject groups (consecutive same-subject runs,
                    in stream order)
    -h, --help      Show this help

skip drops the first n and forwards the rest. head forwards the first n
and then cancels upstream — the source stops reading its input. tail
buffers n and emits them at end of stream. With several inputs these run
once per input (before the concat/merge join).
";

pub const HELP_TREE: &str = "\
graphy tree — dedup + regroup quads into a dataset tree

USAGE:
    graphy ... / tree / ...

Puts quads into a graph > subject > predicate tree (first-seen order at
every level; duplicates drop) and emits it at end of stream — so a
downstream write consolidates subjects that were scattered through the
input. `read / tree / write` is the canonical pretty-print pipeline.
Identity is exact term equality (concise bytes); memory grows with the
number of distinct quads.

OPTIONS:
    -h, --help    Show this help
";

pub const HELP_JOIN: &str = "\
graphy concat / merge — join several inputs into one stream

USAGE:
    graphy read [/ ops] / concat [/ ops] / ... --inputs <FILE>...
    graphy read [/ ops] / merge  [/ ops] / ... --inputs <FILE>...

The commands before the join run once per input (each input gets its own
parser and operator instances; blank labels are namespaced per input so
identical surface labels never unify across files). concat processes
inputs sequentially and preserves order; merge runs one thread per input
and forwards quads in arrival order (faster, unordered).

OPTIONS:
    -h, --help    Show this help
";

pub const HELP_COUNT: &str = "\
graphy count / distinct — result values (a number on stdout)

USAGE:
    graphy ... / count
    graphy ... / distinct [-q|-t|-s|-p|-o|-g]

OPTIONS (distinct):
    -q, --quads       Unique quads [default]
    -t, --triples     Unique triples (graph ignored)
    -s, --subjects    Unique subject terms
    -p, --predicates  Unique predicate terms
    -o, --objects     Unique object terms
    -g, --graphs      Unique graph terms (the default graph counts as one)
    -h, --help        Show this help

Both are terminal commands: they consume the stream and print a single
number followed by a newline.
";

/// Help text for one pipeline verb (the group it belongs to).
pub fn help_for(verb: &str) -> Option<&'static str> {
    Some(match verb {
        "pipeline" => HELP_PIPELINE,
        "read" | "scan" => HELP_READ,
        "scribe" | "write" => HELP_WRITE,
        "skip" | "head" | "tail" => HELP_SLICE,
        "tree" => HELP_TREE,
        "concat" | "merge" => HELP_JOIN,
        "count" | "distinct" => HELP_COUNT,
        _ => return None,
    })
}

fn usage(msg: String) -> Failure {
    Failure::Usage(msg, HELP_PIPELINE)
}

/// Parse one size positional (skip/head/tail). Accepts plain integers and
/// scientific notation (`4e6`, `2.5e3` — must denote a non-negative integer
/// ≤ 2⁵³, where f64 is still exact), matching the original's JS number
/// handling (`graphy read / skip 4e6 / head 2e6 / write` is a documented
/// idiom).
fn parse_n(verb: &str, value: &str) -> Result<u64, Failure> {
    if let Ok(n) = value.parse::<u64>() {
        return Ok(n);
    }
    let err = || {
        usage(format!(
            "{verb}: size must be a non-negative integer (scientific notation ok), got {value:?}"
        ))
    };
    let f = value.parse::<f64>().map_err(|_| err())?;
    if !f.is_finite() || f < 0.0 || f.fract() != 0.0 || f > (1u64 << 53) as f64 {
        return Err(err());
    }
    Ok(f as u64)
}

/// One parsed command group.
enum Cmd {
    Source(SourceSpec),
    Op(OpSpec),
    Junction(Junction),
    Terminal(TerminalSpec),
}

fn parse_group(group: &[&str]) -> Result<Cmd, Failure> {
    let verb = group[0];
    let args = &group[1..];
    let help = help_for(verb).unwrap_or(HELP_PIPELINE);
    if args.iter().any(|a| *a == "-h" || *a == "--help") {
        // Surface the group help as a usage "error" with exit 0 semantics
        // handled by the caller; simplest here: print and succeed.
        print!("{help}");
        return Err(Failure::Usage(String::new(), ""));
    }
    let reject_unknown =
        |a: &str| -> Failure { Failure::Usage(format!("{verb}: unknown option {a:?}"), help) };

    match verb {
        "read" | "scan" => {
            let mut spec = SourceSpec {
                par_threads: (verb == "scan").then_some(0),
                ..SourceSpec::default()
            };
            let mut it = args.iter();
            while let Some(a) = it.next() {
                match *a {
                    "-c" | "--content-type" => {
                        let v = it
                            .next()
                            .ok_or_else(|| usage(format!("{verb}: -c needs a value")))?;
                        spec.format = Some(Format::from_name(v).ok_or_else(|| {
                            usage(format!(
                                "{verb}: unknown content type {v:?} (nt, nq, ttl, trig)"
                            ))
                        })?);
                    }
                    "-b" | "--base-uri" | "--base" => {
                        spec.base = Some(
                            it.next()
                                .ok_or_else(|| usage(format!("{verb}: -b needs a value")))?
                                .to_string(),
                        );
                    }
                    "-r" | "--relax" => spec.lenient = true,
                    "--trusted" => spec.trusted = true,
                    "--threads" if verb == "scan" => {
                        let v = it
                            .next()
                            .ok_or_else(|| usage("scan: --threads needs a value".into()))?;
                        spec.par_threads = Some(v.parse::<usize>().map_err(|_| {
                            usage(format!("scan: --threads must be an integer, got {v:?}"))
                        })?);
                    }
                    other => return Err(reject_unknown(other)),
                }
            }
            Ok(Cmd::Source(spec))
        }
        "skip" | "head" | "tail" => {
            let mut n: Option<u64> = None;
            let mut unit = Unit::Quads;
            for a in args {
                match *a {
                    "-q" | "--quads" => unit = Unit::Quads,
                    "-s" | "--subjects" => unit = Unit::Subjects,
                    v if !v.starts_with('-') && n.is_none() => n = Some(parse_n(verb, v)?),
                    other => return Err(reject_unknown(other)),
                }
            }
            let n = n.unwrap_or(1);
            Ok(Cmd::Op(match verb {
                "skip" => OpSpec::Skip { n, unit },
                "head" => OpSpec::Head { n, unit },
                _ => OpSpec::Tail { n, unit },
            }))
        }
        "tree" => {
            if let Some(a) = args.first() {
                return Err(reject_unknown(a));
            }
            Ok(Cmd::Op(OpSpec::Tree))
        }
        "concat" | "merge" => {
            if let Some(a) = args.first() {
                return Err(reject_unknown(a));
            }
            Ok(Cmd::Junction(if verb == "concat" {
                Junction::Concat
            } else {
                Junction::Merge
            }))
        }
        "count" => {
            if let Some(a) = args.first() {
                return Err(reject_unknown(a));
            }
            Ok(Cmd::Terminal(TerminalSpec::Count))
        }
        "distinct" => {
            let mut by = DistinctBy::Quads;
            for a in args {
                by = match *a {
                    "-q" | "--quads" => DistinctBy::Quads,
                    "-t" | "--triples" => DistinctBy::Triples,
                    "-s" | "--subjects" => DistinctBy::Subjects,
                    "-p" | "--predicates" => DistinctBy::Predicates,
                    "-o" | "--objects" => DistinctBy::Objects,
                    "-g" | "--graphs" => DistinctBy::Graphs,
                    other => return Err(reject_unknown(other)),
                };
            }
            Ok(Cmd::Terminal(TerminalSpec::Distinct { by }))
        }
        "scribe" | "write" => {
            let mut format: Option<Format> = None;
            let mut it = args.iter();
            while let Some(a) = it.next() {
                match *a {
                    "-c" | "--content-type" => {
                        let v = it
                            .next()
                            .ok_or_else(|| usage(format!("{verb}: -c needs a value")))?;
                        format =
                            Some(Format::from_name(v).ok_or_else(|| {
                                usage(format!("{verb}: unknown content type {v:?}"))
                            })?);
                    }
                    other => return Err(reject_unknown(other)),
                }
            }
            let terminal = match (verb, format) {
                ("scribe", None | Some(Format::Nq)) => TerminalSpec::Scribe { triples_only: false },
                ("scribe", Some(Format::Nt)) => TerminalSpec::Scribe { triples_only: true },
                ("scribe", Some(f)) => {
                    return Err(Failure::Usage(
                        format!("scribe: unsupported content type {:?} (nq, nt; use write for pretty Turtle/TriG)", f.name()),
                        help,
                    ))
                }
                ("write", None | Some(Format::Trig)) => TerminalSpec::Write { trig: true },
                ("write", Some(Format::Ttl)) => TerminalSpec::Write { trig: false },
                ("write", Some(Format::Nq)) => TerminalSpec::Scribe { triples_only: false },
                ("write", Some(Format::Nt)) => TerminalSpec::Scribe { triples_only: true },
                _ => unreachable!("verb matched above"),
            };
            Ok(Cmd::Terminal(terminal))
        }
        other => {
            if let Some((_, inc)) = FUTURE_VERBS.iter().find(|(name, _)| *name == other) {
                Err(usage(format!(
                    "{other}: not implemented yet — arrives with MC increment {inc}"
                )))
            } else {
                Err(usage(format!("unknown pipeline command {other:?}")))
            }
        }
    }
}

pub fn cmd_pipeline(args: &[String]) -> Result<(), Failure> {
    // `--inputs FILE...` claims everything after it.
    let mut argv: Vec<&str> = args.iter().map(String::as_str).collect();
    let mut inputs: Vec<Input> = Vec::new();
    if let Some(at) = argv.iter().position(|a| *a == "--inputs") {
        let files = argv.split_off(at + 1);
        argv.pop(); // the --inputs token itself
        if files.is_empty() {
            return Err(usage("--inputs needs at least one file".into()));
        }
        for f in files {
            if f.starts_with('-') && f.len() > 1 {
                return Err(usage(format!(
                    "--inputs takes only file paths (got {f:?}); place options before it"
                )));
            }
            inputs.push(Input::File(PathBuf::from(f)));
        }
    } else {
        inputs.push(Input::Stdin);
    }

    // Split on `/` tokens into command groups.
    let mut groups: Vec<Vec<&str>> = vec![Vec::new()];
    for a in argv {
        if a == "/" {
            groups.push(Vec::new());
        } else {
            groups.last_mut().expect("nonempty").push(a);
        }
    }
    if groups.iter().any(Vec::is_empty) {
        return Err(usage("empty pipeline stage (stray `/`)".into()));
    }

    // Assemble the plan.
    let n_groups = groups.len();
    let mut source: Option<SourceSpec> = None;
    let mut before: Vec<OpSpec> = Vec::new();
    let mut junction: Option<Junction> = None;
    let mut after: Vec<OpSpec> = Vec::new();
    let mut terminal: Option<TerminalSpec> = None;
    for (i, group) in groups.iter().enumerate() {
        let cmd = match parse_group(group) {
            Ok(cmd) => cmd,
            // -h inside a group: printed already, succeed quietly.
            Err(Failure::Usage(msg, _)) if msg.is_empty() => return Ok(()),
            Err(e) => return Err(e),
        };
        match cmd {
            Cmd::Source(s) => {
                if i != 0 {
                    return Err(usage(format!(
                        "{}: deserializers start a pipeline (stage {})",
                        group[0],
                        i + 1
                    )));
                }
                source = Some(s);
            }
            Cmd::Op(op) => {
                if i == 0 {
                    return Err(usage(format!(
                        "pipelines start with read or scan (got {:?})",
                        group[0]
                    )));
                }
                if junction.is_some() {
                    after.push(op);
                } else {
                    before.push(op);
                }
            }
            Cmd::Junction(j) => {
                if i == 0 {
                    return Err(usage(format!(
                        "pipelines start with read or scan (got {:?})",
                        group[0]
                    )));
                }
                if junction.is_some() {
                    return Err(usage("one concat/merge per pipeline".into()));
                }
                junction = Some(j);
            }
            Cmd::Terminal(t) => {
                if i == 0 {
                    return Err(usage(format!(
                        "pipelines start with read or scan (got {:?})",
                        group[0]
                    )));
                }
                if i + 1 != n_groups {
                    return Err(usage(format!(
                        "{}: serializers and result values end a pipeline",
                        group[0]
                    )));
                }
                terminal = Some(t);
            }
        }
    }
    let source = source.ok_or_else(|| usage("pipelines start with read or scan".into()))?;

    if inputs.len() > 1 && junction.is_none() {
        return Err(usage(format!(
            "{} inputs but no joining command (add concat or merge)",
            inputs.len()
        )));
    }
    if junction.is_none() {
        debug_assert!(after.is_empty());
    }
    if source.par_threads.is_some() && matches!(inputs[0], Input::Stdin) {
        return Err(usage(
            "scan requires file inputs (stdin cannot be memory-mapped); use read".into(),
        ));
    }

    let spec = PipelineSpec {
        source,
        before,
        junction,
        after,
        terminal: terminal.unwrap_or(TerminalSpec::Scribe {
            triples_only: false,
        }),
    };
    let out: graphy_pipe::Out = Box::new(std::io::stdout());
    let mut on_warn = |w: String| eprintln!("graphy: warning: {w}");
    graphy_pipe::run(&spec, &inputs, out, &mut on_warn).map_err(|e| Failure::Run(e.to_string()))
}
