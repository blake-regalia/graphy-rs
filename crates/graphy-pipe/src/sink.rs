//! Terminal sinks (docs/09 §2): serializers (`scribe`/`write`) and
//! result-valued statistics (`count`/`distinct` — a JSON number on stdout,
//! per the original's ResultValueStream contract).

use std::collections::HashSet;
use std::fmt;
use std::io::{self, BufWriter, Write};

use graphy_turtle::{NQuadsWriter, TurtleWriter};
use xxhash_rust::xxh3::Xxh3Builder;

use crate::event::{Event, Flow, Sink};

/// Where terminals write. Boxed so legs/threading stay object-safe.
pub type Out = Box<dyn Write + Send>;

/// `scribe`: canonical N-Quads (or N-Triples with `triples_only`, which
/// rejects named-graph quads rather than silently dropping graphs).
pub struct NqSink {
    w: Option<NQuadsWriter<BufWriter<Out>>>,
    triples_only: bool,
}

impl fmt::Debug for NqSink {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("NqSink")
    }
}

impl NqSink {
    pub fn new(out: Out, triples_only: bool) -> NqSink {
        NqSink {
            w: Some(NQuadsWriter::new(BufWriter::new(out))),
            triples_only,
        }
    }
}

impl Sink for NqSink {
    fn event(&mut self, ev: Event<'_>) -> io::Result<Flow> {
        match ev {
            Event::Quad(q) => {
                if self.triples_only && q.g.is_some() {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "named-graph quad in N-Triples output (use -c nq)",
                    ));
                }
                self.w.as_mut().expect("sink not finished").write_quad(&q)?;
                Ok(Flow::Continue)
            }
            Event::Prefix { .. } => Ok(Flow::Continue),
        }
    }

    fn finish(&mut self) -> io::Result<()> {
        self.w
            .take()
            .expect("finish called once")
            .into_inner()
            .flush()
    }
}

/// `write`: pretty Turtle/TriG. Prefix events arriving before the first quad
/// become the `@prefix` header (first declaration of a name wins — the
/// header can carry only one expansion per name); later declarations are
/// ignored for compaction. Output is correct either way — compaction only
/// ever uses mappings present in the emitted header.
pub struct PrettySink {
    trig: bool,
    prefixes: Vec<(String, String)>,
    out: Option<Out>,
    w: Option<TurtleWriter<BufWriter<Out>>>,
}

impl fmt::Debug for PrettySink {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("PrettySink")
    }
}

impl PrettySink {
    pub fn new(out: Out, trig: bool) -> PrettySink {
        PrettySink {
            trig,
            prefixes: Vec::new(),
            out: Some(out),
            w: None,
        }
    }
}

impl Sink for PrettySink {
    fn event(&mut self, ev: Event<'_>) -> io::Result<Flow> {
        match ev {
            Event::Prefix { name, iri } => {
                if self.w.is_none() && !self.prefixes.iter().any(|(n, _)| n == name) {
                    self.prefixes.push((name.to_owned(), iri.to_owned()));
                }
                Ok(Flow::Continue)
            }
            Event::Quad(q) => {
                if !self.trig && q.g.is_some() {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "named-graph quad in Turtle output (use -c trig)",
                    ));
                }
                if self.w.is_none() {
                    let out = self.out.take().expect("writer built once");
                    let mut w = TurtleWriter::new(BufWriter::new(out));
                    if self.trig {
                        w = w.trig();
                    }
                    for (name, iri) in &self.prefixes {
                        w = w.prefix(name, iri);
                    }
                    self.w = Some(w);
                }
                self.w.as_mut().expect("built above").write_quad(&q)?;
                Ok(Flow::Continue)
            }
        }
    }

    fn finish(&mut self) -> io::Result<()> {
        match self.w.take() {
            Some(w) => w.finish()?.flush(),
            // No quads: nothing was written (headers only appear with data).
            None => Ok(()),
        }
    }
}

/// `count`: quad count as a JSON number.
pub struct CountSink {
    n: u64,
    out: Option<Out>,
}

impl fmt::Debug for CountSink {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "CountSink({})", self.n)
    }
}

impl CountSink {
    pub fn new(out: Out) -> CountSink {
        CountSink {
            n: 0,
            out: Some(out),
        }
    }
}

impl Sink for CountSink {
    fn event(&mut self, ev: Event<'_>) -> io::Result<Flow> {
        if let Event::Quad(_) = ev {
            self.n += 1;
        }
        Ok(Flow::Continue)
    }

    fn finish(&mut self) -> io::Result<()> {
        let mut out = self.out.take().expect("finish called once");
        writeln!(out, "{}", self.n)?;
        out.flush()
    }
}

/// `distinct` projection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DistinctBy {
    Quads,
    Triples,
    Subjects,
    Predicates,
    Objects,
    Graphs,
}

/// `distinct`: unique-item count under a projection, as a JSON number.
pub struct DistinctSink {
    by: DistinctBy,
    seen: HashSet<Box<[u8]>, Xxh3Builder>,
    out: Option<Out>,
}

impl fmt::Debug for DistinctSink {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "DistinctSink({:?}, {} seen)", self.by, self.seen.len())
    }
}

impl DistinctSink {
    pub fn new(by: DistinctBy, out: Out) -> DistinctSink {
        DistinctSink {
            by,
            seen: HashSet::with_hasher(Xxh3Builder::new()),
            out: Some(out),
        }
    }
}

/// Composite keys are length-prefixed per term so distinct term splits can
/// never alias; single-term keys are the raw concise bytes (byte equality ⇔
/// term equality). Default graph = the `u32::MAX` marker / empty key, which
/// no concise term can produce.
fn push_term(key: &mut Vec<u8>, term: &[u8]) {
    key.extend_from_slice(&(term.len() as u32).to_le_bytes());
    key.extend_from_slice(term);
}

impl Sink for DistinctSink {
    fn event(&mut self, ev: Event<'_>) -> io::Result<Flow> {
        let q = match ev {
            Event::Quad(q) => q,
            Event::Prefix { .. } => return Ok(Flow::Continue),
        };
        let key: Box<[u8]> = match self.by {
            DistinctBy::Subjects => q.s.into(),
            DistinctBy::Predicates => q.p.into(),
            DistinctBy::Objects => q.o.into(),
            DistinctBy::Graphs => q.g.unwrap_or(b"").into(),
            DistinctBy::Triples | DistinctBy::Quads => {
                let mut key = Vec::new();
                push_term(&mut key, q.s);
                push_term(&mut key, q.p);
                push_term(&mut key, q.o);
                if self.by == DistinctBy::Quads {
                    match q.g {
                        Some(g) => push_term(&mut key, g),
                        None => key.extend_from_slice(&u32::MAX.to_le_bytes()),
                    }
                }
                key.into()
            }
        };
        self.seen.insert(key);
        Ok(Flow::Continue)
    }

    fn finish(&mut self) -> io::Result<()> {
        let mut out = self.out.take().expect("finish called once");
        writeln!(out, "{}", self.seen.len())?;
        out.flush()
    }
}
