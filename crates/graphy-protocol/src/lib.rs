//! `graphy-protocol` — the transport-free half of the SPARQL 1.1 Protocol + Graph Store
//! Protocol service (doc 06): request-parameter parsing, content negotiation, the
//! protocol dataset override, the snapshot ETag scheme, GSP addressing and graph/dataset
//! body handling, the dataset prefix registry, and the results serializers. graphy-server
//! wraps this in axum; the wasm fetch surface and embedded hosts consume it directly.
//!
//! Errors carry a plain status code + message ([`ProtocolError`]) so transports can map
//! them to their own response types.

pub mod results;

use results::Results;

/// A protocol-level failure: HTTP status + message body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProtocolError {
    pub status: u16,
    pub message: String,
}

impl ProtocolError {
    pub fn new(status: u16, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
        }
    }
}

// -------------------------------------------------------------- parameters

/// Decoded protocol request parameters (query string and/or form body).
#[derive(Default, Debug)]
pub struct Params {
    pub query: Option<String>,
    pub update: Option<String>,
    pub default_graph: Vec<String>,
    pub named_graph: Vec<String>,
    pub using_graph: Vec<String>,
    pub using_named_graph: Vec<String>,
    pub timeout: Option<f64>,
}

pub fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => {
                let hex = |b: u8| (b as char).to_digit(16);
                match (hex(bytes[i + 1]), hex(bytes[i + 2])) {
                    (Some(h), Some(l)) => {
                        out.push((h * 16 + l) as u8);
                        i += 3;
                    }
                    _ => {
                        out.push(b'%');
                        i += 1;
                    }
                }
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Folds a urlencoded query string / form body into `p` (later sources may be layered
/// over earlier ones by calling again).
pub fn parse_form(s: &str, p: &mut Params) {
    for pair in s.split('&') {
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        let v = percent_decode(v);
        match percent_decode(k).as_str() {
            "query" => p.query = Some(v),
            "update" => p.update = Some(v),
            "default-graph-uri" => p.default_graph.push(v),
            "named-graph-uri" => p.named_graph.push(v),
            "using-graph-uri" => p.using_graph.push(v),
            "using-named-graph-uri" => p.using_named_graph.push(v),
            "timeout" => p.timeout = v.parse().ok(),
            _ => {}
        }
    }
}

/// The media-type essence of a Content-Type header value (parameters stripped).
pub fn content_type_essence(value: &str) -> String {
    value.split(';').next().unwrap_or("").trim().to_owned()
}

// ------------------------------------------------------------------ conneg

/// Media types for results conneg, checked in the client's listed
/// order; unknown/absent → JSON.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum ResultsFormat {
    Json,
    Xml,
    Csv,
    Tsv,
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum GraphFormat {
    NTriples,
    Turtle,
}

/// Negotiates the solutions and graph response formats from an Accept header value.
pub fn negotiate(accept: Option<&str>) -> (ResultsFormat, GraphFormat) {
    let accept = accept.unwrap_or("*/*");
    let mut rf = None;
    let mut gf = None;
    for part in accept.split(',') {
        let mt = part.split(';').next().unwrap_or("").trim();
        if rf.is_none() {
            rf = match mt {
                "application/sparql-results+json" | "application/json" => Some(ResultsFormat::Json),
                "application/sparql-results+xml" => Some(ResultsFormat::Xml),
                "text/csv" => Some(ResultsFormat::Csv),
                "text/tab-separated-values" => Some(ResultsFormat::Tsv),
                _ => None,
            };
        }
        if gf.is_none() {
            gf = match mt {
                "application/n-triples" => Some(GraphFormat::NTriples),
                "text/turtle" => Some(GraphFormat::Turtle),
                _ => None,
            };
        }
    }
    (
        rf.unwrap_or(ResultsFormat::Json),
        gf.unwrap_or(GraphFormat::Turtle),
    )
}

/// The engine-failure → status mapping (deadline / memory-budget / other).
pub fn engine_status(msg: &str) -> u16 {
    if msg.contains("deadline") {
        408
    } else if msg.contains("memory budget") {
        503
    } else {
        500
    }
}

// ------------------------------------------------------------ query pieces

/// The snapshot cache validator: `"G<generation>.E<epoch>"`.
pub fn snapshot_etag(snap: &graphy_store::Snapshot) -> String {
    format!("\"G{}.E{}\"", snap.generation(), snap.epoch())
}

/// Protocol dataset parameters override the query's own FROM clauses
/// (SPARQL Protocol §2.1.4).
pub fn apply_dataset_params(
    tq: &mut graphy_algebra::TranslatedQuery,
    default_graph: &[String],
    named_graph: &[String],
) {
    if default_graph.is_empty() && named_graph.is_empty() {
        return;
    }
    tq.dataset.clear();
    for iri in default_graph {
        tq.dataset.push((true, format!(">{iri}").into_bytes()));
    }
    for iri in named_graph {
        tq.dataset.push((false, format!(">{iri}").into_bytes()));
    }
}

/// Protocol `using-graph-uri` / `using-named-graph-uri` parameters define
/// the WHERE dataset for every Modify operation, replacing any textual
/// `USING` clauses in the request.
pub fn apply_using_params(
    update: &mut graphy_algebra::TranslatedUpdate,
    default_graph: &[String],
    named_graph: &[String],
) {
    if default_graph.is_empty() && named_graph.is_empty() {
        return;
    }
    let dataset: Vec<(bool, Vec<u8>)> = default_graph
        .iter()
        .map(|iri| (true, format!(">{iri}").into_bytes()))
        .chain(
            named_graph
                .iter()
                .map(|iri| (false, format!(">{iri}").into_bytes())),
        )
        .collect();
    for op in &mut update.ops {
        if let graphy_algebra::UpdateOpT::Modify { using, .. } = op {
            *using = dataset.clone();
        }
    }
}

/// The minimal service description document (text/turtle).
pub const SERVICE_DESCRIPTION: &str = "\
@prefix sd: <http://www.w3.org/ns/sparql-service-description#> .
[] a sd:Service ;
    sd:supportedLanguage sd:SPARQL11Query, sd:SPARQL11Update ;
    sd:resultFormat
        <http://www.w3.org/ns/formats/SPARQL_Results_JSON> ,
        <http://www.w3.org/ns/formats/SPARQL_Results_XML> ,
        <http://www.w3.org/ns/formats/SPARQL_Results_CSV> ,
        <http://www.w3.org/ns/formats/SPARQL_Results_TSV> ,
        <http://www.w3.org/ns/formats/N-Triples> ,
        <http://www.w3.org/ns/formats/Turtle> .
";

// ---------------------------------------------------------- prefix registry

/// Prefix declarations harvested from uploaded documents (Fuseki-style dataset prefix
/// registry): merged into turtle responses so they stay self-describing.
#[derive(Debug, Default)]
pub struct PrefixRegistry {
    prefixes: std::sync::RwLock<Vec<(String, String)>>,
}

impl PrefixRegistry {
    pub fn register(&self, parsed: &[(String, String)]) {
        if parsed.is_empty() {
            return;
        }
        let mut registry = self.prefixes.write().unwrap();
        for (prefix, ns) in parsed {
            match registry.iter_mut().find(|(p, _)| p == prefix) {
                Some(entry) => entry.1 = ns.clone(),
                None => registry.push((prefix.clone(), ns.clone())),
            }
        }
    }

    /// Registry prefixes overlaid with the query's own prologue (query wins).
    pub fn merged(&self, prologue: &[(String, String)]) -> Vec<(String, String)> {
        let mut merged = self.prefixes.read().unwrap().clone();
        for (prefix, ns) in prologue {
            match merged.iter_mut().find(|(p, _)| p == prefix) {
                Some(entry) => entry.1 = ns.clone(),
                None => merged.push((prefix.clone(), ns.clone())),
            }
        }
        merged
    }
}

/// A compact, self-describing turtle response: triples sorted into subject
/// stanzas and compacted against `prefixes`, with an `@prefix` header carrying
/// only the declarations the body actually uses. Falls back to
/// [`turtle_flat`] if any term fails to render (invalid concise bytes).
pub fn turtle_with_prologue(prefixes: &[(String, String)], r: &Results) -> String {
    turtle_pretty(prefixes, r).unwrap_or_else(|| turtle_flat(prefixes, r))
}

/// The flat form: full `@prefix` header + N-Triples statements. Cheaper than
/// the pretty form (no sort, no compaction scan) and lossy-lenient on
/// invalid term bytes.
pub fn turtle_flat(prefixes: &[(String, String)], r: &Results) -> String {
    let mut body = String::new();
    for (prefix, ns) in prefixes {
        body.push_str(&format!("@prefix {prefix}: <{ns}> .\n"));
    }
    if !prefixes.is_empty() {
        body.push('\n');
    }
    body.push_str(&results::to_ntriples(r));
    body
}

fn turtle_pretty(prefixes: &[(String, String)], r: &Results) -> Option<String> {
    let Results::Triples(triples) = r else {
        unreachable!("solutions serialize as results formats")
    };
    // Result order is arbitrary; sorting on the concise bytes groups the
    // subject stanzas (and predicate runs) and makes output deterministic.
    let mut sorted: Vec<&(Vec<u8>, Vec<u8>, Vec<u8>)> = triples.iter().collect();
    sorted.sort_unstable();
    sorted.dedup();
    // Blank nodes stay labeled: result streams carry no single-reference
    // guarantee, so `( … )`/`[ … ]` reconstruction is unsound here. Literals
    // have no syntactic provenance either, so terse mode derives the bare
    // tokens (`5`, `true`) from the datatype where the lexical allows it.
    let mut w = graphy_turtle::TurtleWriter::new(Vec::new())
        .labeled_blanks()
        .terse_literals()
        .used_prefixes_only();
    for (prefix, ns) in prefixes {
        w = w.prefix(prefix, ns);
    }
    for (s, p, o) in sorted {
        w.write_quad(&graphy_turtle::QuadRef {
            s,
            p,
            o,
            g: None,
            shorthand: None,
        })
        .ok()?;
    }
    String::from_utf8(w.finish().ok()?).ok()
}

// ---------------------------------------------------- graph store (GSP)

/// GSP target: the default graph or a named graph (concise IRI bytes).
#[derive(Clone, Debug)]
pub enum GraphTarget {
    Default,
    Named(Vec<u8>),
}

/// Full GSP addressing: a single graph, or — with neither `?graph=` nor `?default` — the
/// dataset itself (the Fuseki-style "upload quads to the dataset endpoint" dialect that
/// clients like Jena's `RDFConnection.putDataset` rely on).
#[derive(Clone, Debug)]
pub enum GspTarget {
    Graph(GraphTarget),
    Dataset,
}

/// Resolves the GSP target from the request's query string.
pub fn gsp_target(qs: Option<&str>) -> Result<GspTarget, ProtocolError> {
    let mut graph = None;
    let mut default = false;
    for pair in qs.unwrap_or("").split('&') {
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        match percent_decode(k).as_str() {
            "graph" => graph = Some(percent_decode(v)),
            "default" => default = true,
            _ => {}
        }
    }
    match (graph, default) {
        (Some(iri), false) => Ok(GspTarget::Graph(GraphTarget::Named(
            format!(">{iri}").into_bytes(),
        ))),
        (None, true) => Ok(GspTarget::Graph(GraphTarget::Default)),
        (None, false) => Ok(GspTarget::Dataset),
        (Some(_), true) => Err(ProtocolError::new(
            400,
            "specify at most one of ?graph=<iri> or ?default",
        )),
    }
}

/// The graph's column in a snapshot (`None` = absent named graph).
pub fn target_col(snap: &graphy_store::Snapshot, t: &GraphTarget) -> Option<u64> {
    match t {
        GraphTarget::Default => Some(0),
        GraphTarget::Named(bytes) => snap
            .resolve(bytes, graphy_store::TermPos::Graph)
            .and_then(|id| snap.column(id, graphy_store::TermPos::Graph))
            .filter(|&c| c > 0),
    }
}

pub type OwnedTriples = Vec<(Vec<u8>, Vec<u8>, Vec<u8>)>;
pub type OwnedQuads = Vec<(Vec<u8>, Vec<u8>, Vec<u8>, Option<Vec<u8>>)>;

/// Every triple in the target graph, decoded to concise bytes.
pub fn collect_graph(
    snap: &graphy_store::Snapshot,
    col: u64,
) -> Result<OwnedTriples, graphy_store::StoreError> {
    use graphy_store::{Pattern, QuadBatch, TermPos};
    let pat = Pattern {
        g: Some(col),
        ..Pattern::default()
    };
    let mut out = Vec::new();
    let mut scan = snap.scan_best(&pat)?;
    let mut batch = QuadBatch::new();
    while scan.next_batch(&mut batch)? {
        for i in 0..batch.len() {
            out.push((
                snap.decode_value(batch.s[i], TermPos::Subject)?,
                snap.decode_value(batch.p[i], TermPos::Predicate)?,
                snap.decode_value(batch.o[i], TermPos::Object)?,
            ));
        }
    }
    Ok(out)
}

/// Every quad in the dataset, decoded to concise bytes (`None` graph = default).
pub fn collect_all_quads(
    snap: &graphy_store::Snapshot,
) -> Result<OwnedQuads, graphy_store::StoreError> {
    use graphy_store::{Pattern, QuadBatch, TermPos};
    let mut out = Vec::new();
    let mut scan = snap.scan_best(&Pattern::default())?;
    let mut batch = QuadBatch::new();
    while scan.next_batch(&mut batch)? {
        for i in 0..batch.len() {
            let g = match batch.g[i] {
                0 => None,
                col => Some(snap.decode_value(col, TermPos::Graph)?),
            };
            out.push((
                snap.decode_value(batch.s[i], TermPos::Subject)?,
                snap.decode_value(batch.p[i], TermPos::Predicate)?,
                snap.decode_value(batch.o[i], TermPos::Object)?,
                g,
            ));
        }
    }
    Ok(out)
}

/// Compatibility shim for hosts that seeded the old clock/counter namespace.
/// Namespaces now come directly from secure randomness and require no restore hook.
pub fn decorrelate_label_ns(_floor: u32) {}

/// Fresh random per-request blank-label namespace so merges cannot capture
/// existing blanks, including after rapid native or wasm process restarts.
pub fn fresh_label_ns() -> u128 {
    let mut bytes = [0; 16];
    getrandom::fill(&mut bytes).expect("secure randomness is required for blank-node freshness");
    u128::from_le_bytes(bytes)
}

/// Parse a Turtle / N-Triples request body into concise triples (blank
/// labels namespaced per request so merges cannot capture existing
/// blanks).
pub fn parse_graph_body(
    ct: &str,
    body: &[u8],
) -> Result<(OwnedTriples, Vec<(String, String)>), ProtocolError> {
    match ct {
        "text/turtle" | "application/n-triples" | "" => {}
        other => {
            return Err(ProtocolError::new(
                415,
                format!("unsupported graph content type `{other}`"),
            ))
        }
    }
    let opts = graphy_turtle::Options {
        label_ns: Some(fresh_label_ns()),
        ..graphy_turtle::Options::default()
    };
    let mut triples = Vec::new();
    let mut had_named = false;
    let mut sink = |q: graphy_turtle::QuadRef<'_>| {
        if q.g.is_some() {
            had_named = true;
        } else {
            triples.push((q.s.to_vec(), q.p.to_vec(), q.o.to_vec()));
        }
    };
    let mut parser = graphy_turtle::TurtleParser::new(opts)
        .map_err(|e| ProtocolError::new(500, e.to_string()))?;
    parser
        .read_from(body, &mut sink)
        .map_err(|e| ProtocolError::new(400, format!("parse error: {e}")))?;
    if had_named {
        return Err(ProtocolError::new(
            400,
            "graph payloads must not contain named graphs",
        ));
    }
    let prefixes = parser
        .prefixes()
        .map(|(p, ns)| (p.to_string(), ns.to_string()))
        .collect();
    Ok((triples, prefixes))
}

/// Parse a TriG / N-Quads (or triples-format, all-default-graph) request body into concise
/// quads.
pub fn parse_dataset_body(
    ct: &str,
    body: &[u8],
) -> Result<(OwnedQuads, Vec<(String, String)>), ProtocolError> {
    let opts = graphy_turtle::Options {
        label_ns: Some(fresh_label_ns()),
        ..graphy_turtle::Options::default()
    };
    let mut quads: OwnedQuads = Vec::new();
    let mut sink = |q: graphy_turtle::QuadRef<'_>| {
        quads.push((
            q.s.to_vec(),
            q.p.to_vec(),
            q.o.to_vec(),
            q.g.map(|g| g.to_vec()),
        ));
    };
    let parse_err = |e: graphy_turtle::Error| ProtocolError::new(400, format!("parse error: {e}"));
    let mut prefixes = Vec::new();
    match ct {
        "application/trig" | "text/turtle" | "" => {
            let mut parser = graphy_turtle::TriGParser::new(opts)
                .map_err(|e| ProtocolError::new(500, e.to_string()))?;
            parser.read_from(body, &mut sink).map_err(parse_err)?;
            prefixes = parser
                .prefixes()
                .map(|(p, ns)| (p.to_string(), ns.to_string()))
                .collect();
        }
        "application/n-quads" | "application/n-triples" => {
            let mut parser = graphy_turtle::NQuadsParser::new(opts)
                .map_err(|e| ProtocolError::new(500, e.to_string()))?;
            parser.read_from(body, &mut sink).map_err(parse_err)?;
        }
        other => {
            return Err(ProtocolError::new(
                415,
                format!("unsupported dataset content type `{other}`"),
            ))
        }
    }
    Ok((quads, prefixes))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn form_parsing_decodes_and_collects() {
        let mut p = Params::default();
        parse_form(
            "query=SELECT%20*%20WHERE%7B%7D&default-graph-uri=http%3A%2F%2Fx%2Fg1&named-graph-uri=http://x/g2&timeout=2.5",
            &mut p,
        );
        assert_eq!(p.query.as_deref(), Some("SELECT * WHERE{}"));
        assert_eq!(p.default_graph, vec!["http://x/g1"]);
        assert_eq!(p.named_graph, vec!["http://x/g2"]);
        assert_eq!(p.timeout, Some(2.5));
    }

    #[test]
    fn update_dataset_parameters_keep_default_and_named_distinct() {
        let mut p = Params::default();
        parse_form(
            "using-graph-uri=http%3A%2F%2Fx%2Fd&using-named-graph-uri=http%3A%2F%2Fx%2Fn",
            &mut p,
        );
        assert_eq!(p.using_graph, ["http://x/d"]);
        assert_eq!(p.using_named_graph, ["http://x/n"]);
    }

    #[test]
    fn negotiation_first_match_wins() {
        assert_eq!(
            negotiate(Some("text/csv, application/sparql-results+json")),
            (ResultsFormat::Csv, GraphFormat::Turtle)
        );
        assert_eq!(
            negotiate(Some("application/n-triples;q=0.9")),
            (ResultsFormat::Json, GraphFormat::NTriples)
        );
        assert_eq!(negotiate(None), (ResultsFormat::Json, GraphFormat::Turtle));
    }

    #[test]
    fn gsp_addressing() {
        assert!(matches!(
            gsp_target(Some("graph=http%3A%2F%2Fx%2Fg")),
            Ok(GspTarget::Graph(GraphTarget::Named(b))) if b == b">http://x/g"
        ));
        assert!(matches!(
            gsp_target(Some("default")),
            Ok(GspTarget::Graph(GraphTarget::Default))
        ));
        assert!(matches!(gsp_target(None), Ok(GspTarget::Dataset)));
        assert_eq!(gsp_target(Some("graph=x&default")).unwrap_err().status, 400);
    }

    #[test]
    fn graph_body_rejects_named_graphs_and_bad_types() {
        assert!(parse_graph_body("text/turtle", b"<urn:a> <urn:b> <urn:c> .").is_ok());
        assert_eq!(
            parse_graph_body("application/pdf", b"").unwrap_err().status,
            415
        );
        let trig = b"<urn:g> { <urn:a> <urn:b> <urn:c> . }";
        // Turtle parser rejects graph syntax outright
        assert!(parse_graph_body("text/turtle", trig).is_err());
    }

    #[test]
    fn graph_body_blank_labels_are_request_scoped() {
        let body = b"_:same <http://x/p> <http://x/o> .";
        let (first, _) = parse_graph_body("text/turtle", body).unwrap();
        let (second, _) = parse_graph_body("text/turtle", body).unwrap();
        assert_ne!(first[0].0, second[0].0);
        assert!(first[0].0.starts_with(b"_f"));
        assert!(second[0].0.starts_with(b"_f"));
    }

    #[test]
    fn prefix_registry_merges_with_query_prologue_winning() {
        let registry = PrefixRegistry::default();
        registry.register(&[("ex".into(), "http://x/".into())]);
        let merged = registry.merged(&[("ex".into(), "http://y/".into())]);
        assert_eq!(merged, vec![("ex".to_string(), "http://y/".to_string())]);
    }

    /// Timing comparison of the flat and pretty turtle paths; run with
    /// `cargo test -p graphy-protocol --release -- --ignored --nocapture`.
    #[test]
    #[ignore = "timing run, release mode"]
    fn bench_turtle_flat_vs_pretty() {
        // Layer1-shaped prefix map: ~20 declarations, few of them used.
        let prefixes: Vec<(String, String)> = [
            ("rdf", "http://www.w3.org/1999/02/22-rdf-syntax-ns#"),
            ("rdfs", "http://www.w3.org/2000/01/rdf-schema#"),
            ("owl", "http://www.w3.org/2002/07/owl#"),
            ("xsd", "http://www.w3.org/2001/XMLSchema#"),
            ("dct", "http://purl.org/dc/terms/"),
            ("mms", "https://mms.openmbee.org/rdf/ontology/"),
            ("mms-txn", "https://mms.openmbee.org/rdf/ontology/txn."),
            ("mms-object", "https://mms.openmbee.org/rdf/objects/"),
            ("mms-datatype", "https://mms.openmbee.org/rdf/datatypes/"),
            ("m", "http://layer1-service/"),
            ("m-object", "http://layer1-service/objects/"),
            ("m-graph", "http://layer1-service/graphs/"),
            ("m-org", "http://layer1-service/orgs/"),
            ("m-user", "http://layer1-service/users/"),
            ("m-group", "http://layer1-service/groups/"),
            ("m-policy", "http://layer1-service/policies/"),
            ("ma", "http://layer1-service/graphs/AccessControl."),
            ("mu", "http://layer1-service/users/root"),
            ("mt", "http://layer1-service/transactions/be06f163"),
            ("foaf", "http://xmlns.com/foaf/0.1/"),
        ]
        .into_iter()
        .map(|(a, b)| (a.to_string(), b.to_string()))
        .collect();
        let iri = |i: String| format!(">{i}").into_bytes();
        for &n in &[1_000usize, 100_000, 1_000_000] {
            // 10 properties per subject, emitted in shuffled subject order.
            let triples: Vec<(Vec<u8>, Vec<u8>, Vec<u8>)> = (0..n)
                .map(|i| {
                    let k = (i.wrapping_mul(2_654_435_761)) % n; // interleave
                    (
                        iri(format!("http://layer1-service/objects/e{}", k / 10)),
                        iri(format!("https://mms.openmbee.org/rdf/ontology/p{}", k % 10)),
                        if k % 3 == 0 {
                            format!("\"value {k}").into_bytes()
                        } else {
                            iri(format!("http://layer1-service/objects/o{k}"))
                        },
                    )
                })
                .collect();
            let r = Results::Triples(triples);
            // Warm both paths once, then measure (pretty first so allocator
            // warmup can't flatter it).
            let _ = turtle_flat(&prefixes, &r);
            let _ = turtle_with_prologue(&prefixes, &r);
            let t0 = std::time::Instant::now();
            let pretty = turtle_with_prologue(&prefixes, &r);
            let t_pretty = t0.elapsed();
            let t0 = std::time::Instant::now();
            let flat = turtle_flat(&prefixes, &r);
            let t_flat = t0.elapsed();
            println!(
                "n={n}: flat {t_flat:?} ({} B) | pretty {t_pretty:?} ({} B) | ratio {:.1}x",
                flat.len(),
                pretty.len(),
                t_pretty.as_secs_f64() / t_flat.as_secs_f64().max(1e-9),
            );
        }
    }

    #[test]
    fn turtle_response_compacts_with_used_prefixes_only() {
        let iri = |i: &str| format!(">{i}").into_bytes();
        // Deliberately unsorted: same subject interleaved, blank shared twice.
        let r = Results::Triples(vec![
            (iri("http://x/s"), iri("http://x/p"), b"\"v".to_vec()),
            (iri("http://x/t"), iri("http://x/q"), b"_b0".to_vec()),
            (
                iri("http://x/s"),
                iri("http://www.w3.org/1999/02/22-rdf-syntax-ns#type"),
                iri("http://x/C"),
            ),
            (b"_b0".to_vec(), iri("http://x/p"), b"\"w".to_vec()),
            (iri("http://x/s"), iri("http://x/q"), b"_b0".to_vec()),
        ]);
        let prefixes = vec![
            ("ex".to_string(), "http://x/".to_string()),
            ("unused".to_string(), "http://elsewhere/".to_string()),
        ];
        let text = turtle_with_prologue(&prefixes, &r);
        assert_eq!(
            text,
            concat!(
                "@prefix ex: <http://x/> .\n",
                "\n",
                "ex:s a ex:C ;\n",
                "\tex:p \"v\" ;\n",
                "\tex:q _:b0 .\n",
                "\n",
                "ex:t ex:q _:b0 .\n",
                "\n",
                "_:b0 ex:p \"w\" .\n",
            )
        );
        // Reparses to the same graph (every triple survives, labels intact).
        let mut p = graphy_turtle::TurtleParser::new(graphy_turtle::Options::default()).unwrap();
        let mut reparsed: Vec<(Vec<u8>, Vec<u8>, Vec<u8>)> = Vec::new();
        p.read_from(text.as_bytes(), |q| {
            reparsed.push((q.s.to_vec(), q.p.to_vec(), q.o.to_vec()));
        })
        .unwrap();
        // The parser maps surface labels to first-seen `s{n}` ordinals (they
        // never re-enter the minted `b{n}` namespace), so `_:b0` comes back
        // as `_s0` — same graph, relabeled.
        let Results::Triples(orig) = &r else {
            unreachable!()
        };
        let relabel = |b: Vec<u8>| if b == b"_b0" { b"_s0".to_vec() } else { b };
        let mut expect: Vec<_> = orig
            .iter()
            .map(|(s, p, o)| (relabel(s.clone()), p.clone(), relabel(o.clone())))
            .collect();
        expect.sort_unstable();
        reparsed.sort_unstable();
        assert_eq!(reparsed, expect);
    }
}
