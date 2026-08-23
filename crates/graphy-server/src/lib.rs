//! `graphy-server` — the SPARQL 1.1 Protocol HTTP service (doc 06).
//! axum/tokio stay confined to this crate; the protocol logic itself
//! (parameter parsing, conneg, dataset override, ETag scheme, GSP
//! addressing and body handling, results serialization) lives in the
//! transport-free `graphy-protocol` crate. CPU-bound engine work runs
//! on the blocking pool with a snapshot pinned per request (consistent
//! reads + `ETag: "G<gen>.E<epoch>"`). M9 inc.1 surface: protocol
//! query (GET, POST urlencoded, POST direct) and update (POST), the
//! four results formats + N-Triples/Turtle graph responses, protocol
//! dataset parameters, per-request deadlines, /health, and a minimal
//! service description.

pub use graphy_protocol::results;

use std::sync::Arc;
use std::time::Duration;
#[cfg(not(target_arch = "wasm32"))]
use std::time::Instant;

use axum::body::Bytes;
use axum::extract::{RawQuery, State};
use axum::http::{header, HeaderMap, Method, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use graphy_engine::exec::ExecOptions;
use graphy_engine::Output;
use graphy_protocol::{
    apply_dataset_params, collect_all_quads, collect_graph, content_type_essence, engine_status,
    gsp_target, negotiate, parse_dataset_body, parse_form, parse_graph_body, snapshot_etag,
    target_col, turtle_with_prologue, GraphFormat, GraphTarget, GspTarget, OwnedQuads,
    OwnedTriples, Params, PrefixRegistry, ProtocolError, ResultsFormat, SERVICE_DESCRIPTION,
};
use graphy_store::Store;

use results::Results;

/// Server configuration.
#[derive(Clone, Debug)]
pub struct Config {
    /// Reject updates (403).
    pub read_only: bool,
    /// Permit compiled outbound-network capabilities. Defaults to false and
    /// is set by the native CLI's explicit `--allow-network` flag.
    pub allow_network: bool,
    /// Default per-request deadline.
    pub default_timeout: Duration,
    /// Upper bound for the `timeout=` parameter.
    pub max_timeout: Duration,
}

impl Default for Config {
    fn default() -> Config {
        Config {
            read_only: false,
            allow_network: false,
            default_timeout: Duration::from_secs(60),
            max_timeout: Duration::from_secs(300),
        }
    }
}

/// Whether this target was compiled with an outbound-network implementation.
///
/// This is always false on WASM, even if a dependent crate accidentally
/// forwards the feature.
pub const fn outbound_network_compiled() -> bool {
    cfg!(all(not(target_arch = "wasm32"), feature = "outbound-http"))
}

struct AppState {
    store: Arc<Store>,
    cfg: Config,
    prefixes: PrefixRegistry,
}

/// Build the axum router for a store.
pub fn router(store: Arc<Store>, cfg: Config) -> Router {
    let state = Arc::new(AppState {
        store,
        cfg,
        prefixes: PrefixRegistry::default(),
    });
    Router::new()
        .route("/sparql", get(sparql_get).post(sparql_post))
        .route("/sparql/service", get(service_description))
        .route(
            "/graphs",
            get(gsp_get).put(gsp_put).post(gsp_post).delete(gsp_delete),
        )
        .route("/health", get(|| async { "ok\n" }))
        .with_state(state)
}

/// Runs CPU-bound store work off the async reactor. The wasm host is
/// single-threaded with no reactor to starve, so it runs the closure inline.
#[cfg(not(target_arch = "wasm32"))]
async fn run_blocking<T, F>(f: F) -> Result<T, String>
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
{
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| e.to_string())
}

#[cfg(target_arch = "wasm32")]
async fn run_blocking<T, F>(f: F) -> Result<T, String>
where
    F: FnOnce() -> T,
{
    Ok(f())
}

/// Serve until ctrl-c (the CLI entry; spins its own runtime so callers
/// stay sync).
#[cfg(not(target_arch = "wasm32"))]
pub fn serve_blocking(addr: &str, store: Store, cfg: Config) -> Result<(), String> {
    if cfg.allow_network && !outbound_network_compiled() {
        return Err(
            "--allow-network requires a binary built with the `outbound-http` feature".into(),
        );
    }
    let rt = tokio::runtime::Runtime::new().map_err(|e| e.to_string())?;
    rt.block_on(async {
        let app = router(Arc::new(store), cfg);
        let listener = tokio::net::TcpListener::bind(addr)
            .await
            .map_err(|e| format!("bind {addr}: {e}"))?;
        eprintln!("graphy-server listening on http://{addr}/sparql");
        axum::serve(listener, app)
            .with_graceful_shutdown(async {
                let _ = tokio::signal::ctrl_c().await;
            })
            .await
            .map_err(|e| e.to_string())
    })
}

// ------------------------------------------------------------- transport

fn error(status: StatusCode, msg: impl std::fmt::Display) -> Response {
    (status, format!("{msg}\n")).into_response()
}

fn protocol_error(e: ProtocolError) -> Response {
    error(
        StatusCode::from_u16(e.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
        e.message,
    )
}

fn status(code: u16) -> StatusCode {
    StatusCode::from_u16(code).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR)
}

fn accept_of(headers: &HeaderMap) -> Option<String> {
    headers
        .get(header::ACCEPT)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned)
}

fn content_type_of(headers: &HeaderMap) -> String {
    content_type_essence(
        headers
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or(""),
    )
}

async fn sparql_get(
    State(state): State<Arc<AppState>>,
    RawQuery(qs): RawQuery,
    headers: HeaderMap,
) -> Response {
    let mut p = Params::default();
    parse_form(qs.as_deref().unwrap_or(""), &mut p);
    if p.update.is_some() {
        return error(StatusCode::BAD_REQUEST, "update is POST-only");
    }
    match p.query.take() {
        Some(q) => run_query(state, q, p, headers).await,
        None => service_description_response(),
    }
}

async fn sparql_post(
    State(state): State<Arc<AppState>>,
    RawQuery(qs): RawQuery,
    method_headers: (Method, HeaderMap),
    body: Bytes,
) -> Response {
    let (_, headers) = method_headers;
    let ct = content_type_of(&headers);
    let mut p = Params::default();
    // Fuseki-style leniency: protocol parameters may also arrive in the URL query
    // string of a POST (some clients send `?update=...` with an empty body); direct
    // body content and body form parameters take precedence below.
    parse_form(qs.as_deref().unwrap_or(""), &mut p);
    match ct.as_str() {
        "application/sparql-query" => {
            p.query = Some(String::from_utf8_lossy(&body).into_owned());
        }
        "application/sparql-update" => {
            p.update = Some(String::from_utf8_lossy(&body).into_owned());
        }
        "application/x-www-form-urlencoded" | "" => {
            parse_form(&String::from_utf8_lossy(&body), &mut p);
        }
        other => {
            return error(
                StatusCode::UNSUPPORTED_MEDIA_TYPE,
                format!("unsupported content type `{other}`"),
            )
        }
    }
    match (p.query.take(), p.update.take()) {
        (Some(q), None) => run_query(state, q, p, headers).await,
        (None, Some(u)) => run_update(state, u, p).await,
        (Some(_), Some(_)) => error(StatusCode::BAD_REQUEST, "both query and update given"),
        (None, None) => error(StatusCode::BAD_REQUEST, "missing query or update"),
    }
}

async fn run_query(state: Arc<AppState>, text: String, p: Params, headers: HeaderMap) -> Response {
    let (rf, gf) = negotiate(accept_of(&headers).as_deref());
    let timeout = p
        .timeout
        .map(Duration::from_secs_f64)
        .unwrap_or(state.cfg.default_timeout)
        .min(state.cfg.max_timeout);
    let if_none_match = headers
        .get(header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);

    let store = state.store.clone();
    let out = run_blocking(move || -> Result<_, (u16, String)> {
        let parsed = graphy_sparql_syntax::parse_query(&text)
            .map_err(|e| (400, format!("parse error: {e}")))?;
        let prefixes = parsed.prefixes.clone();
        let mut tq = graphy_algebra::translate_query(&parsed)
            .map_err(|e| (400, format!("translate error: {e}")))?;
        tq.root = graphy_algebra::rewrite(tq.root.clone());
        apply_dataset_params(&mut tq, &p.default_graph, &p.named_graph);
        let snap = store.snapshot();
        let etag = snapshot_etag(&snap);
        if if_none_match.as_deref() == Some(etag.as_str()) {
            return Ok((None, etag, Vec::new()));
        }
        let opts = ExecOptions {
            deadline: query_deadline(timeout),
            ..ExecOptions::default()
        };
        let out = graphy_engine::exec::evaluate_with(&snap, &tq, &opts)
            .map_err(|e| (engine_status(&e.to_string()), e.to_string()))?;
        Ok((Some(out), etag, prefixes))
    })
    .await;

    let (out, etag, prefixes) = match out {
        Ok(Ok(pair)) => pair,
        Ok(Err((code, msg))) => return error(status(code), msg),
        Err(e) => return error(StatusCode::INTERNAL_SERVER_ERROR, e),
    };
    let Some(out) = out else {
        return Response::builder()
            .status(StatusCode::NOT_MODIFIED)
            .header(header::ETAG, etag)
            .body(axum::body::Body::empty())
            .unwrap()
            .into_response();
    };

    let r = match out {
        Output::Solutions { vars, rows } => Results::Solutions { vars, rows },
        Output::Boolean(b) => Results::Boolean(b),
        Output::Triples(t) => Results::Triples(t),
    };
    let (body, content_type) = match &r {
        Results::Triples(_) => match gf {
            GraphFormat::NTriples => (results::to_ntriples(&r), "application/n-triples"),
            // echo the query prologue's prefix declarations (clients and test
            // harnesses expect the Fuseki-style self-describing response)
            GraphFormat::Turtle => (
                turtle_with_prologue(&state.prefixes.merged(&prefixes), &r),
                "text/turtle",
            ),
        },
        _ => match rf {
            ResultsFormat::Json => (results::to_json(&r), "application/sparql-results+json"),
            ResultsFormat::Xml => (results::to_xml(&r), "application/sparql-results+xml"),
            ResultsFormat::Csv => (results::to_csv(&r), "text/csv; charset=utf-8"),
            ResultsFormat::Tsv => (
                results::to_tsv(&r),
                "text/tab-separated-values; charset=utf-8",
            ),
        },
    };
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, content_type)
        .header(header::ETAG, etag)
        .body(axum::body::Body::from(body))
        .unwrap()
        .into_response()
}

async fn run_update(state: Arc<AppState>, text: String, p: Params) -> Response {
    if state.cfg.read_only {
        return error(StatusCode::FORBIDDEN, "store is read-only");
    }
    let store = state.store.clone();
    let out = run_blocking(move || -> Result<(), (u16, String)> {
        let parsed = graphy_sparql_syntax::parse_update(&text)
            .map_err(|e| (400, format!("parse error: {e}")))?;
        let mut tu = graphy_algebra::translate_update(&parsed)
            .map_err(|e| (400, format!("translate error: {e}")))?;
        // A large INSERT DATA's parse AST rivals the data itself; release
        // it (and the request text) before execution — wasm32 peak-memory
        // discipline for the StoreServer dispatch path (docs/11).
        drop(parsed);
        drop(text);
        graphy_protocol::apply_using_params(&mut tu, &p.using_graph, &p.using_named_graph);
        if state.cfg.allow_network {
            #[cfg(all(not(target_arch = "wasm32"), feature = "outbound-http"))]
            graphy_engine::execute_update_with_loader(&store, &tu, &mut load_rdf_document)
                .map_err(|e| (500, e.to_string()))?;
            #[cfg(not(all(not(target_arch = "wasm32"), feature = "outbound-http")))]
            return Err((
                501,
                "outbound network support was not compiled into this target".into(),
            ));
        } else {
            let mut denied = false;
            let result = graphy_engine::execute_update_with_loader(&store, &tu, &mut |_| {
                denied = true;
                Err(graphy_engine::EngineError(
                    "outbound network access is disabled; build with `outbound-http` and launch with `--allow-network`".into(),
                ))
            });
            if let Err(e) = result {
                return Err((if denied { 403 } else { 500 }, e.to_string()));
            }
        }
        Ok(())
    })
    .await;
    match out {
        Ok(Ok(())) => (StatusCode::NO_CONTENT, "").into_response(),
        Ok(Err((code, msg))) => error(status(code), msg),
        Err(e) => error(StatusCode::INTERNAL_SERVER_ERROR, e),
    }
}

#[cfg(all(not(target_arch = "wasm32"), feature = "outbound-http"))]
fn load_rdf_document(
    source: &[u8],
) -> Result<Vec<graphy_engine::LoadedTriple>, graphy_engine::EngineError> {
    use graphy_core::TermRef;
    use graphy_turtle::{NTriplesParser, Options, TurtleParser};

    let TermRef::Iri(iri) = graphy_core::concise::decode(source)
        .map_err(|e| graphy_engine::EngineError(format!("invalid LOAD source: {e}")))?
    else {
        return Err(graphy_engine::EngineError(
            "LOAD source is not an IRI".into(),
        ));
    };
    if !(iri.starts_with("http://") || iri.starts_with("https://")) {
        return Err(graphy_engine::EngineError(
            "the HTTP server permits LOAD only from http(s) IRIs".into(),
        ));
    }
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(60))
        .build()
        .map_err(|e| graphy_engine::EngineError(format!("LOAD client: {e}")))?;
    let response = client
        .get(iri)
        .header(
            reqwest::header::ACCEPT,
            "text/turtle, application/n-triples, application/rdf+xml, application/ld+json",
        )
        .send()
        .and_then(reqwest::blocking::Response::error_for_status)
        .map_err(|e| graphy_engine::EngineError(format!("LOAD {iri}: {e}")))?;
    const MAX_LOAD_BYTES: u64 = 64 * 1024 * 1024;
    if response
        .content_length()
        .is_some_and(|n| n > MAX_LOAD_BYTES)
    {
        return Err(graphy_engine::EngineError(
            "LOAD document exceeds the 64 MiB limit".into(),
        ));
    }
    let content_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(content_type_essence)
        .unwrap_or_default();
    let body = response
        .bytes()
        .map_err(|e| graphy_engine::EngineError(format!("LOAD {iri}: {e}")))?;
    if body.len() as u64 > MAX_LOAD_BYTES {
        return Err(graphy_engine::EngineError(
            "LOAD document exceeds the 64 MiB limit".into(),
        ));
    }

    if content_type == "application/rdf+xml" || content_type.is_empty() && iri.ends_with(".rdf") {
        let text = std::str::from_utf8(&body)
            .map_err(|_| graphy_engine::EngineError("RDF/XML document is not UTF-8".into()))?;
        return graphy_interop::parse_rdfxml(text, Some(iri))
            .map(|ts| ts.into_iter().map(|t| (t.s, t.p, t.o)).collect())
            .map_err(|e| graphy_engine::EngineError(format!("LOAD RDF/XML: {e}")));
    }
    if content_type == "application/ld+json"
        || content_type.is_empty() && (iri.ends_with(".jsonld") || iri.ends_with(".json"))
    {
        let text = std::str::from_utf8(&body)
            .map_err(|_| graphy_engine::EngineError("JSON-LD document is not UTF-8".into()))?;
        return graphy_interop::parse_jsonld(text, Some(iri))
            .map(|ts| ts.into_iter().map(|t| (t.s, t.p, t.o)).collect())
            .map_err(|e| graphy_engine::EngineError(format!("LOAD JSON-LD: {e}")));
    }

    let opts = Options {
        base: Some(iri.to_owned()),
        label_ns: Some(graphy_protocol::fresh_label_ns()),
        ..Options::default()
    };
    let mut triples = Vec::new();
    let mut sink = |q: graphy_turtle::QuadRef<'_>| {
        triples.push((q.s.to_vec(), q.p.to_vec(), q.o.to_vec()));
    };
    if content_type == "application/n-triples" || content_type.is_empty() && iri.ends_with(".nt") {
        NTriplesParser::new(opts)
            .map_err(|e| graphy_engine::EngineError(e.to_string()))?
            .read_from(&body[..], &mut sink)
            .map_err(|e| graphy_engine::EngineError(format!("LOAD N-Triples: {e}")))?;
    } else if matches!(
        content_type.as_str(),
        "" | "text/turtle" | "application/x-turtle"
    ) {
        TurtleParser::new(opts)
            .map_err(|e| graphy_engine::EngineError(e.to_string()))?
            .read_from(&body[..], &mut sink)
            .map_err(|e| graphy_engine::EngineError(format!("LOAD Turtle: {e}")))?;
    } else {
        return Err(graphy_engine::EngineError(format!(
            "unsupported LOAD content type `{content_type}`"
        )));
    }
    Ok(triples)
}

fn service_description_response() -> Response {
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/turtle")
        .body(axum::body::Body::from(SERVICE_DESCRIPTION))
        .unwrap()
        .into_response()
}

async fn service_description(State(_): State<Arc<AppState>>) -> Response {
    service_description_response()
}

/// The per-request evaluation deadline. Wasm has no monotonic clock (`Instant::now`
/// panics), and the single-threaded host cannot observe a deadline mid-query anyway.
#[cfg(not(target_arch = "wasm32"))]
fn query_deadline(timeout: Duration) -> Option<Instant> {
    Some(Instant::now() + timeout)
}

#[cfg(target_arch = "wasm32")]
fn query_deadline(_timeout: Duration) -> Option<std::time::Instant> {
    None
}

// ---------------------------------------------------- graph store (GSP)

/// Dataset-level PUT (replace) / POST (merge) / DELETE (clear): one atomic `Store::apply`.
async fn dataset_write(
    state: Arc<AppState>,
    replace: bool,
    delete: bool,
    ct: String,
    body: Bytes,
) -> Response {
    let adds = if delete {
        Vec::new()
    } else {
        match parse_dataset_body(&ct, &body) {
            Ok((quads, prefixes)) => {
                state.prefixes.register(&prefixes);
                quads
            }
            Err(e) => return protocol_error(e),
        }
    };
    let store = state.store.clone();
    let out = run_blocking(move || -> Result<StatusCode, String> {
        let snap = store.snapshot();
        let dels: OwnedQuads = if replace || delete {
            collect_all_quads(&snap).map_err(|e| e.to_string())?
        } else {
            Vec::new()
        };
        let del_refs: Vec<graphy_store::QuadTerms<'_>> = dels
            .iter()
            .map(|(s, p, o, g)| (s.as_slice(), p.as_slice(), o.as_slice(), g.as_deref()))
            .collect();
        let add_refs: Vec<graphy_store::QuadTerms<'_>> = adds
            .iter()
            .map(|(s, p, o, g)| (s.as_slice(), p.as_slice(), o.as_slice(), g.as_deref()))
            .collect();
        store
            .apply(&del_refs, &add_refs)
            .map_err(|e| e.to_string())?;
        Ok(StatusCode::NO_CONTENT)
    })
    .await;
    match out {
        Ok(Ok(status)) => (status, "").into_response(),
        Ok(Err(e)) => error(StatusCode::INTERNAL_SERVER_ERROR, e),
        Err(e) => error(StatusCode::INTERNAL_SERVER_ERROR, e),
    }
}

async fn gsp_get(
    State(state): State<Arc<AppState>>,
    RawQuery(qs): RawQuery,
    headers: HeaderMap,
) -> Response {
    let target = match gsp_target(qs.as_deref()) {
        Ok(GspTarget::Graph(t)) => t,
        Ok(GspTarget::Dataset) => {
            return error(
                StatusCode::NOT_IMPLEMENTED,
                "dataset retrieval is not implemented; target ?graph=<iri> or ?default",
            )
        }
        Err(e) => return protocol_error(e),
    };
    let (_, gf) = negotiate(accept_of(&headers).as_deref());
    let store = state.store.clone();
    let out = run_blocking(move || -> Result<Option<OwnedTriples>, String> {
        let snap = store.snapshot();
        let col = target_col(&snap, &target);
        match (col, &target) {
            (None, GraphTarget::Named(_)) => Ok(None),
            (Some(col), _) => {
                let triples = collect_graph(&snap, col).map_err(|e| e.to_string())?;
                match (&target, triples.is_empty()) {
                    // A named graph exists iff nonempty (update.rs note).
                    (GraphTarget::Named(_), true) => Ok(None),
                    _ => Ok(Some(triples)),
                }
            }
            (None, GraphTarget::Default) => unreachable!(),
        }
    })
    .await;
    match out {
        Ok(Ok(Some(triples))) => {
            let (body, ct) = match gf {
                GraphFormat::NTriples => (
                    results::to_ntriples(&Results::Triples(triples)),
                    "application/n-triples",
                ),
                GraphFormat::Turtle => (
                    turtle_with_prologue(&state.prefixes.merged(&[]), &Results::Triples(triples)),
                    "text/turtle",
                ),
            };
            ([(header::CONTENT_TYPE, ct)], body).into_response()
        }
        Ok(Ok(None)) => error(StatusCode::NOT_FOUND, "no such graph"),
        Ok(Err(e)) => error(StatusCode::INTERNAL_SERVER_ERROR, e),
        Err(e) => error(StatusCode::INTERNAL_SERVER_ERROR, e),
    }
}

/// Shared PUT/POST/DELETE write path: one atomic `Store::apply`.
async fn gsp_write(
    state: Arc<AppState>,
    qs: Option<String>,
    replace: bool,
    delete: bool,
    ct: String,
    body: Bytes,
) -> Response {
    if state.cfg.read_only {
        return error(StatusCode::FORBIDDEN, "store is read-only");
    }
    let target = match gsp_target(qs.as_deref()) {
        Ok(GspTarget::Graph(t)) => t,
        // no graph addressed: the operation applies to the dataset itself
        Ok(GspTarget::Dataset) => return dataset_write(state, replace, delete, ct, body).await,
        Err(e) => return protocol_error(e),
    };
    let adds = if delete {
        Vec::new()
    } else {
        match parse_graph_body(&ct, &body) {
            Ok((triples, prefixes)) => {
                state.prefixes.register(&prefixes);
                triples
            }
            Err(e) => return protocol_error(e),
        }
    };
    let store = state.store.clone();
    let out = run_blocking(move || -> Result<StatusCode, String> {
        let snap = store.snapshot();
        let col = target_col(&snap, &target);
        let dels: OwnedTriples = if replace || delete {
            match col {
                Some(col) => collect_graph(&snap, col).map_err(|e| e.to_string())?,
                None => Vec::new(),
            }
        } else {
            Vec::new()
        };
        if delete && dels.is_empty() {
            if matches!(target, GraphTarget::Named(_)) {
                return Ok(StatusCode::NOT_FOUND);
            }
            return Ok(StatusCode::NO_CONTENT);
        }
        let gbytes = match &target {
            GraphTarget::Default => None,
            GraphTarget::Named(b) => Some(b.as_slice()),
        };
        let created = matches!(target, GraphTarget::Named(_)) && col.is_none();
        let del_refs: Vec<graphy_store::QuadTerms<'_>> = dels
            .iter()
            .map(|(s, p, o)| (s.as_slice(), p.as_slice(), o.as_slice(), gbytes))
            .collect();
        let add_refs: Vec<graphy_store::QuadTerms<'_>> = adds
            .iter()
            .map(|(s, p, o)| (s.as_slice(), p.as_slice(), o.as_slice(), gbytes))
            .collect();
        store
            .apply(&del_refs, &add_refs)
            .map_err(|e| e.to_string())?;
        Ok(if created {
            StatusCode::CREATED
        } else {
            StatusCode::NO_CONTENT
        })
    })
    .await;
    match out {
        Ok(Ok(status)) => (status, "").into_response(),
        Ok(Err(e)) => error(StatusCode::INTERNAL_SERVER_ERROR, e),
        Err(e) => error(StatusCode::INTERNAL_SERVER_ERROR, e),
    }
}

async fn gsp_put(
    State(state): State<Arc<AppState>>,
    RawQuery(qs): RawQuery,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    gsp_write(state, qs, true, false, content_type_of(&headers), body).await
}

async fn gsp_post(
    State(state): State<Arc<AppState>>,
    RawQuery(qs): RawQuery,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    gsp_write(state, qs, false, false, content_type_of(&headers), body).await
}

async fn gsp_delete(State(state): State<Arc<AppState>>, RawQuery(qs): RawQuery) -> Response {
    gsp_write(state, qs, false, true, String::new(), Bytes::new()).await
}
