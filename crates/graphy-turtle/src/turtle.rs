//! Turtle / TriG driver (doc 03 §3): recursive descent over the W3C grammar
//! implemented as an explicit pushdown state machine, resumable at any chunk
//! boundary (the lexer buffers the token in flight; the driver's state and
//! frame stack persist across feeds).
//!
//! RDF 1.2 sugar desugars here — consumers only ever see expanded quads:
//! - `<< s p o ~r >>` (reified triple) emits `r rdf:reifies <<(s p o)>>` and
//!   evaluates to `r` (fresh blank node when the reifier is absent or bare);
//!   the inner triple is *not* asserted.
//! - `o ~r` (reifier annotation) emits `r rdf:reifies <<(s p o)>>` for the
//!   just-asserted triple and sets the current reifier.
//! - `o {| … |}` (annotation block) reuses the current reifier (or mints and
//!   emits a fresh one), parses the block with the reifier as subject, then
//!   clears the current reifier.

use graphy_core::{concise::MAX_TRIPLE_TERM_DEPTH, vocab};

use crate::common::{
    emit_lang, emit_shorthand, emit_simple, emit_triple_term, emit_typed, TermCtx,
};
use crate::lexer::{Lexer, Text, Token};
use crate::quad::{Arena, Options, QuadBuf, QuadRef, Shorthand, H};
use crate::ParseError;

/// Turtle parser (quads land in the default graph).
#[derive(Debug)]
pub struct TurtleParser(TtlParser);

/// TriG parser.
#[derive(Debug)]
pub struct TriGParser(TtlParser);

macro_rules! delegate {
    ($ty:ty) => {
        impl $ty {
            /// Push a chunk (any boundary) and parse eagerly. Quads parsed by
            /// this call replace the previous batch — consume via [`Self::drain`]
            /// before feeding again.
            pub fn feed(&mut self, chunk: &[u8]) -> Result<(), ParseError> {
                self.0.feed(chunk)
            }

            /// Quads parsed by the last `feed`/`finish`, in document order.
            pub fn drain(&mut self) -> impl Iterator<Item = QuadRef<'_>> + '_ {
                self.0.quads.iter()
            }

            /// Signal EOF and validate that the input ends cleanly.
            pub fn finish(&mut self) -> Result<(), ParseError> {
                self.0.finish()
            }

            /// Drive the parser from a [`std::io::Read`], invoking `sink`
            /// for each quad in document order.
            pub fn read_from<R, F>(
                &mut self,
                mut reader: R,
                mut sink: F,
            ) -> Result<(), crate::Error>
            where
                R: std::io::Read,
                F: FnMut(crate::QuadRef<'_>),
            {
                let mut buf = vec![0u8; 64 * 1024];
                loop {
                    let n = reader.read(&mut buf)?;
                    if n == 0 {
                        break;
                    }
                    self.feed(&buf[..n])?;
                    for q in self.drain() {
                        sink(q);
                    }
                }
                self.finish()?;
                for q in self.drain() {
                    sink(q);
                }
                Ok(())
            }

            /// Async variant of [`Self::read_from`] over a runtime-agnostic
            /// [`futures_io::AsyncRead`].
            #[cfg(feature = "async")]
            pub async fn read_from_async<R, F>(
                &mut self,
                mut reader: R,
                mut sink: F,
            ) -> Result<(), crate::Error>
            where
                R: futures_io::AsyncRead + Unpin,
                F: FnMut(crate::QuadRef<'_>),
            {
                let mut buf = vec![0u8; 64 * 1024];
                loop {
                    let n = std::future::poll_fn(|cx| {
                        std::pin::Pin::new(&mut reader).poll_read(cx, &mut buf)
                    })
                    .await?;
                    if n == 0 {
                        break;
                    }
                    self.feed(&buf[..n])?;
                    for q in self.drain() {
                        sink(q);
                    }
                }
                self.finish()?;
                for q in self.drain() {
                    sink(q);
                }
                Ok(())
            }

            /// Errors collected in lenient mode.
            pub fn errors(&self) -> &[ParseError] {
                &self.0.errors
            }

            /// Prefixes declared so far (`@prefix` / `PREFIX`), unordered;
            /// a re-declared name reports its latest IRI. Names are the
            /// surface labels (empty string for the default prefix `:`).
            pub fn prefixes(&self) -> impl Iterator<Item = (&str, &str)> + '_ {
                self.0
                    .ctx
                    .prefixes()
                    .filter_map(|(name, iri)| std::str::from_utf8(name).ok().map(|n| (n, iri)))
            }
        }
    };
}

delegate!(TurtleParser);
delegate!(TriGParser);

impl TurtleParser {
    pub fn new(options: Options) -> Result<TurtleParser, ParseError> {
        Ok(TurtleParser(TtlParser::new(options, false)?))
    }
}

impl TriGParser {
    pub fn new(options: Options) -> Result<TriGParser, ParseError> {
        Ok(TriGParser(TtlParser::new(options, true)?))
    }
}

/// Where a completed term value is delivered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Ret {
    Subject,
    /// Subject whose predicateObjectList is optional (bnode property list or
    /// reified triple as a whole statement).
    SubjectOptionalPol,
    /// TriG statement start: graph label iff '{' follows, else subject.
    StatementNode,
    Object,
    RtSubject,
    RtObject,
    RtReifier,
    TtSubject,
    TtObject,
    GraphName,
    /// Statement-level reifier (`o ~r`): reify and set the current reifier.
    AnnoReifier,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum S {
    /// Statement start: directive, subject, or (TriG) graph constructs.
    Statement,
    PrefixName {
        sparql: bool,
    },
    PrefixIri {
        sparql: bool,
    },
    BaseIri {
        sparql: bool,
    },
    /// `@prefix`/`@base` require a terminating '.'.
    DirectiveDot,
    /// After `@version`/`VERSION`: expect a (short) string specifier.
    VersionString {
        sparql: bool,
    },
    /// TriG: node seen at statement start; graph label iff '{' follows.
    MaybeGraph,
    /// After `GRAPH`.
    GraphName,
    /// Graph label seen: expect '{'.
    GraphOpen,
    /// Subject set: expect verb.
    Verb,
    /// Subject was a bnode property list: verb, '.', or (TriG) '}'.
    VerbOrDot,
    Object,
    /// Object done: annotation, ',', ';', '.', ']', '|}', (TriG) '}'.
    AfterObject,
    /// After ';': verb, more ';', or a terminator.
    AfterSemi,
    /// Literal seen: optional `@lang` / `^^`; delivery target in `lit_ret`.
    AfterString,
    Datatype,
    /// After statement-level '~'.
    AnnoReifier,
    /// Just saw '['; ANON vs property list; target in `bracket_ret`.
    Bracket,
    RtSubject,
    RtPredicate,
    RtObject,
    /// rt object done: '~' or '>>'.
    RtAfterObject,
    /// After '~' inside `<< >>`.
    RtReifierNode,
    /// Reifier set: expect '>>'.
    RtClose,
    TtSubject,
    TtPredicate,
    TtObject,
    /// tt object done: expect ')>>'.
    TtClose,
    /// Lenient mode: discarding bytes through the next '.'.
    Recover,
}

#[derive(Debug)]
enum Frame {
    /// `[ pol ]`: restore subject/predicate on ']' and deliver `b`.
    BNode {
        b: H,
        saved_subject: Option<H>,
        saved_predicate: Option<H>,
        ret: Ret,
    },
    /// `( items )`.
    Collection {
        head: Option<H>,
        tail: Option<H>,
        ret: Ret,
    },
    /// `<< s p o ~r >>`.
    Rt {
        s: Option<H>,
        p: Option<H>,
        o: Option<H>,
        reifier: Option<H>,
        ret: Ret,
    },
    /// `<<( s p o )>>`.
    Tt {
        s: Option<H>,
        p: Option<H>,
        o: Option<H>,
        ret: Ret,
    },
    /// `{| pol |}`: annotation context saved around the block.
    Anno {
        saved_subject: Option<H>,
        saved_predicate: Option<H>,
        saved_last: Option<(H, H, H)>,
        saved_tt: Option<H>,
    },
    /// TriG `{ … }`.
    Graph,
}

#[derive(Debug)]
struct TtlParser {
    lx: Lexer,
    quads: QuadBuf,
    /// Arena for the statement in flight (survives feed boundaries).
    stmt: Arena,
    ctx: TermCtx,
    spec12: bool,
    lenient: bool,
    trig: bool,
    state: S,
    stack: Vec<Frame>,
    subject: Option<H>,
    predicate: Option<H>,
    /// TriG: current graph label, owned so it survives statement resets.
    graph: Option<Vec<u8>>,
    /// TriG MaybeGraph: the node that is either a label or a subject.
    pending_node: Option<H>,
    /// Prefix name awaiting its IRI in a directive.
    pending_prefix: Vec<u8>,
    /// Literal lexical awaiting lang/datatype, and its delivery target.
    pending: Vec<u8>,
    lit_ret: Ret,
    /// Delivery target of a pending '[' (ANON or property list).
    bracket_ret: Ret,
    /// Most recently asserted triple at this annotation level.
    last: Option<(H, H, H)>,
    /// Cached `<<(s p o)>>` of `last` (built on first reifier/annotation).
    cur_tt: Option<H>,
    /// Current reifier (RDF 1.2 annotation state).
    cur_reifier: Option<H>,
    /// Nesting depth of triple-term handles built this statement.
    tt_depths: Vec<(H, u8)>,
    tt_scratch: Vec<u8>,
    errors: Vec<ParseError>,
}

impl TtlParser {
    fn new(options: Options, trig: bool) -> Result<TtlParser, ParseError> {
        let ctx = TermCtx::new(options.base.as_deref(), options.trusted, options.label_ns)
            .map_err(|message| ParseError {
                message: format!("invalid base IRI: {message}"),
                offset: 0,
                line: 0,
                column: 0,
            })?;
        let mut lx = Lexer::new();
        lx.trusted = options.trusted;
        Ok(TtlParser {
            lx,
            quads: QuadBuf::default(),
            stmt: Arena::default(),
            ctx,
            spec12: options.spec12,
            lenient: options.lenient,
            trig,
            state: S::Statement,
            stack: Vec::new(),
            subject: None,
            predicate: None,
            graph: None,
            pending_node: None,
            pending_prefix: Vec::new(),
            pending: Vec::new(),
            lit_ret: Ret::Object,
            bracket_ret: Ret::Object,
            last: None,
            cur_tt: None,
            cur_reifier: None,
            tt_depths: Vec::new(),
            tt_scratch: Vec::new(),
            errors: Vec::new(),
        })
    }

    fn feed(&mut self, chunk: &[u8]) -> Result<(), ParseError> {
        self.quads.clear();
        self.lx.feed(chunk);
        self.pump()
    }

    fn finish(&mut self) -> Result<(), ParseError> {
        self.quads.clear();
        self.lx.set_eof();
        self.pump()
    }

    fn pump(&mut self) -> Result<(), ParseError> {
        loop {
            if self.state == S::Recover {
                if !self.lx.skip_to_statement_end() {
                    return Ok(());
                }
                self.reset_statement();
                self.stack.clear();
                self.graph = None;
                self.state = S::Statement;
                continue;
            }
            match self.lx.next() {
                Ok(None) => return Ok(()),
                Ok(Some(Token::Eof)) => {
                    if self.state != S::Statement || !self.stack.is_empty() {
                        let e = self.lx.err_here("unexpected end of input");
                        self.fail(e)?;
                        continue;
                    }
                    return Ok(());
                }
                Ok(Some(tok)) => {
                    if let Err(e) = self.step(tok) {
                        self.fail(e)?;
                    }
                }
                Err(e) => self.fail(e)?,
            }
        }
    }

    fn fail(&mut self, e: ParseError) -> Result<(), ParseError> {
        if !self.lenient {
            return Err(e);
        }
        self.errors.push(e);
        self.state = S::Recover;
        Ok(())
    }

    fn reset_statement(&mut self) {
        self.subject = None;
        self.predicate = None;
        self.pending_node = None;
        self.last = None;
        self.cur_tt = None;
        self.cur_reifier = None;
        self.tt_depths.clear();
        self.stmt.clear();
    }

    fn err(&self, msg: impl Into<String>) -> ParseError {
        self.lx.err_at(self.lx.token_start(), msg)
    }

    fn require12(&self, what: &str) -> Result<(), ParseError> {
        if self.spec12 {
            Ok(())
        } else {
            Err(self.err(format!("{what} requires RDF 1.2 mode")))
        }
    }

    // ------------------------------------------------------- term helpers

    /// IRI / prefixed name / blank label / ANON-independent simple nodes.
    fn simple_node(&mut self, t: &Token) -> Result<Option<H>, ParseError> {
        match *t {
            Token::Iri(x) => {
                let text = self.lx.text_str(x);
                match self.ctx.emit_iri(&mut self.stmt, text) {
                    Ok(h) => Ok(Some(h)),
                    Err(m) => Err(self.err(m)),
                }
            }
            Token::Pname { prefix, local } => self.expand_pname(prefix, local).map(Some),
            Token::BlankLabel(x) => {
                let surface = self.lx.text(x);
                Ok(Some(self.ctx.emit_blank(&mut self.stmt, surface)))
            }
            _ => Ok(None),
        }
    }

    fn expand_pname(&mut self, prefix: Text, local: Text) -> Result<H, ParseError> {
        let iri = self.pname_iri(prefix, local)?;
        let mark = self.stmt.mark();
        graphy_core::concise::encode_iri(&mut self.stmt.bytes, &iri);
        Ok(self.stmt.handle_from(mark))
    }

    fn pname_iri(&mut self, prefix: Text, local: Text) -> Result<String, ParseError> {
        let prefix_bytes = self.lx.text(prefix);
        let Some(expansion) = self.ctx.expand_prefix(prefix_bytes) else {
            return Err(self.err(format!(
                "undeclared prefix {:?}",
                String::from_utf8_lossy(prefix_bytes)
            )));
        };
        let local = self.lx.text_str(local);
        let mut iri = String::with_capacity(expansion.len() + local.len());
        iri.push_str(expansion);
        iri.push_str(local);
        if crate::common::fast_absolute_lexed(&iri) {
            return Ok(iri);
        }
        match graphy_core::iri::validate_iri(&iri) {
            Ok(()) => Ok(iri),
            Err(e) => Err(self.err(format!("prefixed name expands to invalid IRI: {e}"))),
        }
    }

    fn well_known(&mut self, iri: &'static str) -> H {
        let mark = self.stmt.mark();
        graphy_core::concise::encode_iri(&mut self.stmt.bytes, iri);
        self.stmt.handle_from(mark)
    }

    fn fresh_bnode(&mut self) -> H {
        self.ctx.emit_fresh_blank(&mut self.stmt)
    }

    /// Emit a quad in the current graph.
    fn emit(&mut self, s: H, p: H, o: H, shorthand: Option<Shorthand>) {
        self.quads
            .push_quad_from(&self.stmt, s, p, o, self.graph.as_deref(), shorthand);
    }

    /// The `<<(s p o)>>` term for the current annotation target, cached.
    fn annotation_tt(&mut self) -> Result<H, ParseError> {
        if let Some(h) = self.cur_tt {
            return Ok(h);
        }
        let (s, p, o) = self
            .last
            .ok_or_else(|| self.err("no preceding triple to annotate"))?;
        let h = self.build_tt(s, p, o)?;
        self.cur_tt = Some(h);
        Ok(h)
    }

    // ----------------------------------------------------------- delivery

    fn deliver(&mut self, ret: Ret, h: H, shorthand: Option<Shorthand>) {
        match ret {
            Ret::Subject => {
                self.subject = Some(h);
                self.state = S::Verb;
            }
            Ret::SubjectOptionalPol => {
                self.subject = Some(h);
                self.state = S::VerbOrDot;
            }
            Ret::StatementNode => {
                self.pending_node = Some(h);
                self.state = S::MaybeGraph;
            }
            Ret::Object => self.object_done(h, shorthand),
            Ret::RtSubject => {
                *self.rt_top().s = Some(h);
                self.state = S::RtPredicate;
            }
            Ret::RtObject => {
                *self.rt_top().o = Some(h);
                self.state = S::RtAfterObject;
            }
            Ret::RtReifier => {
                *self.rt_top().reifier = Some(h);
                self.state = S::RtClose;
            }
            Ret::TtSubject => {
                match self.stack.last_mut() {
                    Some(Frame::Tt { s, .. }) => *s = Some(h),
                    _ => unreachable!("Tt state without Tt frame"),
                }
                self.state = S::TtPredicate;
            }
            Ret::TtObject => {
                match self.stack.last_mut() {
                    Some(Frame::Tt { o, .. }) => *o = Some(h),
                    _ => unreachable!("Tt state without Tt frame"),
                }
                self.state = S::TtClose;
            }
            Ret::GraphName => {
                self.graph = Some(self.stmt.get(h).to_vec());
                self.state = S::GraphOpen;
            }
            Ret::AnnoReifier => {
                // Only reachable right after an object, so `last` is set.
                self.reify(h)
                    .expect("annotation follows an asserted triple");
                self.cur_reifier = Some(h);
                self.state = S::AfterObject;
            }
        }
    }

    fn rt_top(&mut self) -> RtView<'_> {
        match self.stack.last_mut() {
            Some(Frame::Rt { s, o, reifier, .. }) => RtView { s, o, reifier },
            _ => unreachable!("Rt state without Rt frame"),
        }
    }

    // ------------------------------------------------------------ stepping

    fn in_graph(&self) -> bool {
        matches!(self.stack.first(), Some(Frame::Graph))
    }

    fn step(&mut self, tok: Token) -> Result<(), ParseError> {
        // A state may finish pending work and re-dispatch the same token.
        let mut tok = Some(tok);
        loop {
            let t = tok.take().expect("token consumed exactly once per return");
            match self.state {
                S::Statement => match t {
                    Token::KwPrefixAt
                    | Token::KwPrefixSparql
                    | Token::KwBaseAt
                    | Token::KwBaseSparql
                        if self.in_graph() =>
                    {
                        return Err(self.err("directives are not allowed inside graph blocks"))
                    }
                    Token::KwPrefixAt => {
                        self.state = S::PrefixName { sparql: false };
                        return Ok(());
                    }
                    Token::KwPrefixSparql => {
                        self.state = S::PrefixName { sparql: true };
                        return Ok(());
                    }
                    Token::KwBaseAt => {
                        self.state = S::BaseIri { sparql: false };
                        return Ok(());
                    }
                    Token::KwBaseSparql => {
                        self.state = S::BaseIri { sparql: true };
                        return Ok(());
                    }
                    Token::KwGraph if self.trig && !self.in_graph() => {
                        self.state = S::GraphName;
                        return Ok(());
                    }
                    Token::LBrace if self.trig && !self.in_graph() => {
                        self.stack.push(Frame::Graph);
                        self.graph = None; // default graph block
                        return Ok(());
                    }
                    Token::RBrace if self.in_graph() => {
                        return self.close_graph();
                    }
                    Token::Iri(_) | Token::Pname { .. } | Token::BlankLabel(_) => {
                        let h = self.simple_node(&t)?.expect("node token");
                        if self.trig && !self.in_graph() {
                            // Graph label iff '{' follows.
                            self.pending_node = Some(h);
                            self.state = S::MaybeGraph;
                        } else {
                            self.subject = Some(h);
                            self.state = S::Verb;
                        }
                        return Ok(());
                    }
                    Token::LBracket => {
                        self.bracket_ret = if self.trig && !self.in_graph() {
                            Ret::StatementNode
                        } else {
                            Ret::Subject
                        };
                        self.state = S::Bracket;
                        return Ok(());
                    }
                    Token::LParen => {
                        self.stack.push(Frame::Collection {
                            head: None,
                            tail: None,
                            ret: Ret::Subject,
                        });
                        self.state = S::Object;
                        return Ok(());
                    }
                    Token::LtLt => {
                        self.require12("reified triples")?;
                        // A reified triple may stand alone as a statement.
                        return self.push_rt(Ret::SubjectOptionalPol);
                    }
                    Token::KwVersionAt | Token::KwVersionSparql if !self.in_graph() => {
                        self.require12("the version directive")?;
                        self.state = S::VersionString {
                            sparql: matches!(t, Token::KwVersionSparql),
                        };
                        return Ok(());
                    }
                    _ => return Err(self.err("expected directive or subject")),
                },
                S::MaybeGraph => match t {
                    Token::LBrace => {
                        let h = self.pending_node.take().expect("pending node set");
                        self.graph = Some(self.stmt.get(h).to_vec());
                        self.stack.push(Frame::Graph);
                        self.reset_statement();
                        self.state = S::Statement;
                        return Ok(());
                    }
                    _ => {
                        self.subject = self.pending_node.take();
                        self.state = S::Verb;
                        tok = Some(t);
                        continue;
                    }
                },
                S::GraphName => match self.simple_node(&t)? {
                    Some(h) => {
                        self.deliver(Ret::GraphName, h, None);
                        return Ok(());
                    }
                    None => match t {
                        Token::LBracket => {
                            self.bracket_ret = Ret::GraphName;
                            self.state = S::Bracket;
                            return Ok(());
                        }
                        _ => return Err(self.err("expected graph label")),
                    },
                },
                S::GraphOpen => match t {
                    Token::LBrace => {
                        self.stack.push(Frame::Graph);
                        self.state = S::Statement;
                        return Ok(());
                    }
                    _ => return Err(self.err("expected '{' after graph label")),
                },
                S::PrefixName { sparql } => match t {
                    Token::Pname { prefix, local } => {
                        if !self.lx.text(local).is_empty() {
                            return Err(self.err("expected a prefix declaration like 'ex:'"));
                        }
                        let p = self.lx.text(prefix);
                        self.pending_prefix.clear();
                        self.pending_prefix.extend_from_slice(p);
                        self.state = S::PrefixIri { sparql };
                        return Ok(());
                    }
                    _ => return Err(self.err("expected prefix name")),
                },
                S::PrefixIri { sparql } => match t {
                    Token::Iri(x) => {
                        let text = self.lx.text_str(x);
                        let abs = match self.ctx.resolve_iri(text) {
                            Ok(abs) => abs.into_owned(),
                            Err(m) => return Err(self.err(m)),
                        };
                        let prefix = std::mem::take(&mut self.pending_prefix);
                        self.ctx.set_prefix(&prefix, abs);
                        self.pending_prefix = prefix;
                        self.state = if sparql {
                            S::Statement
                        } else {
                            S::DirectiveDot
                        };
                        return Ok(());
                    }
                    _ => return Err(self.err("expected IRI in prefix declaration")),
                },
                S::BaseIri { sparql } => match t {
                    Token::Iri(x) => {
                        let text = self.lx.text_str(x);
                        if let Err(m) = self.ctx.set_base(text) {
                            return Err(self.err(m));
                        }
                        self.state = if sparql {
                            S::Statement
                        } else {
                            S::DirectiveDot
                        };
                        return Ok(());
                    }
                    _ => return Err(self.err("expected IRI in base declaration")),
                },
                S::DirectiveDot => match t {
                    Token::Dot => {
                        self.state = S::Statement;
                        return Ok(());
                    }
                    _ => return Err(self.err("expected '.' after directive")),
                },
                S::VersionString { sparql } => match t {
                    Token::String { long: false, .. } => {
                        // The version specifier is recorded nowhere yet: both
                        // supported versions share one grammar; parsing mode
                        // is controlled by Options::spec12.
                        self.state = if sparql {
                            S::Statement
                        } else {
                            S::DirectiveDot
                        };
                        return Ok(());
                    }
                    _ => return Err(self.err("expected version string")),
                },
                S::Verb | S::VerbOrDot | S::AfterSemi => match t {
                    Token::KwA => {
                        self.predicate = Some(self.well_known(vocab::RDF_TYPE));
                        self.state = S::Object;
                        return Ok(());
                    }
                    Token::Iri(_) | Token::Pname { .. } => {
                        let h = self.simple_node(&t)?.expect("node token");
                        self.predicate = Some(h);
                        self.state = S::Object;
                        return Ok(());
                    }
                    Token::Semicolon if self.state == S::AfterSemi => return Ok(()),
                    Token::Dot if matches!(self.state, S::VerbOrDot | S::AfterSemi) => {
                        return self.end_statement()
                    }
                    Token::RBracket if self.state == S::AfterSemi => {
                        return self.close_bnode();
                    }
                    Token::AnnoClose if self.state == S::AfterSemi => {
                        return self.close_anno();
                    }
                    Token::RBrace
                        if self.trig && matches!(self.state, S::VerbOrDot | S::AfterSemi) =>
                    {
                        self.end_statement()?;
                        return self.close_graph();
                    }
                    _ => return Err(self.err("expected verb")),
                },
                S::Object => match t {
                    Token::String {
                        content,
                        long: _,
                        single: _,
                    } => {
                        self.pending.clear();
                        self.pending.extend_from_slice(self.lx.text(content));
                        self.lit_ret = Ret::Object;
                        self.state = S::AfterString;
                        return Ok(());
                    }
                    Token::Integer(x) => {
                        let h = emit_shorthand(&mut self.stmt, self.lx.text(x), vocab::XSD_INTEGER);
                        self.object_done(h, Some(Shorthand::Integer));
                        return Ok(());
                    }
                    Token::Decimal(x) => {
                        let h = emit_shorthand(&mut self.stmt, self.lx.text(x), vocab::XSD_DECIMAL);
                        self.object_done(h, Some(Shorthand::Decimal));
                        return Ok(());
                    }
                    Token::Double(x) => {
                        let h = emit_shorthand(&mut self.stmt, self.lx.text(x), vocab::XSD_DOUBLE);
                        self.object_done(h, Some(Shorthand::Double));
                        return Ok(());
                    }
                    Token::KwTrue | Token::KwFalse => {
                        let lex: &[u8] = if matches!(t, Token::KwTrue) {
                            b"true"
                        } else {
                            b"false"
                        };
                        let h = emit_shorthand(&mut self.stmt, lex, vocab::XSD_BOOLEAN);
                        self.object_done(h, Some(Shorthand::Boolean));
                        return Ok(());
                    }
                    Token::LBracket => {
                        self.bracket_ret = Ret::Object;
                        self.state = S::Bracket;
                        return Ok(());
                    }
                    Token::LParen => {
                        self.stack.push(Frame::Collection {
                            head: None,
                            tail: None,
                            ret: Ret::Object,
                        });
                        return Ok(()); // stay in Object for the first item
                    }
                    Token::RParen => return self.close_collection(),
                    Token::LtLt => {
                        self.require12("reified triples")?;
                        return self.push_rt(Ret::Object);
                    }
                    Token::LtLtParen => {
                        self.require12("triple terms")?;
                        return self.push_tt(Ret::Object);
                    }
                    _ => match self.simple_node(&t)? {
                        Some(h) => {
                            self.object_done(h, None);
                            return Ok(());
                        }
                        None => return Err(self.err("expected object term")),
                    },
                },
                S::AfterString => match t {
                    Token::LangTag { tag, dir } => {
                        if dir.is_some() {
                            self.require12("directional language tags")?;
                        }
                        let tag = self.lx.text_str(tag);
                        let h = emit_lang(&mut self.stmt, &self.pending, tag, dir);
                        let ret = self.lit_ret;
                        self.deliver(ret, h, None);
                        return Ok(());
                    }
                    // `@base`, `@prefix`, and `@version` are directives only
                    // at the start of a Turtle statement.  After a string the
                    // same byte sequences are perfectly valid BCP47-shaped
                    // language tags; the lexer cannot decide that context.
                    Token::KwBaseAt | Token::KwPrefixAt | Token::KwVersionAt => {
                        let tag = match t {
                            Token::KwBaseAt => "base",
                            Token::KwPrefixAt => "prefix",
                            Token::KwVersionAt => "version",
                            _ => unreachable!(),
                        };
                        let h = emit_lang(&mut self.stmt, &self.pending, tag, None);
                        let ret = self.lit_ret;
                        self.deliver(ret, h, None);
                        return Ok(());
                    }
                    Token::DoubleCaret => {
                        self.state = S::Datatype;
                        return Ok(());
                    }
                    _ => {
                        let h = emit_simple(&mut self.stmt, &self.pending);
                        let ret = self.lit_ret;
                        self.deliver(ret, h, None);
                        tok = Some(t);
                        continue;
                    }
                },
                S::Datatype => {
                    let dt: String = match t {
                        Token::Iri(x) => {
                            let text = self.lx.text_str(x);
                            match self.ctx.resolve_iri(text) {
                                Ok(abs) => abs.into_owned(),
                                Err(m) => return Err(self.err(m)),
                            }
                        }
                        Token::Pname { prefix, local } => self.pname_iri(prefix, local)?,
                        _ => return Err(self.err("expected datatype IRI after '^^'")),
                    };
                    let h =
                        emit_typed(&mut self.stmt, &self.pending, &dt).map_err(|m| self.err(m))?;
                    let ret = self.lit_ret;
                    self.deliver(ret, h, None);
                    return Ok(());
                }
                S::AfterObject => match t {
                    Token::Comma => {
                        self.state = S::Object;
                        return Ok(());
                    }
                    Token::Semicolon => {
                        self.state = S::AfterSemi;
                        return Ok(());
                    }
                    Token::Dot => return self.end_statement(),
                    Token::RBracket => return self.close_bnode(),
                    Token::AnnoClose => return self.close_anno(),
                    Token::RBrace if self.trig => {
                        self.end_statement()?;
                        return self.close_graph();
                    }
                    Token::Tilde => {
                        self.require12("reifiers")?;
                        self.state = S::AnnoReifier;
                        return Ok(());
                    }
                    Token::AnnoOpen => {
                        self.require12("annotation blocks")?;
                        return self.open_anno();
                    }
                    _ => return Err(self.err("expected ',', ';', '.', or annotation")),
                },
                S::AnnoReifier => match self.simple_node(&t)? {
                    Some(h) => {
                        self.deliver(Ret::AnnoReifier, h, None);
                        return Ok(());
                    }
                    None => match t {
                        Token::LBracket => {
                            self.bracket_ret = Ret::AnnoReifier;
                            self.state = S::Bracket;
                            return Ok(());
                        }
                        _ => {
                            // Bare '~': fresh reifier, then re-dispatch.
                            let r = self.fresh_bnode();
                            self.deliver(Ret::AnnoReifier, r, None);
                            tok = Some(t);
                            continue;
                        }
                    },
                },
                S::Bracket => match t {
                    Token::RBracket => {
                        let b = self.fresh_bnode();
                        let ret = self.bracket_ret;
                        self.deliver(ret, b, None);
                        return Ok(());
                    }
                    _ if matches!(
                        self.bracket_ret,
                        Ret::Subject | Ret::Object | Ret::StatementNode
                    ) =>
                    {
                        let b = self.fresh_bnode();
                        let ret = if self.bracket_ret == Ret::Object {
                            Ret::Object
                        } else {
                            // Bnode property lists cannot be graph labels and
                            // their predicateObjectList is optional.
                            Ret::SubjectOptionalPol
                        };
                        self.stack.push(Frame::BNode {
                            b,
                            saved_subject: self.subject,
                            saved_predicate: self.predicate,
                            ret,
                        });
                        self.subject = Some(b);
                        self.predicate = None;
                        self.state = S::Verb;
                        tok = Some(t);
                        continue;
                    }
                    _ => return Err(self.err("expected ']'")),
                },
                S::RtSubject => match self.simple_node(&t)? {
                    Some(h) => {
                        self.deliver(Ret::RtSubject, h, None);
                        return Ok(());
                    }
                    None => match t {
                        Token::LBracket => {
                            self.bracket_ret = Ret::RtSubject;
                            self.state = S::Bracket;
                            return Ok(());
                        }
                        Token::LtLt => return self.push_rt(Ret::RtSubject),
                        _ => return Err(self.err("expected reified-triple subject")),
                    },
                },
                S::RtPredicate => match t {
                    Token::KwA => {
                        let h = self.well_known(vocab::RDF_TYPE);
                        self.set_rt_predicate(h);
                        return Ok(());
                    }
                    Token::Iri(_) | Token::Pname { .. } => {
                        let h = self.simple_node(&t)?.expect("node token");
                        self.set_rt_predicate(h);
                        return Ok(());
                    }
                    _ => return Err(self.err("expected reified-triple predicate")),
                },
                S::RtObject => match t {
                    Token::String { content, .. } => {
                        self.pending.clear();
                        self.pending.extend_from_slice(self.lx.text(content));
                        self.lit_ret = Ret::RtObject;
                        self.state = S::AfterString;
                        return Ok(());
                    }
                    Token::Integer(_)
                    | Token::Decimal(_)
                    | Token::Double(_)
                    | Token::KwTrue
                    | Token::KwFalse => {
                        let h = self.shorthand_token(&t);
                        self.deliver(Ret::RtObject, h, None);
                        return Ok(());
                    }
                    Token::LBracket => {
                        self.bracket_ret = Ret::RtObject;
                        self.state = S::Bracket;
                        return Ok(());
                    }
                    Token::LtLt => return self.push_rt(Ret::RtObject),
                    Token::LtLtParen => return self.push_tt(Ret::RtObject),
                    _ => match self.simple_node(&t)? {
                        Some(h) => {
                            self.deliver(Ret::RtObject, h, None);
                            return Ok(());
                        }
                        None => return Err(self.err("expected reified-triple object")),
                    },
                },
                S::RtAfterObject => match t {
                    Token::Tilde => {
                        self.state = S::RtReifierNode;
                        return Ok(());
                    }
                    Token::GtGt => return self.close_rt(),
                    _ => return Err(self.err("expected '~' or '>>'")),
                },
                S::RtReifierNode => match self.simple_node(&t)? {
                    Some(h) => {
                        self.deliver(Ret::RtReifier, h, None);
                        return Ok(());
                    }
                    None => match t {
                        Token::LBracket => {
                            self.bracket_ret = Ret::RtReifier;
                            self.state = S::Bracket;
                            return Ok(());
                        }
                        // Bare '~' before '>>': fresh reifier at close.
                        Token::GtGt => return self.close_rt(),
                        _ => return Err(self.err("expected reifier or '>>'")),
                    },
                },
                S::RtClose => match t {
                    Token::GtGt => return self.close_rt(),
                    _ => return Err(self.err("expected '>>'")),
                },
                S::TtSubject => match self.simple_node(&t)? {
                    Some(h) => {
                        self.deliver(Ret::TtSubject, h, None);
                        return Ok(());
                    }
                    None => match t {
                        Token::LBracket => {
                            self.bracket_ret = Ret::TtSubject;
                            self.state = S::Bracket;
                            return Ok(());
                        }
                        _ => return Err(self.err("expected triple-term subject")),
                    },
                },
                S::TtPredicate => match t {
                    Token::KwA => {
                        let h = self.well_known(vocab::RDF_TYPE);
                        self.set_tt_predicate(h);
                        return Ok(());
                    }
                    Token::Iri(_) | Token::Pname { .. } => {
                        let h = self.simple_node(&t)?.expect("node token");
                        self.set_tt_predicate(h);
                        return Ok(());
                    }
                    _ => return Err(self.err("expected triple-term predicate")),
                },
                S::TtObject => match t {
                    Token::String { content, .. } => {
                        self.pending.clear();
                        self.pending.extend_from_slice(self.lx.text(content));
                        self.lit_ret = Ret::TtObject;
                        self.state = S::AfterString;
                        return Ok(());
                    }
                    Token::Integer(_)
                    | Token::Decimal(_)
                    | Token::Double(_)
                    | Token::KwTrue
                    | Token::KwFalse => {
                        let h = self.shorthand_token(&t);
                        self.deliver(Ret::TtObject, h, None);
                        return Ok(());
                    }
                    Token::LBracket => {
                        self.bracket_ret = Ret::TtObject;
                        self.state = S::Bracket;
                        return Ok(());
                    }
                    Token::LtLtParen => return self.push_tt(Ret::TtObject),
                    _ => match self.simple_node(&t)? {
                        Some(h) => {
                            self.deliver(Ret::TtObject, h, None);
                            return Ok(());
                        }
                        None => return Err(self.err("expected triple-term object")),
                    },
                },
                S::TtClose => match t {
                    Token::RParenGtGt => return self.close_tt(),
                    _ => return Err(self.err("expected ')>>'")),
                },
                S::Recover => unreachable!("recovery handled in pump"),
            }
        }
    }

    // -------------------------------------------------- composite handling

    fn shorthand_token(&mut self, t: &Token) -> H {
        match *t {
            Token::Integer(x) => {
                emit_shorthand(&mut self.stmt, self.lx.text(x), vocab::XSD_INTEGER)
            }
            Token::Decimal(x) => {
                emit_shorthand(&mut self.stmt, self.lx.text(x), vocab::XSD_DECIMAL)
            }
            Token::Double(x) => emit_shorthand(&mut self.stmt, self.lx.text(x), vocab::XSD_DOUBLE),
            Token::KwTrue => emit_shorthand(&mut self.stmt, b"true", vocab::XSD_BOOLEAN),
            Token::KwFalse => emit_shorthand(&mut self.stmt, b"false", vocab::XSD_BOOLEAN),
            _ => unreachable!("only shorthand tokens"),
        }
    }

    fn set_rt_predicate(&mut self, h: H) {
        match self.stack.last_mut() {
            Some(Frame::Rt { p, .. }) => *p = Some(h),
            _ => unreachable!("Rt state without Rt frame"),
        }
        self.state = S::RtObject;
    }

    fn set_tt_predicate(&mut self, h: H) {
        match self.stack.last_mut() {
            Some(Frame::Tt { p, .. }) => *p = Some(h),
            _ => unreachable!("Tt state without Tt frame"),
        }
        self.state = S::TtObject;
    }

    fn push_rt(&mut self, ret: Ret) -> Result<(), ParseError> {
        self.nesting_check()?;
        self.stack.push(Frame::Rt {
            s: None,
            p: None,
            o: None,
            reifier: None,
            ret,
        });
        self.state = S::RtSubject;
        Ok(())
    }

    fn push_tt(&mut self, ret: Ret) -> Result<(), ParseError> {
        self.nesting_check()?;
        self.stack.push(Frame::Tt {
            s: None,
            p: None,
            o: None,
            ret,
        });
        self.state = S::TtSubject;
        Ok(())
    }

    fn nesting_check(&self) -> Result<(), ParseError> {
        if self.stack.len() >= 4 * MAX_TRIPLE_TERM_DEPTH {
            return Err(self.err("construct nested too deeply"));
        }
        Ok(())
    }

    /// Build a triple term, tracking nesting depth so emitted terms stay
    /// decodable (core enforces MAX_TRIPLE_TERM_DEPTH on decode).
    fn build_tt(&mut self, s: H, p: H, o: H) -> Result<H, ParseError> {
        let depth = 1 + self.tt_depth(s).max(self.tt_depth(o));
        if depth > MAX_TRIPLE_TERM_DEPTH as u8 {
            return Err(self.err("triple terms nested too deeply"));
        }
        let h = emit_triple_term(&mut self.stmt, s, p, o, &mut self.tt_scratch);
        self.tt_depths.push((h, depth));
        Ok(h)
    }

    fn tt_depth(&self, h: H) -> u8 {
        self.tt_depths
            .iter()
            .rev()
            .find(|(k, _)| *k == h)
            .map_or(0, |&(_, d)| d)
    }

    fn close_rt(&mut self) -> Result<(), ParseError> {
        let Some(Frame::Rt {
            s,
            p,
            o,
            reifier,
            ret,
        }) = self.stack.pop()
        else {
            unreachable!("Rt close without Rt frame")
        };
        let (s, p, o) = (
            s.expect("rt subject set"),
            p.expect("rt predicate set"),
            o.expect("rt object set"),
        );
        let tt = self.build_tt(s, p, o)?;
        let r = reifier.unwrap_or_else(|| self.fresh_bnode());
        let reifies = self.well_known(vocab::RDF_REIFIES);
        self.emit(r, reifies, tt, None);
        self.deliver(ret, r, None);
        Ok(())
    }

    fn close_tt(&mut self) -> Result<(), ParseError> {
        let Some(Frame::Tt { s, p, o, ret }) = self.stack.pop() else {
            unreachable!("Tt close without Tt frame")
        };
        let h = self.build_tt(
            s.expect("tt subject set"),
            p.expect("tt predicate set"),
            o.expect("tt object set"),
        )?;
        self.deliver(ret, h, None);
        Ok(())
    }

    fn close_collection(&mut self) -> Result<(), ParseError> {
        match self.stack.pop() {
            Some(Frame::Collection { head, tail, ret }) => {
                let value = match (head, tail) {
                    (Some(h), Some(t)) => {
                        let rest = self.well_known(vocab::RDF_REST);
                        let nil = self.well_known(vocab::RDF_NIL);
                        self.emit(t, rest, nil, None);
                        h
                    }
                    _ => self.well_known(vocab::RDF_NIL),
                };
                self.deliver(ret, value, None);
                Ok(())
            }
            other => {
                if let Some(f) = other {
                    self.stack.push(f);
                }
                Err(self.err("unexpected ')'"))
            }
        }
    }

    fn close_bnode(&mut self) -> Result<(), ParseError> {
        match self.stack.pop() {
            Some(Frame::BNode {
                b,
                saved_subject,
                saved_predicate,
                ret,
            }) => {
                self.subject = saved_subject;
                self.predicate = saved_predicate;
                self.deliver(ret, b, None);
                Ok(())
            }
            other => {
                if let Some(f) = other {
                    self.stack.push(f);
                }
                Err(self.err("unexpected ']'"))
            }
        }
    }

    /// `{|`: reuse the current reifier (already reified) or mint and reify a
    /// fresh one, then parse the block with the reifier as subject.
    fn open_anno(&mut self) -> Result<(), ParseError> {
        let r = match self.cur_reifier.take() {
            Some(r) => r,
            None => {
                let r = self.fresh_bnode();
                self.reify(r)?;
                r
            }
        };
        self.stack.push(Frame::Anno {
            saved_subject: self.subject,
            saved_predicate: self.predicate,
            saved_last: self.last,
            saved_tt: self.cur_tt,
        });
        self.subject = Some(r);
        self.predicate = None;
        self.state = S::Verb;
        Ok(())
    }

    fn close_anno(&mut self) -> Result<(), ParseError> {
        match self.stack.pop() {
            Some(Frame::Anno {
                saved_subject,
                saved_predicate,
                saved_last,
                saved_tt,
            }) => {
                self.subject = saved_subject;
                self.predicate = saved_predicate;
                self.last = saved_last;
                self.cur_tt = saved_tt;
                self.cur_reifier = None;
                self.state = S::AfterObject;
                Ok(())
            }
            other => {
                if let Some(f) = other {
                    self.stack.push(f);
                }
                Err(self.err("unexpected '|}'"))
            }
        }
    }

    /// Emit `r rdf:reifies <<(s p o)>>` for the current annotation target.
    fn reify(&mut self, r: H) -> Result<(), ParseError> {
        let tt = self.annotation_tt()?;
        let reifies = self.well_known(vocab::RDF_REIFIES);
        self.emit(r, reifies, tt, None);
        Ok(())
    }

    fn end_statement(&mut self) -> Result<(), ParseError> {
        match self.stack.last() {
            None | Some(Frame::Graph) => {}
            Some(_) => return Err(self.err("statement terminator inside an unclosed construct")),
        }
        self.reset_statement();
        self.state = S::Statement;
        Ok(())
    }

    fn close_graph(&mut self) -> Result<(), ParseError> {
        match self.stack.last() {
            Some(Frame::Graph) => {
                self.stack.pop();
                self.graph = None;
                self.state = S::Statement;
                Ok(())
            }
            _ => Err(self.err("unexpected '}'")),
        }
    }

    fn object_done(&mut self, h: H, shorthand: Option<Shorthand>) {
        if let Some(Frame::Collection { head, tail, .. }) = self.stack.last_mut() {
            let (head_v, tail_v) = (*head, *tail);
            let n = self.fresh_bnode();
            let first = self.well_known(vocab::RDF_FIRST);
            if let Some(t) = tail_v {
                let rest = self.well_known(vocab::RDF_REST);
                self.emit(t, rest, n, None);
            }
            self.emit(n, first, h, shorthand);
            match self.stack.last_mut() {
                Some(Frame::Collection { head, tail, .. }) => {
                    if head_v.is_none() {
                        *head = Some(n);
                    }
                    *tail = Some(n);
                }
                _ => unreachable!(),
            }
            self.state = S::Object;
        } else {
            let s = self.subject.expect("subject set in object position");
            let p = self.predicate.expect("predicate set in object position");
            self.emit(s, p, h, shorthand);
            self.last = Some((s, p, h));
            self.cur_tt = None;
            self.cur_reifier = None;
            self.state = S::AfterObject;
        }
    }
}

/// Split-borrow view into the top Rt frame.
struct RtView<'a> {
    s: &'a mut Option<H>,
    o: &'a mut Option<H>,
    reifier: &'a mut Option<H>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use graphy_core::TermRef;

    fn show_term(t: TermRef<'_>) -> String {
        match t {
            TermRef::Iri(i) => format!("<{i}>"),
            TermRef::BlankNode(b) => format!("_:{b}"),
            TermRef::Literal(l) => {
                let mut s = format!("{:?}", l.lexical());
                match l.lang() {
                    Some((tag, dir)) => {
                        s.push('@');
                        s.push_str(tag);
                        if let Some(d) = dir {
                            s.push_str(if d == crate::Dir::Ltr {
                                "--ltr"
                            } else {
                                "--rtl"
                            });
                        }
                    }
                    None => {
                        let dt = l.datatype();
                        if dt != vocab::XSD_STRING {
                            let short = dt
                                .strip_prefix("http://www.w3.org/2001/XMLSchema#")
                                .map(|x| format!("xsd:{x}"))
                                .unwrap_or_else(|| format!("<{dt}>"));
                            s.push_str("^^");
                            s.push_str(&short);
                        }
                    }
                }
                s
            }
            TermRef::TripleTerm(tt) => format!(
                "<<( {} {} {} )>>",
                show_term(tt.subject()),
                show_term(tt.predicate()),
                show_term(tt.object())
            ),
        }
    }

    fn show(q: QuadRef<'_>) -> String {
        let mut s = format!(
            "{} {} {}",
            show_term(q.subject()),
            show_term(q.predicate()),
            show_term(q.object())
        );
        if let Some(g) = q.graph() {
            s.push_str(" G=");
            s.push_str(&show_term(g));
        }
        s
    }

    fn parse_with(
        input: &str,
        opts: Options,
        trig: bool,
    ) -> Result<Vec<(String, Option<Shorthand>)>, ParseError> {
        let mut out = Vec::new();
        if trig {
            let mut p = TriGParser::new(opts)?;
            p.feed(input.as_bytes())?;
            out.extend(p.drain().map(|q| (show(q), q.shorthand)));
            p.finish()?;
            out.extend(p.drain().map(|q| (show(q), q.shorthand)));
        } else {
            let mut p = TurtleParser::new(opts)?;
            p.feed(input.as_bytes())?;
            out.extend(p.drain().map(|q| (show(q), q.shorthand)));
            p.finish()?;
            out.extend(p.drain().map(|q| (show(q), q.shorthand)));
        }
        Ok(out)
    }

    fn parse(input: &str) -> Result<Vec<String>, ParseError> {
        parse_with(input, Options::default(), false)
            .map(|v| v.into_iter().map(|(q, _)| q).collect())
    }

    fn parse_trig(input: &str) -> Result<Vec<String>, ParseError> {
        parse_with(input, Options::default(), true).map(|v| v.into_iter().map(|(q, _)| q).collect())
    }

    const EX: &str = "@prefix ex: <http://x/> .\n";
    const RDF_NS: &str = "http://www.w3.org/1999/02/22-rdf-syntax-ns#";

    #[test]
    fn basic_predicate_object_lists() {
        let got = parse(&format!(
            "{EX}ex:s a ex:C ; ex:p \"v\" , 'w' , \"\"\"multi\nline\"\"\" ; .\n"
        ))
        .unwrap();
        assert_eq!(
            got,
            [
                format!("<http://x/s> <{RDF_NS}type> <http://x/C>"),
                "<http://x/s> <http://x/p> \"v\"".to_owned(),
                "<http://x/s> <http://x/p> \"w\"".to_owned(),
                "<http://x/s> <http://x/p> \"multi\\nline\"".to_owned(),
            ]
        );
    }

    #[test]
    fn directives_all_styles_and_base_resolution() {
        let got = parse(concat!(
            "@base <http://ex.example/dir/> .\n",
            "@prefix a: <sub/> .\n", // relative prefix IRI resolves
            "PREFIX b: <http://y/>\n",
            "BaSe <http://z.example/root/>\n",
            "<a> a:p <../up> .\n",
            "b:q b:r \"x\"^^b:dt .\n",
        ))
        .unwrap();
        assert_eq!(
            got,
            [
                "<http://z.example/root/a> <http://ex.example/dir/sub/p> <http://z.example/up>"
                    .to_owned(),
                "<http://y/q> <http://y/r> \"x\"^^<http://y/dt>".to_owned(),
            ]
        );
    }

    #[test]
    fn shorthand_literals_carry_hints() {
        let got = parse_with(
            &format!("{EX}ex:s ex:p 42, 4.2, 4.2e1, true, false .\n"),
            Options::default(),
            false,
        )
        .unwrap();
        let (quads, hints): (Vec<_>, Vec<_>) = got.into_iter().unzip();
        assert_eq!(
            quads,
            [
                "<http://x/s> <http://x/p> \"42\"^^xsd:integer",
                "<http://x/s> <http://x/p> \"4.2\"^^xsd:decimal",
                "<http://x/s> <http://x/p> \"4.2e1\"^^xsd:double",
                "<http://x/s> <http://x/p> \"true\"^^xsd:boolean",
                "<http://x/s> <http://x/p> \"false\"^^xsd:boolean",
            ]
        );
        assert_eq!(
            hints,
            [
                Some(Shorthand::Integer),
                Some(Shorthand::Decimal),
                Some(Shorthand::Double),
                Some(Shorthand::Boolean),
                Some(Shorthand::Boolean),
            ]
        );
    }

    #[test]
    fn collections() {
        let got = parse(&format!("{EX}ex:s ex:p (1 2) , () .\n")).unwrap();
        assert_eq!(
            got,
            [
                format!("_:b0 <{RDF_NS}first> \"1\"^^xsd:integer"),
                format!("_:b0 <{RDF_NS}rest> _:b1"),
                format!("_:b1 <{RDF_NS}first> \"2\"^^xsd:integer"),
                format!("_:b1 <{RDF_NS}rest> <{RDF_NS}nil>"),
                "<http://x/s> <http://x/p> _:b0".to_owned(),
                format!("<http://x/s> <http://x/p> <{RDF_NS}nil>"),
            ]
        );
        // Nested collection as subject.
        let got = parse(&format!("{EX}((\"i\")) ex:p ex:o .\n")).unwrap();
        assert_eq!(
            got,
            [
                format!("_:b0 <{RDF_NS}first> \"i\""),
                format!("_:b0 <{RDF_NS}rest> <{RDF_NS}nil>"),
                format!("_:b1 <{RDF_NS}first> _:b0"),
                format!("_:b1 <{RDF_NS}rest> <{RDF_NS}nil>"),
                "_:b1 <http://x/p> <http://x/o>".to_owned(),
            ]
        );
    }

    #[test]
    fn bnode_property_lists_and_anon() {
        let got = parse(&format!(
            "{EX}[ ex:q \"x\" ] ex:p [ ex:r [] ] .\n[ ex:only \"y\" ] .\nex:s ex:p [] .\n"
        ))
        .unwrap();
        assert_eq!(
            got,
            [
                "_:b0 <http://x/q> \"x\"".to_owned(),
                "_:b1 <http://x/r> _:b2".to_owned(),
                "_:b0 <http://x/p> _:b1".to_owned(),
                "_:b3 <http://x/only> \"y\"".to_owned(),
                "<http://x/s> <http://x/p> _:b4".to_owned(),
            ]
        );
    }

    #[test]
    fn reified_triples_desugar() {
        // Subject position with explicit reifier; inner triple NOT asserted.
        let got = parse(&format!("{EX}<< ex:s ex:p \"o\" ~ ex:r >> ex:q \"v\" .\n")).unwrap();
        assert_eq!(
            got,
            [
                format!("<http://x/r> <{RDF_NS}reifies> <<( <http://x/s> <http://x/p> \"o\" )>>"),
                "<http://x/r> <http://x/q> \"v\"".to_owned(),
            ]
        );
        // Object position, no reifier → fresh blank node.
        let got = parse(&format!("{EX}ex:s ex:p << ex:a ex:b 5 >> .\n")).unwrap();
        assert_eq!(
            got,
            [
                format!(
                    "_:b0 <{RDF_NS}reifies> <<( <http://x/a> <http://x/b> \"5\"^^xsd:integer )>>"
                ),
                "<http://x/s> <http://x/p> _:b0".to_owned(),
            ]
        );
        // Bare '~' also mints a fresh reifier.
        let got = parse(&format!("{EX}ex:s ex:p << ex:a ex:b ex:c ~ >> .\n")).unwrap();
        assert!(got[0].starts_with("_:b0 "));
    }

    #[test]
    fn annotations_desugar() {
        let got = parse(&format!(
            "{EX}ex:s ex:p ex:o ~ex:r {{| ex:a \"b\" |}} {{| ex:c \"d\" |}} .\n"
        ))
        .unwrap();
        assert_eq!(
            got,
            [
                "<http://x/s> <http://x/p> <http://x/o>".to_owned(),
                format!(
                    "<http://x/r> <{RDF_NS}reifies> <<( <http://x/s> <http://x/p> <http://x/o> )>>"
                ),
                "<http://x/r> <http://x/a> \"b\"".to_owned(),
                format!("_:b0 <{RDF_NS}reifies> <<( <http://x/s> <http://x/p> <http://x/o> )>>"),
                "_:b0 <http://x/c> \"d\"".to_owned(),
            ]
        );
        // Bare reifier annotation without block.
        let got = parse(&format!("{EX}ex:s ex:p ex:o ~ .\n")).unwrap();
        assert_eq!(got.len(), 2);
        assert!(got[1].starts_with("_:b0 "));
        // Nested annotation inside a block.
        let got = parse(&format!(
            "{EX}ex:s ex:p ex:o {{| ex:a ex:v {{| ex:i \"j\" |}} |}} .\n"
        ))
        .unwrap();
        assert_eq!(
            got,
            [
                "<http://x/s> <http://x/p> <http://x/o>".to_owned(),
                format!("_:b0 <{RDF_NS}reifies> <<( <http://x/s> <http://x/p> <http://x/o> )>>"),
                "_:b0 <http://x/a> <http://x/v>".to_owned(),
                format!("_:b1 <{RDF_NS}reifies> <<( _:b0 <http://x/a> <http://x/v> )>>"),
                "_:b1 <http://x/i> \"j\"".to_owned(),
            ]
        );
    }

    #[test]
    fn triple_terms_direct() {
        let got = parse(&format!(
            "{EX}ex:r ex:reifies <<( ex:s ex:p <<( ex:a ex:b \"c\"@en |)) )>> )>> .\n"
        ));
        // Deliberately malformed variant should error…
        assert!(got.is_err());
        let got = parse(&format!(
            "{EX}ex:r ex:reifies <<( ex:s ex:p <<( ex:a ex:b \"c\"@en )>> )>> .\n"
        ))
        .unwrap();
        assert_eq!(
            got,
            ["<http://x/r> <http://x/reifies> <<( <http://x/s> <http://x/p> <<( <http://x/a> <http://x/b> \"c\"@en )>> )>>"]
        );
    }

    #[test]
    fn trig_graph_forms() {
        let got = parse_trig(concat!(
            "@prefix ex: <http://x/> .\n",
            "ex:g1 { ex:s ex:p ex:o . ex:s2 ex:p2 \"x\" }\n",
            "GRAPH ex:g2 { ex:s ex:p ex:o }\n",
            "{ ex:d ex:p ex:o . }\n",
            "_:g { ex:s ex:p ex:o }\n",
            "ex:top ex:p ex:o .\n",
        ))
        .unwrap();
        assert_eq!(
            got,
            [
                "<http://x/s> <http://x/p> <http://x/o> G=<http://x/g1>".to_owned(),
                "<http://x/s2> <http://x/p2> \"x\" G=<http://x/g1>".to_owned(),
                "<http://x/s> <http://x/p> <http://x/o> G=<http://x/g2>".to_owned(),
                "<http://x/d> <http://x/p> <http://x/o>".to_owned(),
                "<http://x/s> <http://x/p> <http://x/o> G=_:s0".to_owned(),
                "<http://x/top> <http://x/p> <http://x/o>".to_owned(),
            ]
        );
    }

    #[test]
    fn syntax_rejections() {
        for bad in [
            "ex:s ex:p ex:o .",                                      // undeclared prefix
            "@prefix ex: <http://x/> . ex:s ex:p .",                 // missing object
            "@prefix ex: <http://x/> ex:s ex:p ex:o .",              // missing directive dot
            "@prefix ex: <http://x/> . a ex:p ex:o .",               // 'a' as subject
            "@prefix ex: <http://x/> . ex:s ex:p ex:o",              // missing final dot
            "@prefix ex: <http://x/> . ex:s ex:p (1 .",              // unclosed collection
            "@prefix ex: <http://x/> . ex:s ex:p [ ex:q \"v\" .",    // unclosed bracket
            "@prefix ex: <http://x/> . {| ex:p ex:o |} ex:q ex:v .", // anno as subject
            "@prefix ex: <http://x/> . ex:s ex:p <<( ex:a ex:b ex:c )>> ~ex:r .", // ~ after tt? fine actually — tt IS an object; hmm keep: annotation after tt object is legal! remove
        ] {
            if bad.contains("<<(") {
                continue; // annotation-after-tt is legal; skip placeholder
            }
            assert!(parse(bad).is_err(), "{bad}");
        }
        // TriG-only syntax rejected in Turtle mode.
        assert!(parse("@prefix ex: <http://x/> . ex:g { ex:s ex:p ex:o }").is_err());
        assert!(parse("{ }").is_err());
        // Nested graphs and directives inside graphs rejected in TriG.
        assert!(parse_trig("{ { } }").is_err());
        assert!(parse_trig("{ @prefix ex: <http://x/> . }").is_err());
        // 1.1 mode rejects 1.2 syntax.
        let opts = Options {
            spec12: false,
            ..Options::default()
        };
        for bad12 in [
            "@prefix ex: <http://x/> . ex:s ex:p << ex:a ex:b ex:c >> .",
            "@prefix ex: <http://x/> . ex:s ex:p ex:o ~ex:r .",
            "@prefix ex: <http://x/> . ex:s ex:p ex:o {| ex:a ex:b |} .",
            "@prefix ex: <http://x/> . ex:s ex:p <<( ex:a ex:b ex:c )>> .",
        ] {
            assert!(parse_with(bad12, opts.clone(), false).is_err(), "{bad12}");
        }
    }

    #[test]
    fn every_chunk_split_yields_identical_quads() {
        let input = concat!(
            "@prefix ex: <http://x/> .\n",
            "@base <http://base.example/> .\n",
            "ex:s a ex:C ; ex:p \"v\"@en--rtl , 4.2 , (1 [ ex:q <rel> ]) .\n",
            "<< ex:a ex:b 'c' >> ex:d ex:e {| ex:f 'g' |} .\n",
            "ex:s ex:p <<( ex:x ex:y \"\"\"z\nz\"\"\" )>> ~ [] .\n",
        );
        let whole = parse(input).unwrap();
        assert!(!whole.is_empty());
        let bytes = input.as_bytes();
        for at in 0..=bytes.len() {
            let mut p = TurtleParser::new(Options::default()).unwrap();
            let mut got = Vec::new();
            p.feed(&bytes[..at])
                .unwrap_or_else(|e| panic!("split {at}: {e}"));
            got.extend(p.drain().map(show));
            p.feed(&bytes[at..])
                .unwrap_or_else(|e| panic!("split {at}: {e}"));
            got.extend(p.drain().map(show));
            p.finish()
                .unwrap_or_else(|e| panic!("split {at} finish: {e}"));
            got.extend(p.drain().map(show));
            assert_eq!(got, whole, "split at {at}");
        }
    }

    #[test]
    fn lenient_resync_is_token_level_not_byte_level() {
        // The statements after the broken one contain IRIs with `.` in their
        // content; a byte hunt for `.` resumed *inside* them, cascading one
        // error into many and swallowing the valid statements (found via the
        // LSP on the W3C Turtle EARL report). Token-level resync: exactly one
        // error, both later statements survive.
        let opts = Options {
            lenient: true,
            ..Options::default()
        };
        let src = "@prefix ex: <http://x/> .\n\
                   ex:s ex:p BROKEN .\n\
                   ex:a ex:b <http://w.example.org/x.ttl> .\n\
                   ex:c ex:d ( <http://w3.org/a.ttl> <http://w3.org/b.ttl> ) .\n";
        // Whole-buffer and byte-at-a-time feeds must agree (the skip spans
        // chunk boundaries in the second case).
        for chunk in [src.len(), 1] {
            let mut p = TurtleParser::new(opts.clone()).unwrap();
            let mut subjects = Vec::new();
            for piece in src.as_bytes().chunks(chunk) {
                p.feed(piece).unwrap();
                subjects.extend(p.drain().map(|q| show_term(q.subject())));
            }
            p.finish().unwrap();
            subjects.extend(p.drain().map(|q| show_term(q.subject())));
            assert_eq!(p.errors().len(), 1, "chunk={chunk}: {:?}", p.errors());
            assert!(
                subjects.contains(&"<http://x/a>".to_string())
                    && subjects.contains(&"<http://x/c>".to_string()),
                "chunk={chunk}: statements after the error must survive: {subjects:?}"
            );
        }
    }

    #[test]
    fn lenient_recovers_at_statement_terminator() {
        let opts = Options {
            lenient: true,
            ..Options::default()
        };
        let got = parse_with(
            concat!(
                "@prefix ex: <http://x/> .\n",
                "ex:s ex:p BROKEN HERE .\n",
                "ex:s2 ex:p2 \"ok\" .\n",
            ),
            opts,
            false,
        )
        .unwrap();
        let quads: Vec<String> = got.into_iter().map(|(q, _)| q).collect();
        assert_eq!(quads, ["<http://x/s2> <http://x/p2> \"ok\""]);
    }
}
