//! Quad-manipulation operators (docs/09 §2): stream slicing by quads or
//! subject groups, and streaming dedup. Operators are middleware — they see
//! each event once and push zero or more events to the rest of the chain.

use std::collections::{HashMap, HashSet, VecDeque};
use std::fmt;
use std::io;

use xxhash_rust::xxh3::Xxh3Builder;

use crate::event::{Event, Flow, OwnedQuad, Sink};

/// Middleware stage: transforms/filters events flowing to `out`.
pub trait Op: Send {
    fn event(&mut self, ev: Event<'_>, out: &mut dyn Sink) -> io::Result<Flow>;

    /// End of stream: flush buffered quads downstream. Must NOT call
    /// `out.finish()` — the chain does that exactly once.
    fn finish(&mut self, out: &mut dyn Sink) -> io::Result<()> {
        let _ = out;
        Ok(())
    }
}

/// Compose operators onto a terminal (first op sees events first). The
/// terminal may borrow (`'a`) — per-input legs feed a shared downstream.
pub fn chain<'a>(ops: Vec<Box<dyn Op>>, terminal: Box<dyn Sink + 'a>) -> Box<dyn Sink + 'a> {
    let mut sink = terminal;
    for op in ops.into_iter().rev() {
        sink = Box::new(Stage { op, next: sink });
    }
    sink
}

struct Stage<'a> {
    op: Box<dyn Op>,
    next: Box<dyn Sink + 'a>,
}

impl fmt::Debug for Stage<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Stage")
    }
}

impl Sink for Stage<'_> {
    fn event(&mut self, ev: Event<'_>) -> io::Result<Flow> {
        self.op.event(ev, &mut *self.next)
    }

    fn finish(&mut self) -> io::Result<()> {
        self.op.finish(&mut *self.next)?;
        self.next.finish()
    }
}

/// What `skip`/`head`/`tail` count.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Unit {
    Quads,
    /// Consecutive same-subject runs in stream order (a subject that
    /// reappears later starts a new group, exactly like the original).
    Subjects,
}

/// Tracks consecutive same-subject runs.
#[derive(Debug, Default)]
struct Groups {
    cur: Vec<u8>,
    begun: u64,
}

impl Groups {
    /// Observe a quad's subject; returns how many groups have begun.
    fn observe(&mut self, s: &[u8]) -> u64 {
        if self.begun == 0 || self.cur != s {
            self.cur.clear();
            self.cur.extend_from_slice(s);
            self.begun += 1;
        }
        self.begun
    }
}

/// `skip [n]`: drop the first n quads (or subject groups), forward the rest.
#[derive(Debug)]
pub struct Skip {
    n: u64,
    unit: Unit,
    quads_seen: u64,
    groups: Groups,
}

impl Skip {
    pub fn new(n: u64, unit: Unit) -> Skip {
        Skip {
            n,
            unit,
            quads_seen: 0,
            groups: Groups::default(),
        }
    }
}

impl Op for Skip {
    fn event(&mut self, ev: Event<'_>, out: &mut dyn Sink) -> io::Result<Flow> {
        match ev {
            Event::Quad(q) => {
                let past = match self.unit {
                    Unit::Quads => {
                        self.quads_seen += 1;
                        self.quads_seen > self.n
                    }
                    Unit::Subjects => self.groups.observe(q.s) > self.n,
                };
                if past {
                    out.event(ev)
                } else {
                    Ok(Flow::Continue)
                }
            }
            Event::Prefix { .. } => out.event(ev),
        }
    }
}

/// `head [n]`: forward the first n quads (or subject groups), then stop the
/// stream — the source's read loop halts, so upstream I/O is bounded.
#[derive(Debug)]
pub struct Head {
    n: u64,
    unit: Unit,
    quads_out: u64,
    groups: Groups,
    stopped: bool,
}

impl Head {
    pub fn new(n: u64, unit: Unit) -> Head {
        Head {
            n,
            unit,
            quads_out: 0,
            groups: Groups::default(),
            stopped: false,
        }
    }
}

impl Op for Head {
    fn event(&mut self, ev: Event<'_>, out: &mut dyn Sink) -> io::Result<Flow> {
        match ev {
            Event::Quad(q) => {
                if self.stopped {
                    return Ok(Flow::Stop);
                }
                match self.unit {
                    Unit::Quads => {
                        if self.n == 0 {
                            self.stopped = true;
                            return Ok(Flow::Stop);
                        }
                        self.quads_out += 1;
                        let flow = out.event(ev)?;
                        if self.quads_out >= self.n {
                            self.stopped = true;
                            return Ok(Flow::Stop);
                        }
                        Ok(flow)
                    }
                    Unit::Subjects => {
                        if self.groups.observe(q.s) > self.n {
                            self.stopped = true;
                            return Ok(Flow::Stop);
                        }
                        out.event(ev)
                    }
                }
            }
            Event::Prefix { .. } => out.event(ev),
        }
    }
}

/// `tail [n]`: buffer the stream, emit the last n quads (or subject groups)
/// at end of stream. Prefixes forward immediately (declarations precede use).
#[derive(Debug)]
pub struct Tail {
    n: u64,
    unit: Unit,
    quads: VecDeque<OwnedQuad>,
    groups: VecDeque<Vec<OwnedQuad>>,
    cur_subject: Vec<u8>,
    started: bool,
}

impl Tail {
    pub fn new(n: u64, unit: Unit) -> Tail {
        Tail {
            n,
            unit,
            quads: VecDeque::new(),
            groups: VecDeque::new(),
            cur_subject: Vec::new(),
            started: false,
        }
    }
}

impl Op for Tail {
    fn event(&mut self, ev: Event<'_>, out: &mut dyn Sink) -> io::Result<Flow> {
        match ev {
            Event::Quad(q) => {
                if self.n == 0 {
                    return Ok(Flow::Continue);
                }
                match self.unit {
                    Unit::Quads => {
                        if self.quads.len() as u64 == self.n {
                            self.quads.pop_front();
                        }
                        self.quads.push_back(OwnedQuad::from_ref(&q));
                    }
                    Unit::Subjects => {
                        if !self.started || self.cur_subject != q.s {
                            self.started = true;
                            self.cur_subject.clear();
                            self.cur_subject.extend_from_slice(q.s);
                            if self.groups.len() as u64 == self.n {
                                self.groups.pop_front();
                            }
                            self.groups.push_back(Vec::new());
                        }
                        self.groups
                            .back_mut()
                            .expect("group pushed above")
                            .push(OwnedQuad::from_ref(&q));
                    }
                }
                Ok(Flow::Continue)
            }
            Event::Prefix { .. } => out.event(ev),
        }
    }

    fn finish(&mut self, out: &mut dyn Sink) -> io::Result<()> {
        let quads = std::mem::take(&mut self.quads);
        let groups = std::mem::take(&mut self.groups);
        for q in quads.iter().chain(groups.iter().flatten()) {
            if out.event(Event::Quad(q.as_ref()))? == Flow::Stop {
                break;
            }
        }
        Ok(())
    }
}

/// `tree`: dataset-tree dedup + regrouping, per the original — quads land in
/// a graph → subject → predicate tree (first-seen order at every level) and
/// emit at end of stream, so a downstream `write` consolidates subjects that
/// were scattered through the input (`read / tree / write` is the canonical
/// pretty-print pipeline). Duplicates drop on entry. Memory is O(distinct
/// quads) — the same as the dedup set alone.
pub struct Tree {
    seen: HashSet<OwnedQuad, Xxh3Builder>,
    /// First-seen ranks per level; keys scope each level to its parent so
    /// the same subject under two graphs (or predicate under two subjects)
    /// groups independently. Empty graph key = default graph.
    g_rank: HashMap<Box<[u8]>, u32, Xxh3Builder>,
    s_rank: HashMap<(u32, Box<[u8]>), u32, Xxh3Builder>,
    p_rank: HashMap<(u32, u32, Box<[u8]>), u32, Xxh3Builder>,
    /// (g, s, p) ranks + the quad; Vec order = arrival = object order
    /// within a predicate group (the sort below is stable).
    quads: Vec<(u32, u32, u32, OwnedQuad)>,
}

impl fmt::Debug for Tree {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Tree({} seen)", self.seen.len())
    }
}

impl Tree {
    #[allow(clippy::new_without_default)]
    pub fn new() -> Tree {
        Tree {
            seen: HashSet::with_hasher(Xxh3Builder::new()),
            g_rank: HashMap::with_hasher(Xxh3Builder::new()),
            s_rank: HashMap::with_hasher(Xxh3Builder::new()),
            p_rank: HashMap::with_hasher(Xxh3Builder::new()),
            quads: Vec::new(),
        }
    }
}

impl Op for Tree {
    fn event(&mut self, ev: Event<'_>, out: &mut dyn Sink) -> io::Result<Flow> {
        match ev {
            Event::Quad(q) => {
                let owned = OwnedQuad::from_ref(&q);
                if self.seen.contains(&owned) {
                    return Ok(Flow::Continue);
                }
                self.seen.insert(owned.clone());
                let next_g = self.g_rank.len() as u32;
                let g = *self
                    .g_rank
                    .entry(q.g.unwrap_or(b"").into())
                    .or_insert(next_g);
                let next_s = self.s_rank.len() as u32;
                let s = *self.s_rank.entry((g, q.s.into())).or_insert(next_s);
                let next_p = self.p_rank.len() as u32;
                let p = *self.p_rank.entry((g, s, q.p.into())).or_insert(next_p);
                self.quads.push((g, s, p, owned));
                Ok(Flow::Continue)
            }
            Event::Prefix { .. } => out.event(ev),
        }
    }

    fn finish(&mut self, out: &mut dyn Sink) -> io::Result<()> {
        let mut quads = std::mem::take(&mut self.quads);
        quads.sort_by_key(|&(g, s, p, _)| (g, s, p));
        for (_, _, _, q) in &quads {
            if out.event(Event::Quad(q.as_ref()))? == Flow::Stop {
                break;
            }
        }
        Ok(())
    }
}
