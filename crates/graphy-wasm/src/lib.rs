//! graphy-wasm (docs/11): the quad-store + SPARQL engine, client-side.
//!
//! Wraps an [ephemeral store](graphy_store::Store::ephemeral) — every quad
//! lives in the in-memory delta; the W3C-conformant Update executor and the
//! evaluators run unmodified. Queries go through the VECTORIZED engine
//! (docs/11 §6): on wasm its scheduler resolves to one worker
//! (`available_parallelism` is unavailable) and runs morsels inline on the
//! calling thread — no spawn, no ambient clock (deadlines default off);
//! planner gaps fall back to the reference evaluator automatically. `NOW()`
//! and the rng seed come from an injected wall clock (`Date.now()` on wasm)
//! set before each evaluation.
//!
//! API sketch (docs/11 §4): `load` / `update` / `query` / `export` / `size`.

use graphy_algebra::{rewrite, translate_query, translate_update};
use graphy_core::concise::decode;
use graphy_core::TermRef;
use graphy_engine::{evaluate_with, execute_update, set_wall_clock_millis, ExecOptions, Output};
use graphy_sparql_syntax::{parse_query, parse_update};
use graphy_store::{Order, QuadBatch, Snapshot, Store, TermPos};
use graphy_turtle::{
    par, write_term, NQuadsParser, NTriplesParser, Options, TriGParser, TurtleParser,
};
use wasm_bindgen::prelude::*;

mod server;
#[cfg(target_arch = "wasm32")]
pub use server::GraphyStoreServer;
pub use server::{Reply, StoreServer};

/// The target-agnostic core (natively testable): SPARQL 1.1/1.2 Query +
/// Update over an in-memory (ephemeral, non-durable) dataset. The
/// [`GraphyStore`] wasm-bindgen wrapper is a thin `String`→`JsError`
/// boundary over this.
#[derive(Debug)]
pub struct GraphStore {
    store: Store,
    /// Worker threads for the vectorized engine (1 = inline). >1 only takes
    /// effect on wasm under the `wasm-threads` build; otherwise clamped.
    threads: std::sync::atomic::AtomicUsize,
}

fn err(e: impl std::fmt::Display) -> String {
    e.to_string()
}

/// Fix the evaluation wall clock (NOW(), rng seed). On wasm the ambient
/// clock panics, so inject `Date.now()`; native keeps its ambient clock.
fn set_clock() {
    #[cfg(target_arch = "wasm32")]
    set_wall_clock_millis(Some(js_sys::Date::now() as u64));
    #[cfg(not(target_arch = "wasm32"))]
    set_wall_clock_millis(None);
}

type OwnedQuad = (Vec<u8>, Vec<u8>, Vec<u8>, Option<Vec<u8>>);

/// Every quad of the snapshot as owned concise rows (SPO order).
fn all_quads(snap: &Snapshot) -> Result<Vec<OwnedQuad>, String> {
    let Some(pat) = snap.resolve_pattern(None, None, None, None) else {
        return Ok(Vec::new());
    };
    let mut scan = snap.scan(&pat, Order::Spo).map_err(err)?;
    let mut batch = QuadBatch::new();
    let mut out = Vec::new();
    while scan.next_batch(&mut batch).map_err(err)? {
        for i in 0..batch.len() {
            let s = snap
                .decode_value(batch.s[i], TermPos::Subject)
                .map_err(err)?;
            let p = snap
                .decode_value(batch.p[i], TermPos::Predicate)
                .map_err(err)?;
            let o = snap
                .decode_value(batch.o[i], TermPos::Object)
                .map_err(err)?;
            let g = if batch.g[i] > 0 {
                Some(snap.decode_value(batch.g[i], TermPos::Graph).map_err(err)?)
            } else {
                None
            };
            out.push((s, p, o, g));
        }
    }
    Ok(out)
}

impl GraphStore {
    /// A fresh, empty, in-memory store.
    pub fn new() -> Result<GraphStore, String> {
        Ok(GraphStore {
            store: Store::ephemeral().map_err(err)?,
            threads: std::sync::atomic::AtomicUsize::new(1),
        })
    }

    /// A persistent-capable store (docs/11 OPFS): pass the previously
    /// captured log (or `None` to start fresh); commits accumulate WAL
    /// frames for [`GraphStore::drain_log`].
    pub fn with_log(log: Option<&[u8]>) -> Result<GraphStore, String> {
        Ok(GraphStore {
            store: Store::ephemeral_persistent(log).map_err(err)?,
            threads: std::sync::atomic::AtomicUsize::new(1),
        })
    }

    /// A store over a segment byte image (docs/11 §6 "scale"): `files` are
    /// the segment's component files — built natively with `graphy load`,
    /// fetched or OPFS-loaded in the browser — and `log` is the separately
    /// persisted edit log layered on top.
    pub fn from_image(
        files: &[(String, Vec<u8>)],
        log: Option<&[u8]>,
    ) -> Result<GraphStore, String> {
        let refs: Vec<(&str, &[u8])> = files
            .iter()
            .map(|(r, b)| (r.as_str(), b.as_slice()))
            .collect();
        Ok(GraphStore {
            store: Store::open_image(&refs, log).map_err(err)?,
            threads: std::sync::atomic::AtomicUsize::new(1),
        })
    }

    /// Worker threads for query evaluation. On wasm this only takes effect
    /// under the `wasm-threads` build (atomics + web workers; the caller
    /// must run off the main thread) — otherwise clamped to 1.
    pub fn set_threads(&self, n: usize) {
        let clamp = cfg!(all(target_arch = "wasm32", not(feature = "wasm-threads")));
        let n = if clamp { 1 } else { n.max(1) };
        self.threads.store(n, std::sync::atomic::Ordering::Relaxed);
    }

    /// Committed WAL frames since the last drain — append them, verbatim
    /// and in order, to the durable log. Empty when nothing new committed
    /// (or on non-persistent stores).
    pub fn drain_log(&self) -> Vec<u8> {
        self.store.drain_wal_capture()
    }

    /// Log compaction: the whole dataset as a single-transaction image.
    /// Atomically replace the durable log with this, then keep appending
    /// subsequent [`GraphStore::drain_log`] output.
    pub fn pack_log(&self) -> Result<Vec<u8>, String> {
        self.store.pack_log().map_err(err)
    }

    /// Parse `text` (`"turtle" | "trig" | "ntriples" | "nquads"`, with an
    /// optional base IRI for relative references) and add every quad.
    /// Returns the number of quads now in the store.
    pub fn load(&self, text: &str, format: &str, base: Option<String>) -> Result<u32, String> {
        // Blank labels are document-scoped: every load gets a random
        // namespace that remains distinct across parallel calls and
        // persisted-store restarts.
        let options = Options {
            base,
            label_ns: Some(graphy_protocol::fresh_label_ns()),
            ..Options::default()
        };
        let threads = self.threads.load(std::sync::atomic::Ordering::Relaxed);
        // Data-parallel N-Triples/N-Quads parse (docs/11 §6): worker-backed
        // on wasm-threads builds, std threads natively; set_threads clamps
        // to 1 where neither applies.
        if threads > 1 && matches!(format, "ntriples" | "nt" | "nquads" | "nq") {
            let collected: std::sync::Mutex<Vec<OwnedQuad>> = std::sync::Mutex::new(Vec::new());
            let sink = |_seg: usize, q: graphy_turtle::QuadRef<'_>| {
                collected.lock().expect("collector").push((
                    q.s.to_vec(),
                    q.p.to_vec(),
                    q.o.to_vec(),
                    q.g.map(<[u8]>::to_vec),
                ));
            };
            match format {
                "ntriples" | "nt" => {
                    par::ntriples(text.as_bytes(), &options, threads, sink).map_err(err)?
                }
                _ => par::nquads(text.as_bytes(), &options, threads, sink).map_err(err)?,
            }
            let quads = collected.into_inner().expect("collector");
            let adds: Vec<_> = quads
                .iter()
                .map(|(s, p, o, g)| (s.as_slice(), p.as_slice(), o.as_slice(), g.as_deref()))
                .collect();
            self.store.apply(&[], &adds).map_err(err)?;
            return self.size();
        }
        let mut quads: Vec<OwnedQuad> = Vec::new();
        let mut sink = |q: graphy_turtle::QuadRef<'_>| {
            quads.push((
                q.s.to_vec(),
                q.p.to_vec(),
                q.o.to_vec(),
                q.g.map(<[u8]>::to_vec),
            ));
        };
        match format {
            "turtle" | "ttl" => {
                let mut p = TurtleParser::new(options).map_err(err)?;
                p.read_from(text.as_bytes(), &mut sink).map_err(err)?;
            }
            "trig" => {
                let mut p = TriGParser::new(options).map_err(err)?;
                p.read_from(text.as_bytes(), &mut sink).map_err(err)?;
            }
            "ntriples" | "nt" => {
                let mut p = NTriplesParser::new(options).map_err(err)?;
                p.read_from(text.as_bytes(), &mut sink).map_err(err)?;
            }
            "nquads" | "nq" => {
                let mut p = NQuadsParser::new(options).map_err(err)?;
                p.read_from(text.as_bytes(), &mut sink).map_err(err)?;
            }
            other => return Err(format!("unknown format {other:?}")),
        }
        let adds: Vec<_> = quads
            .iter()
            .map(|(s, p, o, g)| (s.as_slice(), p.as_slice(), o.as_slice(), g.as_deref()))
            .collect();
        let snap = self.store.apply(&[], &adds).map_err(err)?;
        drop(snap);
        self.size()
    }

    /// Execute a SPARQL Update request (atomic per operation).
    pub fn update(&self, sparql: &str) -> Result<(), String> {
        set_clock();
        let u = parse_update(sparql).map_err(err)?;
        let t = translate_update(&u).map_err(err)?;
        // A large INSERT DATA's parse AST rivals the data itself; release
        // it before execution (wasm32 peak-memory discipline, docs/11).
        drop(u);
        execute_update(&self.store, &t).map_err(err)
    }

    /// Evaluate a SPARQL query. SELECT/ASK return SPARQL 1.1 Results JSON;
    /// CONSTRUCT/DESCRIBE return canonical N-Triples text.
    pub fn query(&self, sparql: &str) -> Result<String, String> {
        set_clock();
        let q = parse_query(sparql).map_err(err)?;
        let mut t = translate_query(&q).map_err(err)?;
        t.root = rewrite(t.root.clone());
        let snap = self.store.snapshot();
        let opts = ExecOptions {
            threads: self.threads.load(std::sync::atomic::Ordering::Relaxed),
            ..ExecOptions::default()
        };
        match evaluate_with(&snap, &t, &opts).map_err(err)? {
            Output::Solutions { vars, rows } => Ok(srj_solutions(&vars, &rows)),
            Output::Boolean(b) => Ok(format!("{{\"head\":{{}},\"boolean\":{b}}}")),
            Output::Triples(triples) => {
                let mut out = Vec::new();
                for (s, p, o) in &triples {
                    for term in [s, p, o] {
                        write_term(&mut out, decode(term).map_err(err)?).map_err(err)?;
                        out.push(b' ');
                    }
                    out.extend_from_slice(b".\n");
                }
                String::from_utf8(out).map_err(err)
            }
        }
    }

    /// Serialize the whole dataset: `"nquads"` (canonical) or
    /// `"turtle"`/`"trig"` (pretty: `tree`-style subject grouping, prefix
    /// compaction, labeled blank nodes).
    pub fn export(&self, format: &str) -> Result<String, String> {
        let snap = self.store.snapshot();
        let quads = all_quads(&snap)?;
        match format {
            "nquads" | "nq" => {
                let mut out = Vec::new();
                for (s, p, o, g) in &quads {
                    for term in [Some(s), Some(p), Some(o), g.as_ref()]
                        .into_iter()
                        .flatten()
                    {
                        write_term(&mut out, decode(term).map_err(err)?).map_err(err)?;
                        out.push(b' ');
                    }
                    out.extend_from_slice(b".\n");
                }
                String::from_utf8(out).map_err(err)
            }
            "turtle" | "ttl" | "trig" => {
                let trig = format == "trig";
                if !trig && quads.iter().any(|(_, _, _, g)| g.is_some()) {
                    return Err("dataset has named graphs: export as \"trig\"".to_string());
                }
                // Store scans carry no syntactic provenance, so terse mode
                // derives bare literal tokens from the datatype. They carry
                // no single-reference guarantee either — updates can copy a
                // fresh-shaped `b{n}` node into new triples — so blank nodes
                // stay labeled: `( … )`/`[ … ]` reconstruction would split a
                // multiply-referenced node.
                let mut w = graphy_turtle::TurtleWriter::new(Vec::new())
                    .labeled_blanks()
                    .terse_literals();
                if trig {
                    w = w.trig();
                }
                // Tree-style regrouping already holds: the SPO scan yields
                // one contiguous run per subject.
                for (s, p, o, g) in &quads {
                    w.write_quad(&graphy_turtle::QuadRef {
                        s,
                        p,
                        o,
                        g: g.as_deref(),
                        shorthand: None,
                    })
                    .map_err(err)?;
                }
                String::from_utf8(w.finish().map_err(err)?).map_err(err)
            }
            other => Err(format!("unknown format {other:?}")),
        }
    }

    /// The number of quads in the store.
    pub fn size(&self) -> Result<u32, String> {
        Ok(all_quads(&self.store.snapshot())?.len() as u32)
    }
}

// ------------------------------------------------ SPARQL 1.1 Results JSON

fn srj_solutions(vars: &[String], rows: &[Vec<Option<Vec<u8>>>]) -> String {
    let mut out = String::from("{\"head\":{\"vars\":[");
    for (i, v) in vars.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push_str(&json_string(v));
    }
    out.push_str("]},\"results\":{\"bindings\":[");
    for (i, row) in rows.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push('{');
        let mut first = true;
        for (v, cell) in vars.iter().zip(row) {
            let Some(bytes) = cell else { continue };
            if !first {
                out.push(',');
            }
            first = false;
            out.push_str(&json_string(v));
            out.push(':');
            srj_term(&mut out, bytes);
        }
        out.push('}');
    }
    out.push_str("]}}");
    out
}

fn srj_term(out: &mut String, concise: &[u8]) {
    match decode(concise) {
        Ok(TermRef::Iri(i)) => {
            out.push_str("{\"type\":\"uri\",\"value\":");
            out.push_str(&json_string(i));
            out.push('}');
        }
        Ok(TermRef::BlankNode(b)) => {
            out.push_str("{\"type\":\"bnode\",\"value\":");
            out.push_str(&json_string(b));
            out.push('}');
        }
        Ok(TermRef::Literal(l)) => {
            out.push_str("{\"type\":\"literal\",\"value\":");
            out.push_str(&json_string(l.lexical()));
            if let Some((tag, dir)) = l.lang() {
                out.push_str(",\"xml:lang\":");
                out.push_str(&json_string(tag));
                if let Some(d) = dir {
                    out.push_str(",\"its:dir\":");
                    out.push_str(&json_string(if d == graphy_turtle::Dir::Ltr {
                        "ltr"
                    } else {
                        "rtl"
                    }));
                }
            } else {
                let dt = l.datatype();
                if dt != "http://www.w3.org/2001/XMLSchema#string" {
                    out.push_str(",\"datatype\":");
                    out.push_str(&json_string(dt));
                }
            }
            out.push('}');
        }
        Ok(TermRef::TripleTerm(t)) => {
            // SPARQL 1.2 results form: a nested triple value.
            out.push_str("{\"type\":\"triple\",\"value\":{\"subject\":");
            let (s, p, o) = (t.subject(), t.predicate(), t.object());
            let mut buf = Vec::new();
            crate::reencode(&mut buf, &s);
            srj_term(out, &buf);
            out.push_str(",\"predicate\":");
            buf.clear();
            crate::reencode(&mut buf, &p);
            srj_term(out, &buf);
            out.push_str(",\"object\":");
            buf.clear();
            crate::reencode(&mut buf, &o);
            srj_term(out, &buf);
            out.push_str("}}");
        }
        Err(_) => out.push_str("{\"type\":\"literal\",\"value\":\"<invalid term>\"}"),
    }
}

/// Concise re-encoding of a decoded component term (triple-term recursion).
fn reencode(out: &mut Vec<u8>, t: &TermRef<'_>) {
    use graphy_core::{concise, vocab};
    match t {
        TermRef::Iri(i) => concise::encode_iri(out, i),
        TermRef::BlankNode(b) => concise::encode_blank(out, b),
        TermRef::Literal(l) => {
            if let Some((tag, dir)) = l.lang() {
                concise::encode_lang(out, l.lexical(), tag, dir);
            } else if l.datatype() == vocab::XSD_STRING {
                concise::encode_simple(out, l.lexical());
            } else {
                concise::encode_datatype(out, l.lexical(), l.datatype());
            }
        }
        TermRef::TripleTerm(view) => {
            let mut s = Vec::new();
            reencode(&mut s, &view.subject());
            let mut p = Vec::new();
            reencode(&mut p, &view.predicate());
            let mut o = Vec::new();
            reencode(&mut o, &view.object());
            concise::encode_triple_term(out, &s, &p, &o);
        }
    }
}

fn json_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// The wasm-bindgen boundary over [`GraphStore`]. The `Arc` exists for the
/// `wasm-threads` async API, where an operation runs on a spawned worker
/// thread that must co-own the store.
#[wasm_bindgen]
#[derive(Debug)]
pub struct GraphyStore {
    inner: std::sync::Arc<GraphStore>,
}

fn js(e: String) -> JsError {
    JsError::new(&e)
}

#[wasm_bindgen]
impl GraphyStore {
    /// A fresh, empty, in-memory store.
    #[wasm_bindgen(constructor)]
    pub fn new() -> Result<GraphyStore, JsError> {
        Ok(GraphyStore {
            inner: std::sync::Arc::new(GraphStore::new().map_err(js)?),
        })
    }

    /// Parse `text` (`"turtle" | "trig" | "ntriples" | "nquads"`) and add
    /// every quad; returns the store's new size.
    pub fn load(&self, text: &str, format: &str, base: Option<String>) -> Result<u32, JsError> {
        self.inner.load(text, format, base).map_err(js)
    }

    /// Execute a SPARQL Update request (atomic per operation).
    pub fn update(&self, sparql: &str) -> Result<(), JsError> {
        self.inner.update(sparql).map_err(js)
    }

    /// Evaluate a SPARQL query: SELECT/ASK → SPARQL Results JSON,
    /// CONSTRUCT/DESCRIBE → canonical N-Triples text.
    pub fn query(&self, sparql: &str) -> Result<String, JsError> {
        self.inner.query(sparql).map_err(js)
    }

    /// Serialize the dataset: `"nquads"`, `"turtle"`, or `"trig"`.
    pub fn export(&self, format: &str) -> Result<String, JsError> {
        self.inner.export(format).map_err(js)
    }

    /// The number of quads in the store.
    pub fn size(&self) -> Result<u32, JsError> {
        self.inner.size().map_err(js)
    }

    /// Worker threads for query evaluation (wasm-threads builds only;
    /// call from a worker context, never the main thread).
    #[wasm_bindgen(js_name = setThreads)]
    pub fn set_threads(&self, n: usize) {
        self.inner.set_threads(n);
    }

    /// A store over a fetched/OPFS-loaded segment image: parallel arrays of
    /// component file names and their bytes, plus an optional edit log.
    #[cfg(target_arch = "wasm32")]
    #[wasm_bindgen(js_name = fromSegment)]
    pub fn from_segment(
        names: Vec<String>,
        blobs: Vec<js_sys::Uint8Array>,
        log: Option<Box<[u8]>>,
    ) -> Result<GraphyStore, JsError> {
        if names.len() != blobs.len() {
            return Err(JsError::new("names/blobs length mismatch"));
        }
        let files: Vec<(String, Vec<u8>)> = names
            .into_iter()
            .zip(blobs)
            .map(|(n, b)| (n, b.to_vec()))
            .collect();
        Ok(GraphyStore {
            inner: std::sync::Arc::new(GraphStore::from_image(&files, log.as_deref()).map_err(js)?),
        })
    }

    /// A persistent-capable store: pass the previously captured log bytes
    /// (or nothing to start fresh). See `drainLog`/`packLog`.
    #[wasm_bindgen(js_name = withLog)]
    pub fn with_log(log: Option<Box<[u8]>>) -> Result<GraphyStore, JsError> {
        Ok(GraphyStore {
            inner: std::sync::Arc::new(GraphStore::with_log(log.as_deref()).map_err(js)?),
        })
    }

    /// Committed WAL frames since the last drain — append them, verbatim
    /// and in order, to the durable log (e.g. an OPFS file).
    #[wasm_bindgen(js_name = drainLog)]
    pub fn drain_log(&self) -> Vec<u8> {
        self.inner.drain_log()
    }

    /// Log compaction: the whole dataset as a single-transaction image;
    /// atomically replace the durable log with it.
    #[wasm_bindgen(js_name = packLog)]
    pub fn pack_log(&self) -> Result<Vec<u8>, JsError> {
        self.inner.pack_log().map_err(js)
    }
}

/// The `wasm-threads` async API (docs/11 §6). Multi-threaded operations
/// must not run on the main thread (blocking scope-joins are banned there),
/// and worker-spawn requests relay through the thread that instantiated the
/// module — so the supported topology is: call these FROM the main thread;
/// the operation runs on a spawned worker thread (which may itself fan out
/// to more workers) and the Promise resolves without ever blocking the
/// caller.
#[cfg(all(target_arch = "wasm32", feature = "wasm-threads"))]
#[wasm_bindgen]
impl GraphyStore {
    /// [`GraphyStore::load`] on a worker thread; parses N-Triples/N-Quads
    /// data-parallel when `setThreads` > 1. Resolves to the store's new
    /// size.
    #[wasm_bindgen(js_name = loadAsync)]
    pub fn load_async(
        &self,
        text: String,
        format: String,
        base: Option<String>,
    ) -> js_sys::Promise {
        let inner = self.inner.clone();
        wasm_bindgen_futures::future_to_promise(async move {
            let n = join_off_main(move || inner.load(&text, &format, base)).await?;
            Ok(JsValue::from(n))
        })
    }

    /// [`GraphyStore::query`] on a worker thread; engages the morsel pool
    /// when `setThreads` > 1 and the driving scan is large enough.
    #[wasm_bindgen(js_name = queryAsync)]
    pub fn query_async(&self, sparql: String) -> js_sys::Promise {
        let inner = self.inner.clone();
        wasm_bindgen_futures::future_to_promise(async move {
            let s = join_off_main(move || inner.query(&sparql)).await?;
            Ok(JsValue::from(s))
        })
    }

    /// [`GraphyStore::update`] on a worker thread.
    #[wasm_bindgen(js_name = updateAsync)]
    pub fn update_async(&self, sparql: String) -> js_sys::Promise {
        let inner = self.inner.clone();
        wasm_bindgen_futures::future_to_promise(async move {
            join_off_main(move || inner.update(&sparql)).await?;
            Ok(JsValue::UNDEFINED)
        })
    }
}

/// Run `f` on a spawned worker thread and await its result without
/// blocking the calling thread.
#[cfg(all(target_arch = "wasm32", feature = "wasm-threads"))]
async fn join_off_main<T: Send + 'static>(
    f: impl FnOnce() -> Result<T, String> + Send + 'static,
) -> Result<T, JsValue> {
    match wasm_thread::spawn(f).join_async().await {
        Ok(Ok(v)) => Ok(v),
        Ok(Err(e)) => Err(JsValue::from(JsError::new(&e))),
        Err(_) => Err(JsValue::from(JsError::new("worker thread panicked"))),
    }
}
