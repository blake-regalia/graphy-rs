//! MC C1 tests (docs/09 §8): operator semantics (quads + subject groups),
//! streaming dedup, result values, junction behavior (concat order, merge
//! multiset, per-input blank-label namespacing), prefix passthrough, and —
//! the load-bearing one — upstream cancellation: `head` bounds *input I/O*,
//! not just output.

use std::io::{self, Read, Write};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use graphy_pipe::{
    chain, read_stream, run, DistinctBy, Event, Flow, Format, Head, Input, Junction, Op, OpSpec,
    PipelineSpec, Sink, Skip, SourceSpec, Tail, TerminalSpec, Tree, Unit,
};
use graphy_turtle::{NQuadsWriter, Options};

// ------------------------------------------------------------ test harness

/// Collects quads as canonical N-Quads lines (plus prefix events).
#[derive(Debug, Default)]
struct Collect {
    lines: Vec<String>,
    prefixes: Vec<(String, String)>,
    finished: bool,
}

impl Sink for Collect {
    fn event(&mut self, ev: Event<'_>) -> io::Result<Flow> {
        match ev {
            Event::Quad(q) => {
                let mut w = NQuadsWriter::new(Vec::new());
                w.write_quad(&q)?;
                let line = String::from_utf8(w.into_inner()).expect("nq is utf-8");
                self.lines.push(line.trim_end().to_owned());
            }
            Event::Prefix { name, iri } => {
                self.prefixes.push((name.to_owned(), iri.to_owned()));
            }
        }
        Ok(Flow::Continue)
    }

    fn finish(&mut self) -> io::Result<()> {
        assert!(!self.finished, "finish called twice");
        self.finished = true;
        Ok(())
    }
}

/// Collect behind an Arc so tests can read results after the chain owns it.
#[derive(Debug, Clone, Default)]
struct SharedCollect(Arc<Mutex<Collect>>);

impl Sink for SharedCollect {
    fn event(&mut self, ev: Event<'_>) -> io::Result<Flow> {
        self.0.lock().expect("collect lock").event(ev)
    }
    fn finish(&mut self) -> io::Result<()> {
        self.0.lock().expect("collect lock").finish()
    }
}

fn run_ops(
    input: &str,
    format: Format,
    ops: Vec<Box<dyn Op>>,
) -> (Vec<String>, Vec<(String, String)>) {
    let collect = SharedCollect::default();
    let mut sink = chain(ops, Box::new(collect.clone()));
    let mut bytes = input.as_bytes();
    read_stream(
        &mut bytes,
        format,
        Options::default(),
        &mut *sink,
        &mut |e| panic!("unexpected warning: {e:?}"),
    )
    .expect("test input parses");
    sink.finish().expect("collect finish");
    let inner = collect.0.lock().expect("collect lock");
    assert!(inner.finished);
    (inner.lines.clone(), inner.prefixes.clone())
}

const NQ: &str = "\
<http://x/a> <http://x/p> \"1\" .
<http://x/a> <http://x/q> \"2\" .
<http://x/b> <http://x/p> \"3\" <http://x/g> .
<http://x/c> <http://x/p> \"4\" .
<http://x/a> <http://x/p> \"5\" .
";

fn nq_lines() -> Vec<String> {
    NQ.lines().map(str::to_owned).collect()
}

// -------------------------------------------------------------- operators

#[test]
fn skip_quads_and_subjects() {
    let (lines, _) = run_ops(NQ, Format::Nq, vec![Box::new(Skip::new(2, Unit::Quads))]);
    assert_eq!(lines, nq_lines()[2..]);

    // Groups: [a,a] [b] [c] [a] — skipping 2 groups drops the first three quads.
    let (lines, _) = run_ops(NQ, Format::Nq, vec![Box::new(Skip::new(2, Unit::Subjects))]);
    assert_eq!(lines, nq_lines()[3..]);

    let (lines, _) = run_ops(NQ, Format::Nq, vec![Box::new(Skip::new(0, Unit::Quads))]);
    assert_eq!(lines, nq_lines());
}

#[test]
fn head_quads_and_subjects() {
    let (lines, _) = run_ops(NQ, Format::Nq, vec![Box::new(Head::new(3, Unit::Quads))]);
    assert_eq!(lines, nq_lines()[..3]);

    let (lines, _) = run_ops(NQ, Format::Nq, vec![Box::new(Head::new(2, Unit::Subjects))]);
    assert_eq!(lines, nq_lines()[..3]);

    let (lines, _) = run_ops(NQ, Format::Nq, vec![Box::new(Head::new(0, Unit::Quads))]);
    assert!(lines.is_empty());

    // The reappearing subject `a` is a NEW group (stream-order runs).
    let (lines, _) = run_ops(NQ, Format::Nq, vec![Box::new(Head::new(4, Unit::Subjects))]);
    assert_eq!(lines, nq_lines());
}

#[test]
fn tail_quads_and_subjects() {
    let (lines, _) = run_ops(NQ, Format::Nq, vec![Box::new(Tail::new(2, Unit::Quads))]);
    assert_eq!(lines, nq_lines()[3..]);

    // Last 2 groups = [c] [a] → quads 4 and 5.
    let (lines, _) = run_ops(NQ, Format::Nq, vec![Box::new(Tail::new(2, Unit::Subjects))]);
    assert_eq!(lines, nq_lines()[3..]);

    let (lines, _) = run_ops(NQ, Format::Nq, vec![Box::new(Tail::new(0, Unit::Quads))]);
    assert!(lines.is_empty());

    let (lines, _) = run_ops(NQ, Format::Nq, vec![Box::new(Tail::new(99, Unit::Quads))]);
    assert_eq!(lines, nq_lines());
}

#[test]
fn tree_dedups_and_regroups() {
    // Tree = dedup + dataset-tree regrouping (graph → subject → predicate,
    // first-seen order at each level), emitted at end of stream: the
    // scattered reappearance of subject `a` (line 5) consolidates with its
    // first run, and the named-graph quad sorts after the default graph.
    let l = nq_lines();
    let grouped = vec![
        l[0].clone(), // a p "1"
        l[4].clone(), // a p "5"   (same g/s/p group as line 1)
        l[1].clone(), // a q "2"
        l[3].clone(), // c p "4"
        l[2].clone(), // b p "3" <g>  (second graph, last)
    ];
    let (lines, _) = run_ops(NQ, Format::Nq, vec![Box::new(Tree::new())]);
    assert_eq!(lines, grouped);

    // Duplicated input dedups to the same tree.
    let dup = format!("{NQ}{NQ}");
    let (lines, _) = run_ops(&dup, Format::Nq, vec![Box::new(Tree::new())]);
    assert_eq!(lines, grouped);

    // Same triple in another graph is a different quad.
    let two_graphs = "\
<http://x/s> <http://x/p> \"1\" .
<http://x/s> <http://x/p> \"1\" <http://x/g> .
";
    let (lines, _) = run_ops(two_graphs, Format::Nq, vec![Box::new(Tree::new())]);
    assert_eq!(lines.len(), 2);
}

#[test]
fn tree_write_consolidates_scattered_subjects() {
    // The canonical pretty-print pipeline (`read / tree / write`): a subject
    // interleaved with others still groups into one `;`/`,` stanza.
    let dir = TempDir::new("treewrite");
    let a = dir.file(
        "a.ttl",
        "@prefix ex: <http://x/> .\nex:s ex:p \"1\" .\nex:other ex:p \"x\" .\nex:s ex:p \"2\" ; ex:q \"3\" .\n",
    );
    let spec = PipelineSpec {
        source: SourceSpec::default(),
        before: vec![OpSpec::Tree],
        junction: None,
        after: vec![],
        terminal: TerminalSpec::Write { trig: false },
    };
    let out = run_plan(&spec, &[Input::File(a)]).expect("runs");
    assert_eq!(
        out,
        "@prefix ex: <http://x/> .\n\nex:s ex:p \"1\", \"2\" ;\n\tex:q \"3\" .\n\nex:other ex:p \"x\" .\n"
    );
}

#[test]
fn ops_compose_in_argv_order() {
    // skip 1 then head 2 = quads 2..4.
    let (lines, _) = run_ops(
        NQ,
        Format::Nq,
        vec![
            Box::new(Skip::new(1, Unit::Quads)),
            Box::new(Head::new(2, Unit::Quads)),
        ],
    );
    assert_eq!(lines, nq_lines()[1..3]);

    // tail 3 / head 1 — tail flushes at finish, head still bounds it.
    let (lines, _) = run_ops(
        NQ,
        Format::Nq,
        vec![
            Box::new(Tail::new(3, Unit::Quads)),
            Box::new(Head::new(1, Unit::Quads)),
        ],
    );
    assert_eq!(lines, nq_lines()[2..3]);
}

#[test]
fn prefixes_pass_through_ops() {
    let ttl = "@prefix ex: <http://x/> .\nex:s ex:p ex:o .\n";
    let (lines, prefixes) = run_ops(ttl, Format::Ttl, vec![Box::new(Skip::new(0, Unit::Quads))]);
    assert_eq!(lines.len(), 1);
    assert_eq!(prefixes, vec![("ex".to_owned(), "http://x/".to_owned())]);
}

// ------------------------------------------------- upstream cancellation

/// Serves an endless repetition of one N-Quads line, counting bytes read.
struct Endless {
    line: &'static [u8],
    at: usize,
    served: usize,
}

impl Read for Endless {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let mut n = 0;
        while n < buf.len() {
            let take = (self.line.len() - self.at).min(buf.len() - n);
            buf[n..n + take].copy_from_slice(&self.line[self.at..self.at + take]);
            self.at = (self.at + take) % self.line.len();
            n += take;
        }
        self.served += n;
        Ok(n)
    }
}

#[test]
fn head_bounds_input_io() {
    let mut source = Endless {
        line: b"<http://x/s> <http://x/p> \"v\" .\n",
        at: 0,
        served: 0,
    };
    let collect = SharedCollect::default();
    let mut sink = chain(
        vec![Box::new(Head::new(1, Unit::Quads)) as Box<dyn Op>],
        Box::new(collect.clone()),
    );
    let flow = read_stream(
        &mut source,
        Format::Nq,
        Options::default(),
        &mut *sink,
        &mut |_| {},
    )
    .expect("valid input");
    sink.finish().expect("finish");
    assert_eq!(flow, Flow::Stop);
    assert_eq!(collect.0.lock().expect("lock").lines.len(), 1);
    // One 256 KiB read-loop chunk, not the (endless) file.
    assert!(
        source.served <= 256 * 1024,
        "head kept reading: {} bytes served",
        source.served
    );
}

// ------------------------------------------------------------- junctions

/// Temp-dir guard (mirrors the graphy-store test convention).
struct TempDir(PathBuf);

impl TempDir {
    fn new(name: &str) -> TempDir {
        let dir = std::env::temp_dir().join(format!("graphy-pipe-{}-{name}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        TempDir(dir)
    }

    fn file(&self, name: &str, contents: &str) -> PathBuf {
        let path = self.0.join(name);
        std::fs::write(&path, contents).expect("write temp file");
        path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Shared output buffer for plan::run.
#[derive(Debug, Clone, Default)]
struct SharedOut(Arc<Mutex<Vec<u8>>>);

impl Write for SharedOut {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.lock().expect("out lock").extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn run_plan(spec: &PipelineSpec, inputs: &[Input]) -> io::Result<String> {
    let out = SharedOut::default();
    run(spec, inputs, Box::new(out.clone()), &mut |w| {
        panic!("unexpected warning: {w}")
    })?;
    let bytes = out.0.lock().expect("out lock").clone();
    Ok(String::from_utf8(bytes).expect("utf-8 output"))
}

fn scribe_spec(
    junction: Option<Junction>,
    before: Vec<OpSpec>,
    after: Vec<OpSpec>,
) -> PipelineSpec {
    PipelineSpec {
        source: SourceSpec::default(),
        before,
        junction,
        after,
        terminal: TerminalSpec::Scribe {
            triples_only: false,
        },
    }
}

#[test]
fn concat_preserves_order_merge_preserves_multiset() {
    let dir = TempDir::new("junction");
    let a = dir.file("a.nq", "<http://x/1> <http://x/p> \"a\" .\n");
    let b = dir.file(
        "b.nq",
        "<http://x/2> <http://x/p> \"b\" .\n<http://x/3> <http://x/p> \"c\" .\n",
    );
    let inputs = [Input::File(a), Input::File(b)];

    let out = run_plan(
        &scribe_spec(Some(Junction::Concat), vec![], vec![]),
        &inputs,
    )
    .expect("concat runs");
    assert_eq!(
        out,
        "<http://x/1> <http://x/p> \"a\" .\n<http://x/2> <http://x/p> \"b\" .\n<http://x/3> <http://x/p> \"c\" .\n"
    );

    let out =
        run_plan(&scribe_spec(Some(Junction::Merge), vec![], vec![]), &inputs).expect("merge runs");
    let mut lines: Vec<&str> = out.lines().collect();
    lines.sort_unstable();
    assert_eq!(
        lines,
        vec![
            "<http://x/1> <http://x/p> \"a\" .",
            "<http://x/2> <http://x/p> \"b\" .",
            "<http://x/3> <http://x/p> \"c\" ."
        ]
    );
}

#[test]
fn per_leg_ops_apply_per_input() {
    let dir = TempDir::new("perleg");
    let a = dir.file(
        "a.nq",
        "<http://x/1> <http://x/p> \"a1\" .\n<http://x/1> <http://x/p> \"a2\" .\n",
    );
    let b = dir.file(
        "b.nq",
        "<http://x/2> <http://x/p> \"b1\" .\n<http://x/2> <http://x/p> \"b2\" .\n",
    );
    let inputs = [Input::File(a), Input::File(b)];

    // head 1 per leg → one quad from EACH input.
    let spec = scribe_spec(
        Some(Junction::Concat),
        vec![OpSpec::Head {
            n: 1,
            unit: Unit::Quads,
        }],
        vec![],
    );
    let out = run_plan(&spec, &inputs).expect("runs");
    assert_eq!(
        out,
        "<http://x/1> <http://x/p> \"a1\" .\n<http://x/2> <http://x/p> \"b1\" .\n"
    );
}

#[test]
fn shared_tail_stop_cancels_remaining_inputs() {
    let dir = TempDir::new("tailstop");
    let a = dir.file("a.nq", "<http://x/1> <http://x/p> \"a\" .\n");
    // The second input does not exist: a global stop after the first
    // input's quad must end the pipeline before this file is opened.
    let missing = dir.0.join("never-created.nq");
    let inputs = [Input::File(a), Input::File(missing)];

    let spec = scribe_spec(
        Some(Junction::Concat),
        vec![],
        vec![OpSpec::Head {
            n: 1,
            unit: Unit::Quads,
        }],
    );
    let out = run_plan(&spec, &inputs).expect("stops before the missing input");
    assert_eq!(out, "<http://x/1> <http://x/p> \"a\" .\n");
}

#[test]
fn blank_labels_never_unify_across_inputs() {
    let dir = TempDir::new("labels");
    let a = dir.file("a.nq", "_:x <http://x/p> \"a\" .\n");
    let b = dir.file("b.nq", "_:x <http://x/p> \"b\" .\n");
    let inputs = [Input::File(a), Input::File(b)];

    let spec = PipelineSpec {
        source: SourceSpec::default(),
        before: vec![],
        junction: Some(Junction::Concat),
        after: vec![],
        terminal: TerminalSpec::Distinct {
            by: DistinctBy::Subjects,
        },
    };
    let out = run_plan(&spec, &inputs).expect("runs");
    assert_eq!(
        out.trim(),
        "2",
        "same surface label in two files must stay distinct"
    );
}

#[test]
fn count_and_distinct_results() {
    let dir = TempDir::new("counts");
    let a = dir.file("a.nq", NQ);
    let inputs = [Input::File(a)];

    let mut spec = scribe_spec(None, vec![], vec![]);
    spec.terminal = TerminalSpec::Count;
    assert_eq!(run_plan(&spec, &inputs).expect("runs").trim(), "5");

    for (by, expect) in [
        (DistinctBy::Quads, "5"),
        (DistinctBy::Triples, "5"),
        (DistinctBy::Subjects, "3"),
        (DistinctBy::Predicates, "2"),
        (DistinctBy::Objects, "5"),
        (DistinctBy::Graphs, "2"), // default graph + <http://x/g>
    ] {
        spec.terminal = TerminalSpec::Distinct { by };
        assert_eq!(
            run_plan(&spec, &inputs).expect("runs").trim(),
            expect,
            "{by:?}"
        );
    }
}

#[test]
fn pretty_write_uses_input_prefixes() {
    let dir = TempDir::new("pretty");
    let a = dir.file(
        "a.ttl",
        "@prefix ex: <http://x/> .\nex:s a ex:C ; ex:p \"v\", \"w\" .\n",
    );
    let inputs = [Input::File(a)];
    let spec = PipelineSpec {
        source: SourceSpec::default(),
        before: vec![],
        junction: None,
        after: vec![],
        terminal: TerminalSpec::Write { trig: false },
    };
    let out = run_plan(&spec, &inputs).expect("runs");
    assert_eq!(
        out,
        "@prefix ex: <http://x/> .\n\nex:s a ex:C ;\n\tex:p \"v\", \"w\" .\n"
    );
}
