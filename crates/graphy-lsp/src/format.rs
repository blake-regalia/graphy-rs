//! Pretty-printing (docs/10 §10 tier 2): the graphy canonical pipeline
//! `read / tree / write` (docs/09) as an editor action — dedup + dataset-tree
//! regroup (graph → subject → predicate, first-seen order) through
//! [`graphy_pipe::Tree`], serialized by the pretty writer with the document's
//! own prefix header.
//!
//! Contract: **refuse to format anything that doesn't parse cleanly** (docs/10
//! §10 — never mangle a broken buffer); a strict parse is pass 1, which also
//! decides Turtle vs TriG output (named-graph quads force TriG). Documents
//! with relative IRIs parse against the document's own URI, so their
//! references come back absolutized — the pipeline stores absolute IRIs only.

use std::io::{self, Write};
use std::sync::{Arc, Mutex};

use graphy_pipe::{chain, read_stream, Event, Flow, Format, Op, PrettySink, Sink, Tree};
use graphy_turtle::Options;

/// Pass 1: strict parse probe. `Some((quads, named_graphs))` on success.
struct Probe {
    quads: u64,
    graphs: bool,
}

impl Sink for Probe {
    fn event(&mut self, ev: Event<'_>) -> io::Result<Flow> {
        if let Event::Quad(q) = ev {
            self.quads += 1;
            self.graphs |= q.g.is_some();
        }
        Ok(Flow::Continue)
    }

    fn finish(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// A `Vec<u8>` behind a handle, because pipe terminals want an owned
/// `Box<dyn Write + Send>`.
#[derive(Clone, Default)]
struct SharedBuf(Arc<Mutex<Vec<u8>>>);

impl Write for SharedBuf {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.lock().expect("no panics hold the lock").write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn options(base: Option<&str>) -> Options {
    Options {
        base: base.map(String::from),
        ..Options::default()
    }
}

fn run(src: &str, base: Option<&str>, sink: &mut dyn Sink) -> bool {
    let mut input: &[u8] = src.as_bytes();
    let ok = read_stream(
        &mut input,
        Format::Trig, // superset dialect, same choice as the diagnostics tier
        options(base),
        sink,
        &mut |_| {},
    )
    .is_ok();
    ok && sink.finish().is_ok()
}

/// Canonical pretty-print of a Turtle-family document, or `None` when the
/// document has no data statements or does not parse cleanly. `base` is the
/// document's own URI; it is only consulted when the document actually
/// contains relative references (so fully-absolute documents never change
/// their IRI spellings).
pub fn turtle_pretty(src: &str, base: Option<&str>) -> Option<String> {
    // Strict probe, without a base first: succeeds for self-contained
    // documents. Only documents that *need* a base get one (and with it,
    // absolutized relative references — the only meaning-preserving option).
    let mut probe = Probe {
        quads: 0,
        graphs: false,
    };
    let base = if run(src, None, &mut probe) {
        None
    } else {
        probe = Probe {
            quads: 0,
            graphs: false,
        };
        if !base.is_some_and(|b| run(src, Some(b), &mut probe)) {
            return None;
        }
        base
    };
    if probe.quads == 0 {
        return None; // prefix-only / empty docs: nothing to canonicalize
    }

    let buf = SharedBuf::default();
    let ops: Vec<Box<dyn Op>> = vec![Box::new(Tree::new())];
    let mut sink = chain(
        ops,
        Box::new(PrettySink::new(Box::new(buf.clone()), probe.graphs)),
    );
    if !run(src, base, sink.as_mut()) {
        return None;
    }
    drop(sink);
    let bytes = std::mem::take(&mut *buf.0.lock().expect("pipeline done"));
    String::from_utf8(bytes).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use graphy_turtle::{Options, TriGParser};

    /// The distinct quad set of a document, as sorted concise-byte rows.
    fn quad_set(src: &str, base: Option<&str>) -> Vec<Vec<u8>> {
        let mut p = TriGParser::new(Options {
            base: base.map(String::from),
            ..Options::default()
        })
        .unwrap();
        let mut rows = std::collections::BTreeSet::new();
        p.read_from(src.as_bytes(), |q| {
            let mut row = q.s.to_vec();
            row.push(0);
            row.extend_from_slice(q.p);
            row.push(0);
            row.extend_from_slice(q.o);
            row.push(0);
            row.extend_from_slice(q.g.unwrap_or(b""));
            rows.insert(row);
        })
        .unwrap();
        rows.into_iter().collect()
    }

    #[test]
    fn regroups_scattered_subjects_and_dedupes() {
        // ex:s appears twice, split by ex:other; one triple is a duplicate.
        let src = "@prefix ex: <http://e/> .\n\
                   ex:s ex:p ex:o .\n\
                   ex:other ex:q 1 .\n\
                   ex:s ex:p2 \"x\" .\n\
                   ex:s ex:p ex:o .\n";
        let out = turtle_pretty(src, None).expect("formats");
        // Same quad set (dedup only removes the exact duplicate)...
        assert_eq!(quad_set(&out, None), quad_set(src, None));
        // ...one stanza per subject: each subject IRI appears exactly once.
        assert_eq!(out.matches("ex:s ").count(), 1, "{out}");
        assert_eq!(out.matches("ex:other").count(), 1, "{out}");
        // ...and the prefix header survives for compaction.
        assert!(out.contains("@prefix ex:"), "{out}");
        // Numeric shorthand survives the tree (OwnedQuad carries it).
        assert!(out.contains(" 1 ") || out.contains(" 1 ."), "{out}");
        assert!(!out.contains("XMLSchema#integer"), "{out}");
    }

    #[test]
    fn idempotent_on_its_own_output() {
        let src = "@prefix ex: <http://e/> .\n\
                   ex:b ex:p ex:o .\nex:a ex:q ( 1 2 ) .\nex:b ex:r \"z\"@en .\n";
        let once = turtle_pretty(src, None).expect("formats");
        let twice = turtle_pretty(&once, None).expect("formats its own output");
        assert_eq!(once, twice);
    }

    #[test]
    fn named_graphs_come_back_as_trig() {
        let src = "@prefix ex: <http://e/> .\n\
                   ex:g1 { ex:s ex:p ex:o . }\nex:s ex:d ex:e .\n";
        let out = turtle_pretty(src, None).expect("formats");
        assert_eq!(quad_set(&out, None), quad_set(src, None));
    }

    #[test]
    fn collections_survive_the_canonical_pipeline() {
        // `( … )` must come back as collection syntax, not rdf:first chains
        // (spine stanzas are captured post-tree and spliced at their single
        // reference — the fresh-label invariant makes that safe).
        let src = "@prefix ex: <http://e/> .\n\
                   ex:s ex:q ( 1 ( 2 3 ) \"x\" ) ; ex:p ex:o .\n\
                   ex:t ex:u () .\n";
        let out = turtle_pretty(src, None).expect("formats");
        assert!(
            out.contains("ex:q (\n\t1\n\t(\n\t\t2\n\t\t3\n\t)\n\t\"x\"\n) ;"),
            "{out}"
        );
        assert!(out.contains("ex:u ()"), "{out}");
        assert!(!out.contains("rdf-syntax-ns#first"), "{out}");
        assert_eq!(quad_set(&out, None), quad_set(src, None));
        // And stays stable under reformatting.
        assert_eq!(turtle_pretty(&out, None).as_deref(), Some(out.as_str()));
    }

    #[test]
    fn collections_inline_when_subject_precedes_its_lists() {
        // Regression: when the subject has an earlier statement it outranks
        // its list spines in tree order, so the reference renders BEFORE the
        // spines arrive — the writer must defer (hole) and splice.
        let src = "@prefix mf: <http://www.w3.org/2001/sw/DataAccess/tests/test-manifest#> .\n\
                   @prefix ex: <http://example.org/> .\n\
                   ex:m a mf:Manifest .\n\
                   ex:m mf:entries ( ex:t1 ex:t2 ex:t3 ) .\n\
                   ex:m ex:matrix ( ( 1 2 ) ( 3 4 ) ) .\n";
        let out = turtle_pretty(src, None).expect("formats");
        assert!(
            out.contains("mf:entries (\n\t\tex:t1\n\t\tex:t2\n\t\tex:t3\n\t) ;"),
            "{out}"
        );
        assert!(
            out.contains("ex:matrix (\n\t\t(\n\t\t\t1\n\t\t\t2\n\t\t)"),
            "{out}"
        );
        assert!(!out.contains("rdf-syntax-ns#first"), "{out}");
        assert_eq!(quad_set(&out, None), quad_set(src, None));
        assert_eq!(turtle_pretty(&out, None).as_deref(), Some(out.as_str()));
    }

    #[test]
    fn anonymous_blank_nodes_round_trip_as_anonymous() {
        // Anonymous in, anonymous out — and byte-stable under reformat.
        let src = "@prefix earl: <http://www.w3.org/ns/earl#> .\n\
                   @prefix : <http://e/> .\n\
                   :a earl:assertions [\n\
                   \ta earl:Assertion ;\n\
                   \tearl:result [\n\
                   \t\tearl:outcome earl:passed\n\
                   \t]\n\
                   ] .\n";
        let out = turtle_pretty(src, None).expect("formats");
        assert!(
            out.contains("earl:assertions [\n\ta earl:Assertion ;\n\tearl:result [\n\t\tearl:outcome earl:passed\n\t]\n] ."),
            "{out}"
        );
        assert!(!out.contains("_:b"), "{out}");
        assert_eq!(
            quad_set(&out, None).len(),
            quad_set(src, None).len(),
            "{out}"
        );
        assert_eq!(turtle_pretty(&out, None).as_deref(), Some(out.as_str()));

        // Zero-property anon object stays `[]`; subject-anons come back as
        // `[] …` stanzas.
        let src = "@prefix ex: <http://e/> .\nex:s ex:p [] .\n[] ex:q 1 .\n";
        let out = turtle_pretty(src, None).expect("formats");
        assert!(out.contains("ex:p []"), "{out}");
        assert!(out.contains("[] ex:q 1 ."), "{out}");
        assert!(!out.contains("_:b"), "{out}");
    }

    #[test]
    fn refuses_broken_and_empty_docs() {
        assert_eq!(turtle_pretty("ex:s ex:p BROKEN .", None), None);
        assert_eq!(turtle_pretty("", None), None);
        assert_eq!(turtle_pretty("@prefix ex: <http://e/> .\n", None), None);
    }

    #[test]
    fn relative_iris_format_against_the_doc_base() {
        let src = "<> <p> <o> .";
        assert_eq!(turtle_pretty(src, None), None); // needs a base
        let out = turtle_pretty(src, Some("http://doc.example/d.ttl")).expect("formats");
        assert_eq!(
            quad_set(&out, None),
            quad_set(src, Some("http://doc.example/d.ttl"))
        );
        // Absolute-only docs never consult the base (spellings untouched).
        let abs = "<http://a/s> <http://a/p> <http://a/o> .";
        let out = turtle_pretty(abs, Some("http://doc.example/d.ttl")).expect("formats");
        assert!(out.contains("<http://a/s>"));
    }
}
