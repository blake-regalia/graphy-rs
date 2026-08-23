//! Pipeline event model (docs/09 §3). Quads flow between stages as borrowed
//! concise bytes — byte equality ⇔ term equality, so downstream operators
//! compare slices, never decoded terms. Prefix declarations ride along so
//! pretty serializers (and, from C2, terse-form regexes) see the input's
//! namespace map without a side channel.

use std::io;

use graphy_turtle::{QuadRef, Shorthand};

/// One pipeline event, valid for the duration of the callback (the same
/// lifetime contract as the parsers' `drain`); buffering stages copy.
#[derive(Debug, Clone, Copy)]
pub enum Event<'a> {
    Quad(QuadRef<'a>),
    /// A prefix declaration observed by a source (Turtle/TriG only).
    /// Operators forward these; serializing sinks may use them for
    /// compaction, every other terminal ignores them.
    Prefix {
        name: &'a str,
        iri: &'a str,
    },
}

/// Downstream verdict. `Stop` propagates stage by stage back to the source,
/// whose read loop stops consuming input — `head 10` on a huge file reads
/// O(chunk), not the file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Flow {
    Continue,
    Stop,
}

/// A stage that consumes events: the rest of a chain, or a terminal.
/// `Send` so per-input legs can run on scoped threads.
pub trait Sink: Send {
    fn event(&mut self, ev: Event<'_>) -> io::Result<Flow>;
    /// End of stream: flush buffered state. Called exactly once, after the
    /// last event (even when the stream ended early via [`Flow::Stop`]).
    fn finish(&mut self) -> io::Result<()>;
}

impl<S: Sink + ?Sized> Sink for Box<S> {
    fn event(&mut self, ev: Event<'_>) -> io::Result<Flow> {
        (**self).event(ev)
    }

    fn finish(&mut self) -> io::Result<()> {
        (**self).finish()
    }
}

/// A quad copied out of a parser arena: one allocation, term offsets into it.
/// Equality/hashing follow concise-byte identity — the writer-facing
/// [`Shorthand`] is display metadata and excluded (`42` and
/// `"42"^^xsd:integer` are the same quad).
#[derive(Debug, Clone)]
pub struct OwnedQuad {
    bytes: Box<[u8]>,
    p_at: u32,
    o_at: u32,
    /// `u32::MAX` = default graph.
    g_at: u32,
    shorthand: Option<Shorthand>,
}

impl PartialEq for OwnedQuad {
    fn eq(&self, other: &OwnedQuad) -> bool {
        (self.g_at, self.p_at, self.o_at, &self.bytes)
            == (other.g_at, other.p_at, other.o_at, &other.bytes)
    }
}

impl Eq for OwnedQuad {}

impl std::hash::Hash for OwnedQuad {
    fn hash<H: std::hash::Hasher>(&self, h: &mut H) {
        self.bytes.hash(h);
        self.p_at.hash(h);
        self.o_at.hash(h);
        self.g_at.hash(h);
    }
}

impl OwnedQuad {
    pub fn from_ref(q: &QuadRef<'_>) -> OwnedQuad {
        let g_len = q.g.map_or(0, <[u8]>::len);
        let mut bytes = Vec::with_capacity(q.s.len() + q.p.len() + q.o.len() + g_len);
        bytes.extend_from_slice(q.s);
        let p_at = bytes.len() as u32;
        bytes.extend_from_slice(q.p);
        let o_at = bytes.len() as u32;
        bytes.extend_from_slice(q.o);
        let g_at = match q.g {
            Some(g) => {
                let at = bytes.len() as u32;
                bytes.extend_from_slice(g);
                at
            }
            None => u32::MAX,
        };
        OwnedQuad {
            bytes: bytes.into_boxed_slice(),
            p_at,
            o_at,
            g_at,
            shorthand: q.shorthand,
        }
    }

    pub fn as_ref(&self) -> QuadRef<'_> {
        let o_end = if self.g_at == u32::MAX {
            self.bytes.len()
        } else {
            self.g_at as usize
        };
        QuadRef {
            s: &self.bytes[..self.p_at as usize],
            p: &self.bytes[self.p_at as usize..self.o_at as usize],
            o: &self.bytes[self.o_at as usize..o_end],
            g: (self.g_at != u32::MAX).then(|| &self.bytes[self.g_at as usize..]),
            shorthand: self.shorthand,
        }
    }
}

/// Owned batch of events for crossing a thread boundary (merge legs → the
/// junction): one arena, offset records, amortized allocation.
#[derive(Debug, Default)]
pub struct EventBatch {
    bytes: Vec<u8>,
    items: Vec<Item>,
}

#[derive(Debug, Clone, Copy)]
enum Item {
    /// Term boundaries within `bytes`: s = start..s_end, p = s_end..p_end,
    /// o = p_end..o_end, g = o_end..g_end (`g_end == o_end` = default graph).
    Quad {
        start: u32,
        s_end: u32,
        p_end: u32,
        o_end: u32,
        g_end: u32,
        shorthand: Option<Shorthand>,
    },
    Prefix {
        start: u32,
        name_end: u32,
        iri_end: u32,
    },
}

impl EventBatch {
    pub fn push(&mut self, ev: &Event<'_>) {
        let start = self.bytes.len() as u32;
        match ev {
            Event::Quad(q) => {
                self.bytes.extend_from_slice(q.s);
                let s_end = self.bytes.len() as u32;
                self.bytes.extend_from_slice(q.p);
                let p_end = self.bytes.len() as u32;
                self.bytes.extend_from_slice(q.o);
                let o_end = self.bytes.len() as u32;
                if let Some(g) = q.g {
                    self.bytes.extend_from_slice(g);
                }
                let g_end = self.bytes.len() as u32;
                self.items.push(Item::Quad {
                    start,
                    s_end,
                    p_end,
                    o_end,
                    g_end,
                    shorthand: q.shorthand,
                });
            }
            Event::Prefix { name, iri } => {
                self.bytes.extend_from_slice(name.as_bytes());
                let name_end = self.bytes.len() as u32;
                self.bytes.extend_from_slice(iri.as_bytes());
                let iri_end = self.bytes.len() as u32;
                self.items.push(Item::Prefix {
                    start,
                    name_end,
                    iri_end,
                });
            }
        }
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    pub fn byte_len(&self) -> usize {
        self.bytes.len()
    }

    pub fn clear(&mut self) {
        self.bytes.clear();
        self.items.clear();
    }

    pub fn events(&self) -> impl Iterator<Item = Event<'_>> {
        self.items.iter().map(|item| match *item {
            Item::Quad {
                start,
                s_end,
                p_end,
                o_end,
                g_end,
                shorthand,
            } => Event::Quad(QuadRef {
                s: &self.bytes[start as usize..s_end as usize],
                p: &self.bytes[s_end as usize..p_end as usize],
                o: &self.bytes[p_end as usize..o_end as usize],
                g: (g_end != o_end).then(|| &self.bytes[o_end as usize..g_end as usize]),
                shorthand,
            }),
            Item::Prefix {
                start,
                name_end,
                iri_end,
            } => Event::Prefix {
                // Written from `&str` in `push`; slicing at the recorded
                // boundaries recovers the original UTF-8.
                name: std::str::from_utf8(&self.bytes[start as usize..name_end as usize])
                    .expect("batch prefix bytes came from &str"),
                iri: std::str::from_utf8(&self.bytes[name_end as usize..iri_end as usize])
                    .expect("batch prefix bytes came from &str"),
            },
        })
    }
}
