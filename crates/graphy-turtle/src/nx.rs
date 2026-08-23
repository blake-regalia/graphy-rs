//! N-Triples / N-Quads driver (doc 03 §4): the simple line formats — no
//! prefixes, no collections, no shorthand. RDF 1.2 adds triple terms
//! `<<( s p o )>>` in the object position and directional language tags.
//!
//! Sans-io: `feed` parses eagerly into an internal quad buffer, `drain`
//! yields the quads parsed by the last feed, `finish` validates EOF.

use graphy_core::concise::MAX_TRIPLE_TERM_DEPTH;

use crate::common::{emit_lang, emit_simple, emit_triple_term, emit_typed, TermCtx};
use crate::lexer::{Lexer, Token};
use crate::quad::{Arena, Options, QuadBuf, QuadRef, H};
use crate::ParseError;

/// N-Triples parser (quads land in the default graph).
#[derive(Debug)]
pub struct NTriplesParser(NxParser);

/// N-Quads parser.
#[derive(Debug)]
pub struct NQuadsParser(NxParser);

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
        }
    };
}

delegate!(NTriplesParser);
delegate!(NQuadsParser);

impl NTriplesParser {
    pub fn new(options: Options) -> Result<NTriplesParser, ParseError> {
        Ok(NTriplesParser(NxParser::new(options, false)?))
    }

    /// Content-derived blank labels (data-parallel mode, doc 03 §4.1).
    pub(crate) fn set_content_labels(&mut self) {
        self.0.ctx.content_labels = true;
    }
}

impl NQuadsParser {
    pub fn new(options: Options) -> Result<NQuadsParser, ParseError> {
        Ok(NQuadsParser(NxParser::new(options, true)?))
    }

    /// Content-derived blank labels (data-parallel mode, doc 03 §4.1).
    pub(crate) fn set_content_labels(&mut self) {
        self.0.ctx.content_labels = true;
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum S {
    /// Statement start (the only legal EOF state).
    Subject,
    Predicate,
    Object,
    /// Literal parsed; awaiting optional `@lang` / `^^`.
    AfterString,
    /// After `^^`.
    Datatype,
    /// Object done: graph label (N-Quads) or `.`.
    AfterObject,
    /// Graph label done: `.`.
    AfterGraph,
    /// Inside `<<(`: subject, then predicate, then Object state (shared).
    TtSubject,
    TtPredicate,
    /// Triple-term object done: `)>>`.
    TtClose,
    /// Lenient mode: discarding bytes until the next newline.
    Recover,
}

#[derive(Debug)]
struct NxParser {
    lx: Lexer,
    quads: QuadBuf,
    /// Arena for the statement in flight (survives feed boundaries).
    stmt: Arena,
    ctx: TermCtx,
    spec12: bool,
    lenient: bool,
    quads_mode: bool,
    state: S,
    s: Option<H>,
    p: Option<H>,
    o: Option<H>,
    g: Option<H>,
    /// String content copied out of the lexer while awaiting lang/datatype.
    pending: Vec<u8>,
    /// (subject, predicate) frames of open `<<(`.
    tt_stack: Vec<(Option<H>, Option<H>)>,
    tt_scratch: Vec<u8>,
    errors: Vec<ParseError>,
}

impl NxParser {
    fn new(options: Options, quads_mode: bool) -> Result<NxParser, ParseError> {
        // N-Triples/N-Quads require absolute IRIs; `Options::base` is a
        // Turtle/TriG concern and is deliberately ignored here.
        let ctx =
            TermCtx::new(None, options.trusted, options.label_ns).expect("no base to validate");
        let mut lx = Lexer::new();
        lx.trusted = options.trusted;
        Ok(NxParser {
            lx,
            quads: QuadBuf::default(),
            stmt: Arena::default(),
            ctx,
            spec12: options.spec12,
            lenient: options.lenient,
            quads_mode,
            state: S::Subject,
            s: None,
            p: None,
            o: None,
            g: None,
            pending: Vec::new(),
            tt_stack: Vec::new(),
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
                if !self.lx.skip_past(b'\n') {
                    return Ok(()); // need more input (or EOF consumed all)
                }
                self.reset_statement();
                continue;
            }
            match self.lx.next() {
                Ok(None) => return Ok(()),
                Ok(Some(Token::Eof)) => {
                    if self.state != S::Subject {
                        let e = self.lx.err_here("unexpected end of input mid-statement");
                        self.fail(e)?;
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
        self.state = S::Subject;
        self.s = None;
        self.p = None;
        self.o = None;
        self.g = None;
        self.tt_stack.clear();
        self.stmt.clear();
    }

    fn err(&self, msg: impl Into<String>) -> ParseError {
        self.lx.err_at(self.lx.token_start(), msg)
    }

    /// An IRI or blank-node token in a node position, or `None`.
    fn node_term(&mut self, tok: &Token) -> Result<Option<H>, ParseError> {
        match *tok {
            Token::Iri(t) => {
                let text = self.lx.text_str(t);
                match self.ctx.emit_iri(&mut self.stmt, text) {
                    Ok(h) => Ok(Some(h)),
                    Err(m) => Err(self.err(m)),
                }
            }
            Token::BlankLabel(t) => {
                let surface = self.lx.text(t);
                Ok(Some(self.ctx.emit_blank(&mut self.stmt, surface)))
            }
            _ => Ok(None),
        }
    }

    fn step(&mut self, tok: Token) -> Result<(), ParseError> {
        // Loop so a state can finish pending work and re-dispatch the token.
        let mut tok = Some(tok);
        loop {
            let t = tok.take().expect("token consumed exactly once per return");
            match self.state {
                S::Subject => match self.node_term(&t)? {
                    Some(h) => {
                        self.s = Some(h);
                        self.state = S::Predicate;
                        return Ok(());
                    }
                    None => return Err(self.err("expected subject (IRI or blank node)")),
                },
                S::Predicate => match t {
                    Token::Iri(_) => {
                        let h = self.node_term(&t)?.expect("Iri is a node");
                        self.p = Some(h);
                        self.state = S::Object;
                        return Ok(());
                    }
                    _ => return Err(self.err("expected predicate (IRI)")),
                },
                S::Object => match t {
                    Token::String {
                        content,
                        long,
                        single,
                    } => {
                        if long || single {
                            return Err(
                                self.err("only double-quoted single-line strings are allowed")
                            );
                        }
                        self.pending.clear();
                        self.pending.extend_from_slice(self.lx.text(content));
                        self.state = S::AfterString;
                        return Ok(());
                    }
                    Token::LtLtParen => {
                        if !self.spec12 {
                            return Err(self.err("triple terms require RDF 1.2 mode"));
                        }
                        if self.tt_stack.len() >= MAX_TRIPLE_TERM_DEPTH {
                            return Err(self.err("triple terms nested too deeply"));
                        }
                        self.tt_stack.push((None, None));
                        self.state = S::TtSubject;
                        return Ok(());
                    }
                    _ => match self.node_term(&t)? {
                        Some(h) => {
                            self.complete_object(h);
                            return Ok(());
                        }
                        None => return Err(self.err("expected object term")),
                    },
                },
                S::AfterString => match t {
                    Token::LangTag { tag, dir } => {
                        if dir.is_some() && !self.spec12 {
                            return Err(self.err("directional language tags require RDF 1.2 mode"));
                        }
                        let tag = self.lx.text_str(tag);
                        let h = emit_lang(&mut self.stmt, &self.pending, tag, dir);
                        self.complete_object(h);
                        return Ok(());
                    }
                    // These spellings are directive tokens in Turtle, but in
                    // N-Triples literal context they are language tags.
                    Token::KwBaseAt | Token::KwPrefixAt | Token::KwVersionAt => {
                        let tag = match t {
                            Token::KwBaseAt => "base",
                            Token::KwPrefixAt => "prefix",
                            Token::KwVersionAt => "version",
                            _ => unreachable!(),
                        };
                        let h = emit_lang(&mut self.stmt, &self.pending, tag, None);
                        self.complete_object(h);
                        return Ok(());
                    }
                    Token::DoubleCaret => {
                        self.state = S::Datatype;
                        return Ok(());
                    }
                    _ => {
                        let h = emit_simple(&mut self.stmt, &self.pending);
                        self.complete_object(h);
                        tok = Some(t); // not consumed: re-dispatch
                        continue;
                    }
                },
                S::Datatype => match t {
                    Token::Iri(text) => {
                        let dt = self.lx.text_str(text);
                        let h = match self.ctx.resolve_iri(dt) {
                            Ok(abs) => emit_typed(&mut self.stmt, &self.pending, &abs),
                            Err(m) => Err(m),
                        }
                        .map_err(|m| self.err(m))?;
                        self.complete_object(h);
                        return Ok(());
                    }
                    _ => return Err(self.err("expected datatype IRI after '^^'")),
                },
                S::AfterObject => match t {
                    Token::Dot => {
                        self.emit_statement();
                        return Ok(());
                    }
                    _ if self.quads_mode => match self.node_term(&t)? {
                        Some(h) => {
                            self.g = Some(h);
                            self.state = S::AfterGraph;
                            return Ok(());
                        }
                        None => return Err(self.err("expected graph label or '.'")),
                    },
                    _ => return Err(self.err("expected '.'")),
                },
                S::AfterGraph => match t {
                    Token::Dot => {
                        self.emit_statement();
                        return Ok(());
                    }
                    _ => return Err(self.err("expected '.'")),
                },
                S::TtSubject => match self.node_term(&t)? {
                    Some(h) => {
                        self.tt_stack.last_mut().expect("in triple term").0 = Some(h);
                        self.state = S::TtPredicate;
                        return Ok(());
                    }
                    None => return Err(self.err("expected triple-term subject")),
                },
                S::TtPredicate => match t {
                    Token::Iri(_) => {
                        let h = self.node_term(&t)?.expect("Iri is a node");
                        self.tt_stack.last_mut().expect("in triple term").1 = Some(h);
                        self.state = S::Object;
                        return Ok(());
                    }
                    _ => return Err(self.err("expected triple-term predicate (IRI)")),
                },
                S::TtClose => match t {
                    Token::RParenGtGt => {
                        let (s, p) = self.tt_stack.pop().expect("in triple term");
                        let (s, p) = (s.expect("set"), p.expect("set"));
                        let o = self.o.take().expect("triple-term object set");
                        let h = emit_triple_term(&mut self.stmt, s, p, o, &mut self.tt_scratch);
                        self.complete_object(h);
                        return Ok(());
                    }
                    _ => return Err(self.err("expected ')>>'")),
                },
                S::Recover => unreachable!("recovery handled in pump"),
            }
        }
    }

    /// Route a finished object term: into the enclosing triple term, or as
    /// the statement object.
    fn complete_object(&mut self, h: H) {
        if self.tt_stack.is_empty() {
            self.o = Some(h);
            self.state = S::AfterObject;
        } else {
            self.o = Some(h);
            self.state = S::TtClose;
        }
    }

    fn emit_statement(&mut self) {
        let (s, p, o) = (
            self.s.take().expect("subject set"),
            self.p.take().expect("predicate set"),
            self.o.take().expect("object set"),
        );
        let g = self.g.take();
        self.quads
            .push_quad_from(&self.stmt, s, p, o, g.map(|h| self.stmt.get(h)), None);
        self.reset_statement();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Parse a whole doc, rendering each quad's concise bytes for comparison.
    fn parse_nq(input: &str) -> Result<Vec<String>, ParseError> {
        parse_nq_opts(input, Options::default())
    }

    fn parse_nq_opts(input: &str, opts: Options) -> Result<Vec<String>, ParseError> {
        let mut p = NQuadsParser::new(opts)?;
        p.feed(input.as_bytes())?;
        let mut out: Vec<String> = p.drain().map(render).collect();
        p.finish()?;
        out.extend(p.drain().map(render));
        Ok(out)
    }

    fn render(q: QuadRef<'_>) -> String {
        let t = |b: &[u8]| String::from_utf8_lossy(b).into_owned();
        match q.g {
            Some(g) => format!("{} | {} | {} | {}", t(q.s), t(q.p), t(q.o), t(g)),
            None => format!("{} | {} | {}", t(q.s), t(q.p), t(q.o)),
        }
    }

    #[test]
    fn simple_triples_and_quads() {
        let got = parse_nq(concat!(
            "<http://x/s> <http://x/p> <http://x/o> .\n",
            "_:a <http://x/p> \"lit\" .\n",
            "<http://x/s> <http://x/p> \"chat\"@en-US <http://x/g> .\n",
            "<http://x/s> <http://x/p> \"5\"^^<http://www.w3.org/2001/XMLSchema#integer> _:g .\n",
        ))
        .unwrap();
        assert_eq!(
            got,
            [
                ">http://x/s | >http://x/p | >http://x/o",
                "_s0 | >http://x/p | \"lit",
                // Language tags lowercase at construction.
                ">http://x/s | >http://x/p | @en-us\"chat | >http://x/g",
                ">http://x/s | >http://x/p | ^>http://www.w3.org/2001/XMLSchema#integer\"5 | _s1",
            ]
        );
    }

    #[test]
    fn escapes_and_string_datatype_folding() {
        let got = parse_nq(concat!(
            r#"<http://x/s> <http://x/p> "a\tbé" ."#,
            "\n",
            r#"<http://x/s> <http://x/p> "x"^^<http://www.w3.org/2001/XMLSchema#string> ."#,
            "\n",
        ))
        .unwrap();
        assert_eq!(got[0], ">http://x/s | >http://x/p | \"a\tbé");
        // xsd:string folds into the simple form (single spelling invariant).
        assert_eq!(got[1], ">http://x/s | >http://x/p | \"x");
    }

    #[test]
    fn blank_labels_relabel_deterministically() {
        let got = parse_nq("_:zz <http://x/p> _:aa .\n_:aa <http://x/p> _:zz .\n").unwrap();
        assert_eq!(got, ["_s0 | >http://x/p | _s1", "_s1 | >http://x/p | _s0",]);
    }

    #[test]
    fn triple_terms_nest() {
        let got = parse_nq(
            "_:r <http://x/reifies> <<( <http://x/s> <http://x/p> <<( _:i <http://x/q> \"v\" )>> )>> .\n",
        )
        .unwrap();
        assert_eq!(got.len(), 1);
        // Outer term decodes and exposes the nested one.
        let mut p = NQuadsParser::new(Options::default()).unwrap();
        p.feed(
            b"_:r <http://x/reifies> <<( <http://x/s> <http://x/p> <<( _:i <http://x/q> \"v\" )>> )>> .\n",
        )
        .unwrap();
        let q = p.drain().next().unwrap();
        match q.object() {
            graphy_core::TermRef::TripleTerm(tt) => {
                assert_eq!(tt.subject(), graphy_core::TermRef::Iri("http://x/s"));
                assert!(matches!(tt.object(), graphy_core::TermRef::TripleTerm(_)));
            }
            other => panic!("expected triple term, got {other:?}"),
        }
    }

    #[test]
    fn spec12_gate() {
        let opts = Options {
            spec12: false,
            ..Options::default()
        };
        let e = parse_nq_opts(
            "<http://x/s> <http://x/p> <<( <http://x/s> <http://x/p> \"v\" )>> .\n",
            opts.clone(),
        )
        .unwrap_err();
        assert!(e.message.contains("RDF 1.2"));
        let e = parse_nq_opts("<http://x/s> <http://x/p> \"v\"@ar--rtl .\n", opts).unwrap_err();
        assert!(e.message.contains("RDF 1.2"));
        // Both accepted by default.
        assert_eq!(
            parse_nq("<http://x/s> <http://x/p> \"v\"@ar--rtl .\n").unwrap()[0],
            ">http://x/s | >http://x/p | @ar--rtl\"v"
        );
    }

    #[test]
    fn syntax_rejections() {
        for bad in [
            "<http://x/s> <http://x/p> 'single' .",         // quote style
            "<http://x/s> <http://x/p> \"\"\"long\"\"\" .", // long string
            "<http://x/s> <http://x/p> 42 .",               // shorthand numeric
            "<http://x/s> ex:p <http://x/o> .",             // pname
            "<http://x/s> <http://x/p> <relative> .",       // relative, no base
            "<http://x/s> <http://x/p> .",                  // missing object
            "<http://x/s> <http://x/p> <http://x/o>",       // missing dot
            "<http://x/s> <http://x/p> \"x\"^^<http://www.w3.org/1999/02/22-rdf-syntax-ns#langString> .",
            "<http://x/s> _:b <http://x/o> .",              // blank predicate
            "<<( <http://x/s> <http://x/p> \"v\" )>> <http://x/p> <http://x/o> .", // tt subject
        ] {
            assert!(parse_nq(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn base_is_ignored_relative_iris_rejected() {
        // N-Triples/N-Quads require absolute IRIs; Options::base is a
        // Turtle/TriG concern and never applies here.
        let opts = Options {
            base: Some("http://ex.example/dir/doc".to_owned()),
            ..Options::default()
        };
        assert!(parse_nq_opts("<#f> <http://x/p> <http://x/o> .\n", opts).is_err());
    }

    #[test]
    fn lenient_mode_recovers_per_line() {
        let opts = Options {
            lenient: true,
            ..Options::default()
        };
        let mut p = NQuadsParser::new(opts).unwrap();
        p.feed(
            concat!(
                "<http://x/s> <http://x/p> <http://x/o> .\n",
                "<http://x/s> BROKEN GARBAGE\n",
                "<http://x/s2> <http://x/p2> \"ok\" .\n",
            )
            .as_bytes(),
        )
        .unwrap();
        let got: Vec<String> = p.drain().map(render).collect();
        p.finish().unwrap();
        assert_eq!(
            got,
            [
                ">http://x/s | >http://x/p | >http://x/o",
                ">http://x/s2 | >http://x/p2 | \"ok",
            ]
        );
        assert_eq!(p.errors().len(), 1);
    }

    #[test]
    fn every_chunk_split_yields_identical_quads() {
        let input = concat!(
            "<http://x/s> <http://x/p> \"a\\tb\"@en--ltr <http://x/g> .\n",
            "_:a <http://x/p> <<( _:a <http://x/q> \"4.0\"^^<http://www.w3.org/2001/XMLSchema#decimal> )>> .\n",
            "# comment\n",
            "<http://x/s> <http://x/p> \"\" .\n",
        );
        let whole = parse_nq(input).unwrap();
        let bytes = input.as_bytes();
        for at in 0..=bytes.len() {
            let mut p = NQuadsParser::new(Options::default()).unwrap();
            let mut got = Vec::new();
            p.feed(&bytes[..at])
                .unwrap_or_else(|e| panic!("split {at}: {e}"));
            got.extend(p.drain().map(render));
            p.feed(&bytes[at..])
                .unwrap_or_else(|e| panic!("split {at}: {e}"));
            got.extend(p.drain().map(render));
            p.finish().unwrap();
            got.extend(p.drain().map(render));
            assert_eq!(got, whole, "split at {at}");
        }
    }

    #[test]
    fn ntriples_rejects_graph_label() {
        let mut p = NTriplesParser::new(Options::default()).unwrap();
        let r = p.feed(b"<http://x/s> <http://x/p> <http://x/o> <http://x/g> .\n");
        assert!(r.is_err());
    }
}
