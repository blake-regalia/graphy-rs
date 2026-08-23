//! Parser output: quads of concise-encoded terms borrowed from the parser's
//! arena (doc 03 §2). Consumers intern the concise bytes directly or decode
//! views via [`QuadRef::subject`] et al.

use graphy_core::{concise, GraphName, Quad, Term, TermRef};

/// Parser configuration.
#[derive(Debug, Clone)]
pub struct Options {
    /// Base IRI for resolving relative references (must be absolute).
    pub base: Option<String>,
    /// Accept the RDF 1.2 additions (triple terms, reified triples,
    /// annotations, directional language tags). On by default.
    pub spec12: bool,
    /// Collect errors and resynchronize at the next statement instead of
    /// stopping at the first error (bulk-loading dirty data).
    pub lenient: bool,
    /// Namespace this parser's blank-node labels with the given identifier
    /// (labels come out as `f{ns}b{n}` / `f{ns}s{surface}` instead of
    /// `b{n}` / `s{surface}`). Blank labels are document-scoped, so loaders
    /// combining several inputs into one dataset MUST give each input a
    /// distinct namespace or identical surface labels will incorrectly
    /// unify across documents.
    pub label_ns: Option<u128>,
    /// Skip character-level validation for input trusted to be syntactically
    /// valid (e.g. previously validated dumps or this library's own output).
    /// Structure is still parsed — token boundaries, escapes, resolution,
    /// and UTF-8 all behave identically on valid input — but forbidden-
    /// character checks inside IRIs/strings and the RFC 3986 re-validation
    /// of absolute IRIs are elided. On *invalid* input this mode may accept
    /// or misparse instead of erroring (never unsafely); it is the opposite
    /// trade to `lenient`, which pays extra to recover from bad input.
    pub trusted: bool,
}

impl Default for Options {
    fn default() -> Options {
        Options {
            base: None,
            spec12: true,
            lenient: false,
            label_ns: None,
            trusted: false,
        }
    }
}

/// Object literal came from a Turtle shorthand token, so its datatype is the
/// implied XSD type and the lexical form was validated by the grammar — the
/// loader can attempt inline encoding without a syntax pre-check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Shorthand {
    Integer,
    Decimal,
    Double,
    Boolean,
}

/// (start, end) into an [`Arena`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct H {
    pub start: u32,
    pub end: u32,
}

/// Byte arena of concise term encodings. Statement terms build in a
/// per-statement arena (cleared per statement, so it survives `feed`
/// boundaries mid-statement); finished quads copy into the output buffer's
/// own arena (cleared per feed).
#[derive(Debug, Default)]
pub(crate) struct Arena {
    pub bytes: Vec<u8>,
}

impl Arena {
    pub fn mark(&self) -> usize {
        self.bytes.len()
    }

    /// Handle for the bytes appended since `mark`.
    pub fn handle_from(&self, mark: usize) -> H {
        H {
            start: mark as u32,
            end: self.bytes.len() as u32,
        }
    }

    pub fn get(&self, h: H) -> &[u8] {
        &self.bytes[h.start as usize..h.end as usize]
    }

    pub fn clear(&mut self) {
        self.bytes.clear();
    }
}

#[derive(Debug, Clone, Copy)]
struct QuadRec {
    s: H,
    p: H,
    o: H,
    g: Option<H>,
    shorthand: Option<Shorthand>,
}

/// Quads parsed by the current feed, with their term bytes.
#[derive(Debug, Default)]
pub(crate) struct QuadBuf {
    arena: Arena,
    quads: Vec<QuadRec>,
}

impl QuadBuf {
    /// Copy a finished statement's terms out of its arena and record the
    /// quad. The graph label is raw concise bytes (it may be owned outside
    /// the statement arena, e.g. a TriG graph block spanning statements).
    pub fn push_quad_from(
        &mut self,
        src: &Arena,
        s: H,
        p: H,
        o: H,
        g: Option<&[u8]>,
        shorthand: Option<Shorthand>,
    ) {
        let mut copy = |bytes: &[u8]| {
            let mark = self.arena.mark();
            self.arena.bytes.extend_from_slice(bytes);
            self.arena.handle_from(mark)
        };
        let rec = QuadRec {
            s: copy(src.get(s)),
            p: copy(src.get(p)),
            o: copy(src.get(o)),
            g: g.map(&mut copy),
            shorthand,
        };
        self.quads.push(rec);
    }

    pub fn clear(&mut self) {
        self.arena.clear();
        self.quads.clear();
    }

    pub fn iter(&self) -> impl Iterator<Item = QuadRef<'_>> + '_ {
        self.quads.iter().map(|q| QuadRef {
            s: self.arena.get(q.s),
            p: self.arena.get(q.p),
            o: self.arena.get(q.o),
            g: q.g.map(|g| self.arena.get(g)),
            shorthand: q.shorthand,
        })
    }
}

/// One parsed quad; term fields are concise-encoded bytes valid until the
/// next `feed` on the parser.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QuadRef<'a> {
    pub s: &'a [u8],
    pub p: &'a [u8],
    pub o: &'a [u8],
    /// `None` = default graph.
    pub g: Option<&'a [u8]>,
    /// See [`Shorthand`]; always `None` for N-Triples/N-Quads input.
    pub shorthand: Option<Shorthand>,
}

impl<'a> QuadRef<'a> {
    pub fn subject(&self) -> TermRef<'a> {
        decode(self.s)
    }

    pub fn predicate(&self) -> TermRef<'a> {
        decode(self.p)
    }

    pub fn object(&self) -> TermRef<'a> {
        decode(self.o)
    }

    pub fn graph(&self) -> Option<TermRef<'a>> {
        self.g.map(decode)
    }

    /// Copy into an owned [`Quad`].
    pub fn to_quad(&self) -> Quad {
        let own = |b: &[u8]| Term::from_concise(b).expect("parser emits valid concise terms");
        Quad {
            s: own(self.s),
            p: own(self.p),
            o: own(self.o),
            g: match self.g {
                None => GraphName::Default,
                Some(g) => GraphName::Named(own(g)),
            },
        }
    }
}

fn decode(bytes: &[u8]) -> TermRef<'_> {
    concise::decode(bytes).expect("parser emits valid concise terms")
}
