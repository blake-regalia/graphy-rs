//! Streaming serializers (doc 03 §2): canonical N-Quads/N-Triples (the
//! merge/debug workhorse) and pretty Turtle/TriG (grouped `;`/`,`, prefix
//! compaction). Writers consume [`QuadRef`]s — or owned quads via
//! [`NQuadsWriter::write_owned`] — in document order.

use std::io::{self, Write};

use graphy_core::{concise, vocab, Dir, GraphName, LiteralParts, Quad, TermRef};

use crate::quad::{QuadRef, Shorthand};

// ------------------------------------------------------- canonical N-Quads

/// Canonical N-Quads writer. With `g: None` on every quad this is exactly
/// canonical N-Triples.
#[derive(Debug)]
pub struct NQuadsWriter<W: Write> {
    out: W,
}

impl<W: Write> NQuadsWriter<W> {
    pub fn new(out: W) -> NQuadsWriter<W> {
        NQuadsWriter { out }
    }

    pub fn write_quad(&mut self, q: &QuadRef<'_>) -> io::Result<()> {
        write_term(&mut self.out, q.subject())?;
        self.out.write_all(b" ")?;
        write_term(&mut self.out, q.predicate())?;
        self.out.write_all(b" ")?;
        write_term(&mut self.out, q.object())?;
        if let Some(g) = q.graph() {
            self.out.write_all(b" ")?;
            write_term(&mut self.out, g)?;
        }
        self.out.write_all(b" .\n")
    }

    pub fn write_owned(&mut self, q: &Quad) -> io::Result<()> {
        self.write_quad(&QuadRef {
            s: q.s.as_concise(),
            p: q.p.as_concise(),
            o: q.o.as_concise(),
            g: match &q.g {
                GraphName::Default => None,
                GraphName::Named(t) => Some(t.as_concise()),
            },
            shorthand: None,
        })
    }

    pub fn into_inner(self) -> W {
        self.out
    }
}

/// Serialize one term in N-Triples syntax (canonical escapes).
pub fn write_term<W: Write>(out: &mut W, t: TermRef<'_>) -> io::Result<()> {
    match t {
        TermRef::Iri(i) => {
            out.write_all(b"<")?;
            out.write_all(i.as_bytes())?;
            out.write_all(b">")
        }
        TermRef::BlankNode(b) => {
            out.write_all(b"_:")?;
            out.write_all(b.as_bytes())
        }
        TermRef::Literal(l) => {
            write_string(out, l.lexical())?;
            if let Some((tag, dir)) = l.lang() {
                out.write_all(b"@")?;
                out.write_all(tag.as_bytes())?;
                if let Some(d) = dir {
                    out.write_all(match d {
                        Dir::Ltr => b"--ltr",
                        Dir::Rtl => b"--rtl",
                    })?;
                }
            } else {
                let dt = l.datatype();
                if dt != vocab::XSD_STRING {
                    out.write_all(b"^^<")?;
                    out.write_all(dt.as_bytes())?;
                    out.write_all(b">")?;
                }
            }
            Ok(())
        }
        TermRef::TripleTerm(tt) => {
            out.write_all(b"<<( ")?;
            write_term(out, tt.subject())?;
            out.write_all(b" ")?;
            write_term(out, tt.predicate())?;
            out.write_all(b" ")?;
            write_term(out, tt.object())?;
            out.write_all(b" )>>")
        }
    }
}

/// Canonical string escaping: `\" \\ \n \r`, everything else raw.
fn write_string<W: Write>(out: &mut W, lexical: &str) -> io::Result<()> {
    out.write_all(b"\"")?;
    let bytes = lexical.as_bytes();
    let mut from = 0;
    for (i, &b) in bytes.iter().enumerate() {
        let esc: &[u8] = match b {
            b'"' => b"\\\"",
            b'\\' => b"\\\\",
            b'\n' => b"\\n",
            b'\r' => b"\\r",
            _ => continue,
        };
        out.write_all(&bytes[from..i])?;
        out.write_all(esc)?;
        from = i + 1;
    }
    out.write_all(&bytes[from..])?;
    out.write_all(b"\"")
}

// ----------------------------------------------------------- pretty Turtle

/// One accumulated property of the stanza under construction.
#[derive(Debug)]
struct Prop {
    p: Vec<u8>,
    o: Vec<u8>,
    shorthand: Option<Shorthand>,
}

/// The stanza under construction: a consecutive same-subject run.
#[derive(Debug)]
struct Stanza {
    subject: Vec<u8>,
    props: Vec<Prop>,
}

/// A captured fresh-blank stanza (list spine or anonymous node), held until
/// its single reference splices it inline — as `( … )` when the chain is
/// spine-shaped, as `[ … ]` otherwise — or flushed as a `[] …` subject-anon
/// stanza at a section boundary if the reference never comes.
#[derive(Debug)]
struct Captured {
    /// Full concise blank bytes (`_b0`), doubling as the lookup key.
    label: Vec<u8>,
    props: Vec<Prop>,
}

/// A pending splice point in a held stanza: byte position, the awaited
/// concise blank bytes, and the indent depth at the reference site.
type Hole = (usize, Vec<u8>, usize);

/// Pretty Turtle/TriG writer: groups consecutive same-subject quads with
/// `;` / `,`, compacts IRIs against the declared prefixes, indents with TAB
/// (doc 09), and (TriG mode) wraps graph runs in `name { … }` blocks.
///
/// **Fresh blank nodes reconstruct as syntax** (docs/09 C6a): a stanza whose
/// subject carries a parser-*fresh* label (`b{n}`, minted only by `( … )` /
/// `[ … ]` syntax — such nodes are referenced at most once *by grammar*, so
/// inlining can never split a shared node; surface `_:label`s map to the
/// disjoint `s`-prefixed namespace — `s{n}` ordinals from the parsers,
/// `s{surface}` in NT/NQ content-label mode — and are never eligible) is
/// captured instead of written. When
/// its single reference arrives, a spine-shaped chain splices as a multiline
/// collection and anything else as an anonymous `[ … ]` block; unreferenced
/// captures flush as `[] …` subject-anon stanzas at section boundaries.
/// Locality after `tree` is bidirectional, and both directions are bounded:
/// captures ahead of their reference are consumed on sight, and a stanza
/// whose references haven't arrived yet defers with splice holes resolved by
/// the immediately-following captures. Streams are assumed to come from the
/// parsers (document order, fresh-label conventions); memory is the current
/// stanza plus live captures, with an overflow backstop that degrades to
/// labeled output, never wrong output. Streams that do NOT come from the
/// parsers (query results, store scans) can reference a fresh-shaped label
/// more than once, which would split the node once its capture is spliced —
/// such producers must set [`TurtleWriter::labeled_blanks`].
#[derive(Debug)]
pub struct TurtleWriter<W: Write> {
    out: W,
    trig: bool,
    /// (prefix, iri) in declaration order (header order).
    prefixes: Vec<(String, String)>,
    /// Indices into `prefixes`, longest IRIs first for greedy compaction.
    match_order: Vec<usize>,
    /// Which declarations compaction has used (drives `used_prefixes_only`).
    used: Vec<bool>,
    /// Render every blank node with its label: no `( … )`/`[ … ]`
    /// reconstruction. Required for non-parser streams.
    labeled_blanks: bool,
    /// Derive bare literal tokens (`5`, `true`) from the datatype for
    /// eligible typed literals, even without parser shorthand provenance.
    terse: bool,
    /// Buffer the body and emit only the used prefixes at `finish`.
    used_only: bool,
    /// The buffered body (`used_only` mode).
    body: Vec<u8>,
    header_written: bool,
    cur_graph: Option<Vec<u8>>,
    in_graph_block: bool,
    /// Whether the current graph section has flushed a stanza yet (drives
    /// the blank-line separators between stanzas).
    section_has_stanzas: bool,
    stanza: Option<Stanza>,
    captured: Vec<Captured>,
    /// A rendered stanza deferred on unresolved fresh-blank references.
    held: Option<(Vec<u8>, Vec<Hole>)>,
}

impl<W: Write> TurtleWriter<W> {
    pub fn new(out: W) -> TurtleWriter<W> {
        TurtleWriter {
            out,
            trig: false,
            prefixes: Vec::new(),
            match_order: Vec::new(),
            used: Vec::new(),
            labeled_blanks: false,
            terse: false,
            used_only: false,
            body: Vec::new(),
            header_written: false,
            cur_graph: None,
            in_graph_block: false,
            section_has_stanzas: false,
            stanza: None,
            captured: Vec::new(),
            held: None,
        }
    }

    /// Enable TriG output (graph blocks). Must be set before the first quad.
    pub fn trig(mut self) -> TurtleWriter<W> {
        self.trig = true;
        self
    }

    /// Declare a prefix (emitted as an `@prefix` header, used for
    /// compaction). Must be called before the first quad.
    pub fn prefix(mut self, name: &str, iri: &str) -> TurtleWriter<W> {
        self.prefixes.push((name.to_owned(), iri.to_owned()));
        self.used.push(false);
        self.match_order = (0..self.prefixes.len()).collect();
        self.match_order.sort_by(|&a, &b| {
            let (a, b) = (&self.prefixes[a].1, &self.prefixes[b].1);
            b.len().cmp(&a.len()).then_with(|| a.cmp(b))
        });
        self
    }

    /// Render every blank node with its `_:label` — no collection or
    /// anonymous-node reconstruction. Required for streams that don't come
    /// from the parsers (query results, store merges), where a fresh-shaped
    /// `b{n}` label may be referenced more than once.
    pub fn labeled_blanks(mut self) -> TurtleWriter<W> {
        self.labeled_blanks = true;
        self
    }

    /// Write eligible typed literals as their bare Turtle token even when the
    /// quad carries no parser [`Shorthand`]: `"5"^^xsd:integer` renders as
    /// `5`, `"true"^^xsd:boolean` as `true`. Eligibility requires the lexical
    /// to match the corresponding grammar production exactly, so literals
    /// with no bare spelling (`"TRUE"^^xsd:boolean`, `" 5"^^xsd:integer`,
    /// `"NaN"^^xsd:double`) stay quoted and every triple round-trips
    /// unchanged. Intended for streams without syntactic provenance (query
    /// results, store scans); parser-fed reformatting paths leave this off so
    /// an author's explicit `"5"^^xsd:integer` spelling survives.
    pub fn terse_literals(mut self) -> TurtleWriter<W> {
        self.terse = true;
        self
    }

    /// Emit only the `@prefix` declarations compaction actually used, in
    /// declaration order. Buffers the whole body until [`Self::finish`]
    /// (the header can't be known sooner), so streaming output stops —
    /// intended for response-sized documents.
    pub fn used_prefixes_only(mut self) -> TurtleWriter<W> {
        self.used_only = true;
        self
    }

    pub fn write_quad(&mut self, q: &QuadRef<'_>) -> io::Result<()> {
        if !self.header_written {
            self.header_written = true;
            if !self.used_only {
                for (name, iri) in &self.prefixes {
                    writeln!(self.out, "@prefix {name}: <{iri}> .")?;
                }
                if !self.prefixes.is_empty() {
                    writeln!(self.out)?;
                }
            }
        }
        if !self.trig && q.g.is_some() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "named graph quad in Turtle output (use .trig())",
            ));
        }
        // Graph transition (TriG). Fresh nodes never span graphs, so
        // captures flush (as subject-anons) before the section closes.
        if self.trig && self.cur_graph.as_deref() != q.g {
            self.finish_stanza()?;
            self.flush_captured(true)?;
            let closed_block = self.in_graph_block;
            if self.in_graph_block {
                self.emit(b"}\n")?;
                self.in_graph_block = false;
            }
            // Blank line between what came before and the next graph section.
            if self.section_has_stanzas || closed_block {
                self.emit(b"\n")?;
            }
            self.cur_graph = q.g.map(<[u8]>::to_vec);
            self.section_has_stanzas = false;
            if let Some(g) = q.graph() {
                let mut buf = Vec::new();
                self.render_term_to(&mut buf, g)?;
                self.emit(&buf)?;
                self.emit(b" ")?;
            }
            if self.cur_graph.is_some() {
                self.emit(b"{\n")?;
                self.in_graph_block = true;
            }
        }
        if self.stanza.as_ref().map(|s| s.subject.as_slice()) != Some(q.s) {
            self.finish_stanza()?;
            self.stanza = Some(Stanza {
                subject: q.s.to_vec(),
                props: Vec::new(),
            });
        }
        self.stanza.as_mut().expect("set above").props.push(Prop {
            p: q.p.to_vec(),
            o: q.o.to_vec(),
            shorthand: q.shorthand,
        });
        Ok(())
    }

    /// Close the writer: the open stanza, unreferenced captures, and an open
    /// graph block. In `used_prefixes_only` mode this is also where the
    /// header and buffered body reach the sink.
    pub fn finish(mut self) -> io::Result<W> {
        self.finish_stanza()?;
        self.flush_captured(true)?;
        if self.in_graph_block {
            self.emit(b"}\n")?;
        }
        if self.used_only {
            let mut any = false;
            for (ix, (name, iri)) in self.prefixes.iter().enumerate() {
                if self.used[ix] {
                    writeln!(self.out, "@prefix {name}: <{iri}> .")?;
                    any = true;
                }
            }
            if any {
                writeln!(self.out)?;
            }
            self.out.write_all(&self.body)?;
        }
        Ok(self.out)
    }

    /// Body byte sink: the buffer in `used_prefixes_only` mode, `out` otherwise.
    fn emit(&mut self, bytes: &[u8]) -> io::Result<()> {
        if self.used_only {
            self.body.extend_from_slice(bytes);
            Ok(())
        } else {
            self.out.write_all(bytes)
        }
    }

    /// Fresh-label eligibility, modulo the `labeled_blanks` override.
    fn is_fresh(&self, bytes: &[u8]) -> bool {
        !self.labeled_blanks && fresh_blank_label(bytes)
    }

    /// Route the completed stanza: capture fresh-label stanzas, render
    /// everything else (deferring on not-yet-arrived references).
    fn finish_stanza(&mut self) -> io::Result<()> {
        let Some(st) = self.stanza.take() else {
            return Ok(());
        };
        if self.is_fresh(&st.subject) {
            self.captured.push(Captured {
                label: st.subject,
                props: st.props,
            });
            // Backstop for adversarial non-local streams: flushing LABELED
            // keeps output correct (later references use the same labels).
            if self.captured.len() > 4096 {
                self.resolve_held()?;
                self.flush_captured(false)?;
            }
            return Ok(());
        }
        // A non-captured stanza ends any deferral window: whatever its
        // predecessor was waiting for has arrived by now (or never will).
        self.resolve_held()?;
        let mut buf = Vec::new();
        let mut holes = Vec::new();
        self.build_stanza(&mut buf, Some(&mut holes), None, &st.subject, &st.props)?;
        if holes.is_empty() {
            self.write_stanza(&buf)
        } else {
            self.held = Some((buf, holes));
            Ok(())
        }
    }

    /// Splice a held stanza's holes — spine chains as collections, other
    /// captures as `[ … ]`, zero-property fresh anons as `[]` — and write it.
    fn resolve_held(&mut self) -> io::Result<()> {
        let Some((buf, holes)) = self.held.take() else {
            return Ok(());
        };
        let mut out = Vec::with_capacity(buf.len());
        let mut at = 0usize;
        for (pos, label, depth) in holes {
            out.extend_from_slice(&buf[at..pos]);
            at = pos;
            self.render_fresh_ref(&mut out, &label, depth)?;
        }
        out.extend_from_slice(&buf[at..]);
        self.write_stanza(&out)
    }

    /// Flush remaining captures. At a true section boundary (`anon`) they
    /// are provably unreferenced and render as `[] …` subject-anon stanzas;
    /// on overflow they keep their labels (a later reference stays valid).
    fn flush_captured(&mut self, anon: bool) -> io::Result<()> {
        self.resolve_held()?;
        while !self.captured.is_empty() {
            let c = self.captured.remove(0);
            let mut buf = Vec::new();
            let subject_override: Option<&[u8]> = if anon { Some(b"[]") } else { None };
            self.build_stanza(&mut buf, None, subject_override, &c.label, &c.props)?;
            self.write_stanza(&buf)?;
        }
        Ok(())
    }

    /// Render a stanza into `buf`. With `holes`, a fresh-blank object whose
    /// capture hasn't arrived yet becomes a splice point instead of a label.
    fn build_stanza(
        &mut self,
        buf: &mut Vec<u8>,
        mut holes: Option<&mut Vec<Hole>>,
        subject_override: Option<&[u8]>,
        subject: &[u8],
        props: &[Prop],
    ) -> io::Result<()> {
        // Canonical indent is TAB (doc 09 — the original CLI's default).
        let base = usize::from(self.in_graph_block);
        push_tabs(buf, base);
        match subject_override {
            Some(text) => buf.extend_from_slice(text),
            None => self.render_concise(buf, subject)?,
        }
        buf.extend_from_slice(b" ");
        let mut cur_pred: Option<Vec<u8>> = None;
        let mut depth = base;
        for prop in props {
            match &cur_pred {
                Some(p) if p.as_slice() == prop.p.as_slice() => buf.extend_from_slice(b", "),
                Some(_) => {
                    buf.extend_from_slice(b" ;\n");
                    push_tabs(buf, base + 1);
                    depth = base + 1;
                    self.render_verb(buf, &prop.p)?;
                    buf.extend_from_slice(b" ");
                }
                None => {
                    self.render_verb(buf, &prop.p)?;
                    buf.extend_from_slice(b" ");
                }
            }
            cur_pred = Some(prop.p.clone());
            // `rdf:rest` keeps explicit chain structure (only reached when a
            // capture flushes un-inlined); everywhere else, sugar applies.
            let sugar = !is_concise_iri(&prop.p, vocab::RDF_REST);
            self.render_object(
                buf,
                holes.as_deref_mut(),
                &prop.o,
                prop.shorthand,
                sugar,
                depth,
            )?;
        }
        buf.extend_from_slice(b" .\n");
        Ok(())
    }

    /// Write a fully-resolved stanza, with the blank-line separator between
    /// subject stanzas (cli.examples parity).
    fn write_stanza(&mut self, buf: &[u8]) -> io::Result<()> {
        if self.section_has_stanzas {
            self.emit(b"\n")?;
        }
        self.emit(buf)?;
        self.section_has_stanzas = true;
        Ok(())
    }

    /// One object position at indent `depth`. With `sugar`, captured chains
    /// splice as collections / `[ … ]` blocks and bare `rdf:nil` renders as
    /// `()`; with `holes`, an uncaptured fresh blank defers as a splice
    /// point (its capture is expected right after the current stanza).
    fn render_object(
        &mut self,
        buf: &mut Vec<u8>,
        holes: Option<&mut Vec<Hole>>,
        o: &[u8],
        shorthand: Option<Shorthand>,
        sugar: bool,
        depth: usize,
    ) -> io::Result<()> {
        if sugar {
            if self.is_fresh(o) {
                if let Some(holes) = holes {
                    if self.chain_head(o).is_none() && !self.is_captured(o) {
                        holes.push((buf.len(), o.to_vec(), depth));
                        return Ok(());
                    }
                }
                return self.render_fresh_ref(buf, o, depth);
            }
            if is_concise_iri(o, vocab::RDF_NIL) {
                buf.extend_from_slice(b"()");
                return Ok(());
            }
        }
        // Shorthand literals go back out as their bare lexical: tagged by the
        // parsers (the source document used the bare token, so the lexical is
        // grammar-valid), or — in `terse_literals` mode — derived from the
        // datatype when the lexical is itself a valid bare token.
        if shorthand.is_some() || self.terse {
            if let Ok(TermRef::Literal(l)) = concise::decode(o) {
                if shorthand.is_some() || bare_form_valid(&l) {
                    buf.extend_from_slice(l.lexical().as_bytes());
                    return Ok(());
                }
            }
        }
        self.render_concise(buf, o)
    }

    /// A reference to a fresh blank: a spine chain inlines as a multiline
    /// collection, any other capture as `[ … ]`, and an uncaptured fresh
    /// node — a zero-property `[]` in the source — as `[]`.
    fn render_fresh_ref(
        &mut self,
        buf: &mut Vec<u8>,
        label: &[u8],
        depth: usize,
    ) -> io::Result<()> {
        if let Some(elems) = self.take_chain(label) {
            return self.render_list(buf, &elems, depth);
        }
        if let Some(props) = self.take_captured(label) {
            return self.render_bnode(buf, &props, depth);
        }
        buf.extend_from_slice(b"[]");
        Ok(())
    }

    /// `(\n … \n)`: one element per line, indented one extra tab; the
    /// closing paren returns to the construct's own indent.
    fn render_list(
        &mut self,
        buf: &mut Vec<u8>,
        elems: &[(Vec<u8>, Option<Shorthand>)],
        depth: usize,
    ) -> io::Result<()> {
        buf.extend_from_slice(b"(\n");
        for (eo, esh) in elems {
            push_tabs(buf, depth + 1);
            self.render_object(buf, None, eo, *esh, true, depth + 1)?;
            buf.extend_from_slice(b"\n");
        }
        push_tabs(buf, depth);
        buf.extend_from_slice(b")");
        Ok(())
    }

    /// `[\n … \n]`: property groups one per line at one extra tab.
    fn render_bnode(&mut self, buf: &mut Vec<u8>, props: &[Prop], depth: usize) -> io::Result<()> {
        buf.extend_from_slice(b"[\n");
        let mut cur_pred: Option<Vec<u8>> = None;
        for prop in props {
            match &cur_pred {
                Some(p) if p.as_slice() == prop.p.as_slice() => buf.extend_from_slice(b", "),
                Some(_) => {
                    buf.extend_from_slice(b" ;\n");
                    push_tabs(buf, depth + 1);
                    self.render_verb(buf, &prop.p)?;
                    buf.extend_from_slice(b" ");
                }
                None => {
                    push_tabs(buf, depth + 1);
                    self.render_verb(buf, &prop.p)?;
                    buf.extend_from_slice(b" ");
                }
            }
            cur_pred = Some(prop.p.clone());
            let sugar = !is_concise_iri(&prop.p, vocab::RDF_REST);
            self.render_object(buf, None, &prop.o, prop.shorthand, sugar, depth + 1)?;
        }
        buf.extend_from_slice(b"\n");
        push_tabs(buf, depth);
        buf.extend_from_slice(b"]");
        Ok(())
    }

    fn is_captured(&self, label: &[u8]) -> bool {
        self.captured.iter().any(|c| c.label == label)
    }

    /// The chain's element list when `head` starts a complete, acyclic,
    /// captured spine chain ending in `rdf:nil` — without consuming.
    fn chain_head(&self, head: &[u8]) -> Option<Vec<Vec<u8>>> {
        let mut order = Vec::new();
        let mut cur = head;
        loop {
            let node = self.captured.iter().find(|c| c.label == cur)?;
            let (_, _, rest) = spine_shape(&node.props)?;
            if order.iter().any(|l: &Vec<u8>| l == &node.label) {
                return None; // cycle
            }
            order.push(node.label.clone());
            if is_concise_iri(rest, vocab::RDF_NIL) {
                return Some(order);
            }
            if rest.first() != Some(&b'_') {
                return None;
            }
            cur = rest;
        }
    }

    /// Verify-then-consume a captured spine chain starting at `head`.
    fn take_chain(&mut self, head: &[u8]) -> Option<Vec<(Vec<u8>, Option<Shorthand>)>> {
        let order = self.chain_head(head)?;
        let mut elems = Vec::new();
        for label in order {
            let at = self
                .captured
                .iter()
                .position(|c| c.label == label)
                .expect("chain verified above");
            let node = self.captured.remove(at);
            let (first, sh, _) = spine_shape(&node.props).expect("chain verified above");
            elems.push((first.to_vec(), sh));
        }
        Some(elems)
    }

    /// Consume a captured non-chain node's properties.
    fn take_captured(&mut self, label: &[u8]) -> Option<Vec<Prop>> {
        let at = self.captured.iter().position(|c| c.label == label)?;
        Some(self.captured.remove(at).props)
    }

    fn render_verb(&mut self, buf: &mut Vec<u8>, p: &[u8]) -> io::Result<()> {
        if is_concise_iri(p, vocab::RDF_TYPE) {
            buf.extend_from_slice(b"a");
            Ok(())
        } else {
            self.render_concise(buf, p)
        }
    }

    fn render_concise(&mut self, buf: &mut Vec<u8>, bytes: &[u8]) -> io::Result<()> {
        match concise::decode(bytes) {
            Ok(t) => self.render_term_to(buf, t),
            Err(_) => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid concise term bytes",
            )),
        }
    }

    fn render_term_to(&mut self, buf: &mut Vec<u8>, t: TermRef<'_>) -> io::Result<()> {
        match t {
            TermRef::Iri(iri) => self.render_iri(buf, iri),
            TermRef::Literal(l) if l.lang().is_none() && l.datatype() != vocab::XSD_STRING => {
                write_string(buf, l.lexical())?;
                buf.extend_from_slice(b"^^");
                self.render_iri(buf, l.datatype())
            }
            other => write_term(buf, other),
        }
    }

    /// An IRI, compacted against the declared prefixes when the local name
    /// needs no escaping.
    fn render_iri(&mut self, buf: &mut Vec<u8>, iri: &str) -> io::Result<()> {
        for &ix in &self.match_order {
            let (name, expansion) = &self.prefixes[ix];
            if let Some(local) = iri.strip_prefix(expansion.as_str()) {
                if is_safe_local(local) {
                    self.used[ix] = true;
                    return write!(buf, "{name}:{local}");
                }
            }
        }
        write!(buf, "<{iri}>")
    }
}

/// The props of a pure list-spine node: exactly one `rdf:first` and one
/// `rdf:rest` (to a blank or `rdf:nil`).
fn spine_shape(props: &[Prop]) -> Option<(&[u8], Option<Shorthand>, &[u8])> {
    if props.len() != 2 {
        return None;
    }
    let mut first = None;
    let mut rest = None;
    for p in props {
        if first.is_none() && is_concise_iri(&p.p, vocab::RDF_FIRST) {
            first = Some(p);
        } else if rest.is_none() && is_concise_iri(&p.p, vocab::RDF_REST) {
            rest = Some(p);
        } else {
            return None;
        }
    }
    let (first, rest) = (first?, rest?);
    if rest.o.first() != Some(&b'_') && !is_concise_iri(&rest.o, vocab::RDF_NIL) {
        return None;
    }
    Some((&first.o, first.shorthand, &rest.o))
}

fn push_tabs(buf: &mut Vec<u8>, n: usize) {
    buf.extend(std::iter::repeat_n(b'\t', n));
}

/// Concise blank bytes with a parser-FRESH label — `_b12`, or `_f3b12` from
/// a namespaced multi-input load. Fresh nodes come only from `( … )`/`[ … ]`
/// syntax and are referenced at most once by grammar; surface `_:x` labels
/// land in the `s` namespace (`_s0` first-seen ordinals from the parsers,
/// `_sx` in NT/NQ content-label mode) and are never eligible, which also
/// makes reformatting our own explicit `_:bN`/`_:sN` output safe (it
/// re-parses as an `_s{n}` ordinal).
fn fresh_blank_label(o: &[u8]) -> bool {
    let Some(mut r) = o.strip_prefix(b"_") else {
        return false;
    };
    if let Some(after_f) = r.strip_prefix(b"f") {
        let digits = after_f.iter().take_while(|c| c.is_ascii_digit()).count();
        if digits == 0 {
            return false;
        }
        r = &after_f[digits..];
    }
    match r.strip_prefix(b"b") {
        Some(digits) => !digits.is_empty() && digits.iter().all(u8::is_ascii_digit),
        None => false,
    }
}

/// Whether a literal may render as its bare Turtle token: one of the four
/// shorthand datatypes AND a lexical matching the corresponding grammar
/// production *exactly*. The lexical check is what keeps `terse_literals`
/// round-trip safe — `"TRUE"^^xsd:boolean` and `" 5"^^xsd:integer` are valid
/// RDF literals with no bare spelling, and the XSD lexical spaces are wider
/// than the Turtle tokens (`"1."^^xsd:decimal`, `"NaN"^^xsd:double`).
fn bare_form_valid(l: &LiteralParts<'_>) -> bool {
    if l.lang().is_some() {
        return false;
    }
    let lex = l.lexical();
    match l.datatype() {
        vocab::XSD_BOOLEAN => lex == "true" || lex == "false",
        vocab::XSD_INTEGER => turtle_integer(lex),
        vocab::XSD_DECIMAL => turtle_decimal(lex),
        vocab::XSD_DOUBLE => turtle_double(lex),
        _ => false,
    }
}

/// INTEGER: `[+-]? [0-9]+`
fn turtle_integer(s: &str) -> bool {
    let d = s.strip_prefix(['+', '-']).unwrap_or(s);
    !d.is_empty() && d.bytes().all(|b| b.is_ascii_digit())
}

/// DECIMAL: `[+-]? [0-9]* '.' [0-9]+`
fn turtle_decimal(s: &str) -> bool {
    let d = s.strip_prefix(['+', '-']).unwrap_or(s);
    let Some((int, frac)) = d.split_once('.') else {
        return false;
    };
    !frac.is_empty()
        && int.bytes().all(|b| b.is_ascii_digit())
        && frac.bytes().all(|b| b.is_ascii_digit())
}

/// DOUBLE: `[+-]? ([0-9]+ '.' [0-9]* | '.' [0-9]+ | [0-9]+) [eE] [+-]? [0-9]+`
fn turtle_double(s: &str) -> bool {
    let d = s.strip_prefix(['+', '-']).unwrap_or(s);
    let Some(epos) = d.find(['e', 'E']) else {
        return false;
    };
    let (mantissa, exp) = (&d[..epos], &d[epos + 1..]);
    let exp = exp.strip_prefix(['+', '-']).unwrap_or(exp);
    if exp.is_empty() || !exp.bytes().all(|b| b.is_ascii_digit()) {
        return false;
    }
    match mantissa.split_once('.') {
        Some((int, frac)) => {
            !(int.is_empty() && frac.is_empty())
                && int.bytes().all(|b| b.is_ascii_digit())
                && frac.bytes().all(|b| b.is_ascii_digit())
        }
        None => !mantissa.is_empty() && mantissa.bytes().all(|b| b.is_ascii_digit()),
    }
}

fn is_concise_iri(bytes: &[u8], iri: &str) -> bool {
    bytes.first() == Some(&b'>') && &bytes[1..] == iri.as_bytes()
}

/// Conservative check that a local name needs no escaping (chars from
/// PN_CHARS plus '.' not at the edges).
fn is_safe_local(local: &str) -> bool {
    if local.is_empty() {
        return true;
    }
    if local.starts_with('.') || local.ends_with('.') {
        return false;
    }
    let mut chars = local.chars();
    let first = chars.next().expect("nonempty");
    (crate::tables::is_pn_chars_u(first) || first.is_ascii_digit())
        && local
            .chars()
            .skip(1)
            .all(|c| crate::tables::is_pn_chars(c) || c == '.')
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{NQuadsParser, Options, TriGParser, TurtleParser};

    fn quads_of_nq(input: &str) -> Vec<String> {
        let mut p = NQuadsParser::new(Options::default()).unwrap();
        p.feed(input.as_bytes()).unwrap();
        let mut out: Vec<String> = p
            .drain()
            .map(|q| format!("{:?}|{:?}|{:?}|{:?}", q.s, q.p, q.o, q.g))
            .collect();
        p.finish().unwrap();
        out.extend(
            p.drain()
                .map(|q| format!("{:?}|{:?}|{:?}|{:?}", q.s, q.p, q.o, q.g)),
        );
        out
    }

    #[test]
    fn nquads_round_trip_fixpoint() {
        let src = concat!(
            "<http://x/s> <http://x/p> \"esc\\\"aped\\n\\\\\" .\n",
            "_:a <http://x/p> \"chat\"@en--rtl <http://x/g> .\n",
            "<http://x/s> <http://x/p> \"5\"^^<http://www.w3.org/2001/XMLSchema#integer> .\n",
            "_:a <http://x/p> <<( <http://x/s> <http://x/q> \"v\" )>> .\n",
        );
        let mut p = NQuadsParser::new(Options::default()).unwrap();
        p.feed(src.as_bytes()).unwrap();
        let mut w = NQuadsWriter::new(Vec::new());
        for q in p.drain() {
            w.write_quad(&q).unwrap();
        }
        p.finish().unwrap();
        let text1 = String::from_utf8(w.into_inner()).unwrap();
        // Parse the writer's output again: must yield identical quads and
        // identical re-serialization (fixpoint).
        let mut p2 = NQuadsParser::new(Options::default()).unwrap();
        p2.feed(text1.as_bytes()).unwrap();
        let mut w2 = NQuadsWriter::new(Vec::new());
        for q in p2.drain() {
            w2.write_quad(&q).unwrap();
        }
        p2.finish().unwrap();
        let text2 = String::from_utf8(w2.into_inner()).unwrap();
        assert_eq!(text1, text2);
        assert_eq!(quads_of_nq(&text1), quads_of_nq(&text2));
    }

    #[test]
    fn turtle_pretty_groups_and_compacts() {
        let src = concat!(
            "@prefix ex: <http://x/> .\n",
            "ex:s a ex:C ; ex:p \"v\", \"w\" .\n",
            "ex:s2 ex:p \"z\" .\n",
        );
        let mut p = TurtleParser::new(Options::default()).unwrap();
        p.feed(src.as_bytes()).unwrap();
        let mut w = TurtleWriter::new(Vec::new()).prefix("ex", "http://x/");
        for q in p.drain() {
            w.write_quad(&q).unwrap();
        }
        p.finish().unwrap();
        let text = String::from_utf8(w.finish().unwrap()).unwrap();
        assert_eq!(
            text,
            concat!(
                "@prefix ex: <http://x/> .\n",
                "\n",
                "ex:s a ex:C ;\n",
                "\tex:p \"v\", \"w\" .\n",
                "\n",
                "ex:s2 ex:p \"z\" .\n",
            )
        );
        // And it reparses to the same quads.
        let mut p2 = TurtleParser::new(Options::default()).unwrap();
        p2.feed(text.as_bytes()).unwrap();
        assert_eq!(p2.drain().count(), 4);
        p2.finish().unwrap();
    }

    #[test]
    fn turtle_pretty_reconstructs_collections() {
        // Nested lists interleave spine quads in raw parser order; the
        // canonical pipeline regroups subjects (`tree`) before writing, so
        // model that here: stable-sort quads by subject first-seen rank.
        let src = concat!(
            "@prefix ex: <http://x/> .\n",
            "ex:s ex:q ( 1 ( 2 3 ) ) ; ex:p \"v\" .\n",
            "ex:t ex:u () .\n",
        );
        type OwnedRow = (Vec<u8>, Vec<u8>, Vec<u8>, Option<Shorthand>);
        let mut p = TurtleParser::new(Options::default()).unwrap();
        let mut quads: Vec<OwnedRow> = Vec::new();
        p.read_from(src.as_bytes(), |q| {
            quads.push((q.s.to_vec(), q.p.to_vec(), q.o.to_vec(), q.shorthand));
        })
        .unwrap();
        let n_in = quads.len();
        let mut rank: Vec<Vec<u8>> = Vec::new();
        for q in &quads {
            if !rank.contains(&q.0) {
                rank.push(q.0.clone());
            }
        }
        quads.sort_by_key(|q| rank.iter().position(|s| s == &q.0).expect("ranked"));

        let mut w = TurtleWriter::new(Vec::new()).prefix("ex", "http://x/");
        for (s, pr, o, sh) in &quads {
            w.write_quad(&QuadRef {
                s,
                p: pr,
                o,
                g: None,
                shorthand: *sh,
            })
            .unwrap();
        }
        let text = String::from_utf8(w.finish().unwrap()).unwrap();
        assert!(
            text.contains("ex:q (\n\t1\n\t(\n\t\t2\n\t\t3\n\t)\n) ;"),
            "{text}"
        );
        assert!(text.contains("ex:u ()"), "{text}");
        assert!(!text.contains("rdf-syntax-ns#first"), "{text}");
        // Round-trips to the same number of quads.
        let mut p2 = TurtleParser::new(Options::default()).unwrap();
        let mut n = 0;
        p2.read_from(text.as_bytes(), |_| n += 1).unwrap();
        assert_eq!(n, n_in, "{text}");
    }

    /// Hand-fed streams: ineligible or malformed spines always stay explicit.
    #[test]
    fn collection_reconstruction_safety() {
        let iri = |i: &str| format!(">{i}").into_bytes();
        let q = |s: &[u8], p: &[u8], o: &[u8]| -> (Vec<u8>, Vec<u8>, Vec<u8>) {
            (s.to_vec(), p.to_vec(), o.to_vec())
        };
        let first = iri(vocab::RDF_FIRST);
        let rest = iri(vocab::RDF_REST);
        let nil = iri(vocab::RDF_NIL);
        let write_all = |quads: &[(Vec<u8>, Vec<u8>, Vec<u8>)]| -> String {
            let mut w = TurtleWriter::new(Vec::new());
            for (s, p, o) in quads {
                w.write_quad(&QuadRef {
                    s,
                    p,
                    o,
                    g: None,
                    shorthand: None,
                })
                .unwrap();
            }
            String::from_utf8(w.finish().unwrap()).unwrap()
        };

        // Surface-labeled spine (user wrote `_:x rdf:first …`): shared
        // references are possible, so it must never inline.
        let out = write_all(&[
            q(b"_sx", &first, b"\"one".as_ref()),
            q(b"_sx", &rest, &nil),
            q(&iri("http://x/a"), &iri("http://x/p"), b"_sx"),
            q(&iri("http://x/b"), &iri("http://x/q"), b"_sx"),
        ]);
        assert!(out.contains("#first"), "{out}");
        assert_eq!(out.matches("_:sx").count(), 3, "{out}");

        // Fresh-labeled node with an extra property: not a pure spine.
        let out = write_all(&[
            q(b"_b0", &first, b"\"one".as_ref()),
            q(b"_b0", &rest, &nil),
            q(b"_b0", &iri("http://x/extra"), b"\"z".as_ref()),
            q(&iri("http://x/a"), &iri("http://x/p"), b"_b0"),
        ]);
        // …as an anonymous [ … ] block, not a collection, keeping all props.
        assert!(out.contains("[\n"), "{out}");
        assert!(out.contains("#first"), "{out}");
        assert!(!out.contains("(\n"), "{out}");
        assert!(!out.contains("_:b0"), "{out}");

        // Orphan spine (reference never arrives): flushed as a subject-anon
        // at the boundary (fresh labels are single-reference, so an
        // unconsumed capture is provably unreferenced).
        let out = write_all(&[q(b"_b0", &first, b"\"one".as_ref()), q(b"_b0", &rest, &nil)]);
        assert!(out.starts_with("[] "), "{out}");
        assert!(out.contains("#first"), "{out}");
    }

    /// Regression: a surface `_:label` referenced more than once keeps its
    /// label in default mode. The Turtle parser maps surface labels to
    /// `s{n}` ordinals precisely so the writer never mistakes them for
    /// inline-safe minted blanks — splicing the definition at the first
    /// reference would leave the second rendering as `[]`, silently
    /// splitting one shared node into two.
    #[test]
    fn turtle_shared_surface_label_stays_labeled() {
        let src = "_:x <urn:p> \"v\" . <urn:a> <urn:q> _:x . <urn:b> <urn:q> _:x .";
        let mut p = TurtleParser::new(Options::default()).unwrap();
        let mut w = TurtleWriter::new(Vec::new());
        p.read_from(src.as_bytes(), |q| w.write_quad(&q).unwrap())
            .unwrap();
        let text = String::from_utf8(w.finish().unwrap()).unwrap();
        assert_eq!(text.matches("_:s0").count(), 3, "{text}");
        assert!(!text.contains('['), "{text}");
        // Reparses to three quads sharing a single blank node.
        let mut p2 = TurtleParser::new(Options::default()).unwrap();
        let mut blanks = Vec::new();
        let mut n = 0;
        p2.read_from(text.as_bytes(), |q| {
            n += 1;
            for t in [q.s, q.o] {
                if t.first() == Some(&b'_') {
                    blanks.push(t.to_vec());
                }
            }
        })
        .unwrap();
        assert_eq!(n, 3, "{text}");
        assert_eq!(blanks.len(), 3, "{text}");
        assert!(blanks.iter().all(|b| b == &blanks[0]), "{text}");
    }

    /// A fresh-shaped label referenced twice — legal in non-parser streams
    /// (query results, store merges). Labeled mode must keep the label at
    /// every site instead of splicing the definition into one of them.
    #[test]
    fn labeled_blanks_keeps_shared_nodes() {
        let iri = |i: &str| format!(">{i}").into_bytes();
        let quads = [
            (b"_b0".to_vec(), iri("http://x/p"), b"\"v".to_vec()),
            (iri("http://x/a"), iri("http://x/q"), b"_b0".to_vec()),
            (iri("http://x/b"), iri("http://x/q"), b"_b0".to_vec()),
        ];
        let mut w = TurtleWriter::new(Vec::new()).labeled_blanks();
        for (s, p, o) in &quads {
            w.write_quad(&QuadRef {
                s,
                p,
                o,
                g: None,
                shorthand: None,
            })
            .unwrap();
        }
        let text = String::from_utf8(w.finish().unwrap()).unwrap();
        assert_eq!(text.matches("_:b0").count(), 3, "{text}");
        assert!(!text.contains('['), "{text}");
        // All three quads survive a reparse.
        let mut p2 = TurtleParser::new(Options::default()).unwrap();
        let mut n = 0;
        p2.read_from(text.as_bytes(), |_| n += 1).unwrap();
        assert_eq!(n, 3, "{text}");
    }

    /// `used_prefixes_only`: the header carries exactly the declarations
    /// compaction used, in declaration order, unused ones dropped.
    #[test]
    fn used_prefixes_only_header() {
        let iri = |i: &str| format!(">{i}").into_bytes();
        let quads = [
            (iri("http://x/s"), iri("http://x/p"), iri("http://y/o")),
            // An unsafe local (`.`-terminated) that must stay a full IRI.
            (iri("http://x/s"), iri("http://x/p"), iri("http://y/end.")),
        ];
        let mut w = TurtleWriter::new(Vec::new())
            .used_prefixes_only()
            .prefix("ex", "http://x/")
            .prefix("unused", "http://elsewhere/")
            .prefix("why", "http://y/");
        for (s, p, o) in &quads {
            w.write_quad(&QuadRef {
                s,
                p,
                o,
                g: None,
                shorthand: None,
            })
            .unwrap();
        }
        let text = String::from_utf8(w.finish().unwrap()).unwrap();
        assert_eq!(
            text,
            concat!(
                "@prefix ex: <http://x/> .\n",
                "@prefix why: <http://y/> .\n",
                "\n",
                "ex:s ex:p why:o, <http://y/end.> .\n",
            )
        );
    }

    /// `terse_literals`: typed literals from provenance-free streams render
    /// as bare tokens exactly when the lexical is a valid Turtle token, and
    /// everything round-trips with lexical and datatype intact.
    #[test]
    fn terse_literals_derives_bare_tokens() {
        let iri = |i: &str| {
            let mut v = Vec::new();
            graphy_core::concise::encode_iri(&mut v, i);
            v
        };
        let typed = |lex: &str, dt: &str| {
            let mut v = Vec::new();
            graphy_core::concise::encode_datatype(&mut v, lex, dt);
            v
        };
        let s = iri("http://x/s");
        let p = iri("http://x/p");
        let objects = [
            typed("true", vocab::XSD_BOOLEAN),
            typed("5", vocab::XSD_INTEGER),
            typed("-0.5", vocab::XSD_DECIMAL),
            typed("4.2E9", vocab::XSD_DOUBLE),
            // Valid XSD lexicals with no bare Turtle spelling: stay quoted.
            typed("TRUE", vocab::XSD_BOOLEAN),
            typed(" 5", vocab::XSD_INTEGER),
            typed("1.", vocab::XSD_DECIMAL),
            typed("NaN", vocab::XSD_DOUBLE),
            // Non-shorthand datatype: stays quoted regardless of lexical.
            typed("5", "http://www.w3.org/2001/XMLSchema#byte"),
        ];
        let mut w = TurtleWriter::new(Vec::new()).terse_literals();
        for o in &objects {
            w.write_quad(&QuadRef {
                s: &s,
                p: &p,
                o,
                g: None,
                shorthand: None,
            })
            .unwrap();
        }
        let text = String::from_utf8(w.finish().unwrap()).unwrap();
        assert!(text.contains("true, 5, -0.5, 4.2E9, \"TRUE\"^^"), "{text}");
        for quoted in [
            "\" 5\"^^",
            "\"1.\"^^",
            "\"NaN\"^^",
            "\"5\"^^<http://www.w3.org/2001/XMLSchema#byte>",
        ] {
            assert!(text.contains(quoted), "{text}");
        }
        // Every object survives a reparse byte-identically (bare tokens
        // re-encode as the same datatyped concise term).
        let mut p2 = TurtleParser::new(Options::default()).unwrap();
        let mut got: Vec<Vec<u8>> = Vec::new();
        p2.read_from(text.as_bytes(), |q| got.push(q.o.to_vec()))
            .unwrap();
        let mut want = objects.to_vec();
        want.sort();
        got.sort();
        assert_eq!(want, got, "{text}");
    }

    /// Datatype IRIs compact against the declared prefixes; without
    /// `terse_literals`, even shorthand-eligible literals stay quoted.
    #[test]
    fn datatype_iris_compact_against_prefixes() {
        let iri = |i: &str| {
            let mut v = Vec::new();
            graphy_core::concise::encode_iri(&mut v, i);
            v
        };
        let typed = |lex: &str, dt: &str| {
            let mut v = Vec::new();
            graphy_core::concise::encode_datatype(&mut v, lex, dt);
            v
        };
        let s = iri("http://x/s");
        let p = iri("http://x/p");
        let objects = [
            typed("2020", "http://www.w3.org/2001/XMLSchema#gYear"),
            typed("5", vocab::XSD_INTEGER),
        ];
        let mut w =
            TurtleWriter::new(Vec::new()).prefix("xsd", "http://www.w3.org/2001/XMLSchema#");
        for o in &objects {
            w.write_quad(&QuadRef {
                s: &s,
                p: &p,
                o,
                g: None,
                shorthand: None,
            })
            .unwrap();
        }
        let text = String::from_utf8(w.finish().unwrap()).unwrap();
        assert!(text.contains("\"2020\"^^xsd:gYear"), "{text}");
        assert!(text.contains("\"5\"^^xsd:integer"), "{text}");
    }

    #[test]
    fn trig_pretty_graph_blocks() {
        let src = concat!(
            "@prefix ex: <http://x/> .\n",
            "ex:g { ex:s ex:p ex:o . ex:s ex:q ex:o2 }\n",
            "ex:top ex:p ex:o .\n",
        );
        let mut p = TriGParser::new(Options::default()).unwrap();
        p.feed(src.as_bytes()).unwrap();
        let mut w = TurtleWriter::new(Vec::new())
            .trig()
            .prefix("ex", "http://x/");
        for q in p.drain() {
            w.write_quad(&q).unwrap();
        }
        p.finish().unwrap();
        let text = String::from_utf8(w.finish().unwrap()).unwrap();
        assert_eq!(
            text,
            concat!(
                "@prefix ex: <http://x/> .\n",
                "\n",
                "ex:g {\n",
                "\tex:s ex:p ex:o ;\n",
                "\t\tex:q ex:o2 .\n",
                "}\n",
                "\n",
                "ex:top ex:p ex:o .\n",
            )
        );
        // Round-trip through the TriG parser.
        let mut p2 = TriGParser::new(Options::default()).unwrap();
        p2.feed(text.as_bytes()).unwrap();
        assert_eq!(p2.drain().count(), 3);
        p2.finish().unwrap();
    }
}
