//! SPARQL Protocol integration tests: a real server on an ephemeral
//! port, exercised over HTTP with blocking reqwest (protocol encodings,
//! conneg, dataset parameters, updates, ETag caching, error mapping).

use std::sync::Arc;
use std::time::Duration;

use graphy_server::{router, Config};
use graphy_store::{BuilderConfig, Profile, SegmentBuilder, Store};

fn build_store(dir: &std::path::Path) -> Store {
    let iri = |s: &str| format!(">http://x/{s}").into_bytes();
    let int = |i: i64| format!("^>http://www.w3.org/2001/XMLSchema#integer\"{i}").into_bytes();
    let mut cfg = BuilderConfig::new(dir);
    cfg.profile = Profile::Balanced;
    cfg.sort_budget = 1 << 14;
    let mut b = SegmentBuilder::new(cfg).unwrap();
    b.push_quad(&iri("alice"), &iri("knows"), &iri("bob"), None)
        .unwrap();
    b.push_quad(&iri("alice"), &iri("age"), &int(30), None)
        .unwrap();
    b.push_quad(&iri("bob"), &iri("age"), &int(25), None)
        .unwrap();
    b.push_quad(&iri("dave"), &iri("knows"), &iri("alice"), Some(&iri("g1")))
        .unwrap();
    b.finish().unwrap();
    Store::open(dir).unwrap()
}

/// Spawn the server on an ephemeral port; returns its base URL.
fn spawn_server(read_only: bool, allow_network: bool) -> String {
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let dir = std::env::temp_dir().join(format!(
        "graphy-server-test-{}-{}",
        std::process::id(),
        SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
    ));
    let _ = std::fs::remove_dir_all(&dir);
    let store = build_store(&dir);
    let cfg = Config {
        read_only,
        allow_network,
        ..Config::default()
    };
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async move {
            let app = router(Arc::new(store), cfg);
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            tx.send(listener.local_addr().unwrap()).unwrap();
            axum::serve(listener, app).await.unwrap();
        });
    });
    let addr = rx.recv_timeout(Duration::from_secs(10)).unwrap();
    format!("http://{addr}")
}

fn client() -> reqwest::blocking::Client {
    reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .unwrap()
}

#[test]
fn protocol_end_to_end() {
    let base = spawn_server(false, false);
    let c = client();

    // -- GET with encoded query, default JSON results.
    let r = c
        .get(format!(
            "{base}/sparql?query=SELECT%20%3Fo%20WHERE%20%7B%20%3Chttp%3A%2F%2Fx%2Falice%3E%20%3Chttp%3A%2F%2Fx%2Fknows%3E%20%3Fo%20%7D"
        ))
        .send()
        .unwrap();
    assert_eq!(r.status(), 200);
    assert!(r
        .headers()
        .get("content-type")
        .unwrap()
        .to_str()
        .unwrap()
        .starts_with("application/sparql-results+json"));
    let etag = r
        .headers()
        .get("etag")
        .unwrap()
        .to_str()
        .unwrap()
        .to_owned();
    assert!(etag.starts_with("\"G"), "{etag}");
    let j: serde_json::Value = serde_json::from_str(&r.text().unwrap()).unwrap();
    assert_eq!(j["head"]["vars"], serde_json::json!(["o"]));
    assert_eq!(j["results"]["bindings"][0]["o"]["type"], "uri");
    assert_eq!(j["results"]["bindings"][0]["o"]["value"], "http://x/bob");

    // -- If-None-Match short-circuits on the unchanged store.
    let r = c
        .get(format!("{base}/sparql?query=ASK%20%7B%7D"))
        .header("if-none-match", etag.clone())
        .send()
        .unwrap();
    assert_eq!(r.status(), 304);

    // -- POST direct (application/sparql-query), XML conneg.
    let r = c
        .post(format!("{base}/sparql"))
        .header("content-type", "application/sparql-query")
        .header("accept", "application/sparql-results+xml")
        .body("ASK { <http://x/alice> <http://x/knows> <http://x/bob> }")
        .send()
        .unwrap();
    assert_eq!(r.status(), 200);
    let body = r.text().unwrap();
    assert!(body.contains("<boolean>true</boolean>"), "{body}");

    // -- POST urlencoded, CSV + TSV conneg.
    let q = "SELECT ?s ?a WHERE { ?s <http://x/age> ?a } ORDER BY ?a";
    let r = c
        .post(format!("{base}/sparql"))
        .header("content-type", "application/x-www-form-urlencoded")
        .header("accept", "text/csv")
        .body(format!("query={}", urlenc(q)))
        .send()
        .unwrap();
    let csv = r.text().unwrap();
    assert_eq!(csv, "s,a\r\nhttp://x/bob,25\r\nhttp://x/alice,30\r\n");
    let r = c
        .post(format!("{base}/sparql"))
        .header("content-type", "application/x-www-form-urlencoded")
        .header("accept", "text/tab-separated-values")
        .body(format!("query={}", urlenc(q)))
        .send()
        .unwrap();
    let tsv = r.text().unwrap();
    assert!(tsv.starts_with("?s\t?a\n"), "{tsv}");
    // Full typed-literal form (the spec permits either spelling).
    assert!(
        tsv.contains("<http://x/bob>\t\"25\"^^<http://www.w3.org/2001/XMLSchema#integer>"),
        "{tsv}"
    );

    // -- CONSTRUCT: Turtle (N-Triples subset) body.
    let r = c
        .post(format!("{base}/sparql"))
        .header("content-type", "application/sparql-query")
        .body("CONSTRUCT { ?s <http://x/knownBy> ?o } WHERE { ?o <http://x/knows> ?s }")
        .send()
        .unwrap();
    assert!(r
        .headers()
        .get("content-type")
        .unwrap()
        .to_str()
        .unwrap()
        .starts_with("text/turtle"));
    let body = r.text().unwrap();
    assert!(
        body.contains("<http://x/bob> <http://x/knownBy> <http://x/alice> ."),
        "{body}"
    );

    // -- Protocol dataset parameter: named graph g1 as the default graph.
    let r = c
        .get(format!(
            "{base}/sparql?query={}&default-graph-uri={}",
            urlenc("SELECT ?s WHERE { ?s <http://x/knows> ?o }"),
            urlenc("http://x/g1"),
        ))
        .send()
        .unwrap();
    let j: serde_json::Value = serde_json::from_str(&r.text().unwrap()).unwrap();
    let bindings = j["results"]["bindings"].as_array().unwrap();
    assert_eq!(bindings.len(), 1);
    assert_eq!(bindings[0]["s"]["value"], "http://x/dave");

    // -- Parse error → 400.
    let r = c.get(format!("{base}/sparql?query=SELEKT")).send().unwrap();
    assert_eq!(r.status(), 400);

    // -- Update: insert, then observe via query; epoch bumps the ETag.
    let r = c
        .post(format!("{base}/sparql"))
        .header("content-type", "application/sparql-update")
        .body("INSERT DATA { <http://x/carol> <http://x/age> 41 }")
        .send()
        .unwrap();
    assert_eq!(r.status(), 204);
    let r = c
        .get(format!(
            "{base}/sparql?query={}",
            urlenc("SELECT ?a WHERE { <http://x/carol> <http://x/age> ?a }")
        ))
        .send()
        .unwrap();
    let new_etag = r
        .headers()
        .get("etag")
        .unwrap()
        .to_str()
        .unwrap()
        .to_owned();
    assert_ne!(new_etag, etag, "epoch must advance the ETag");
    let j: serde_json::Value = serde_json::from_str(&r.text().unwrap()).unwrap();
    assert_eq!(j["results"]["bindings"][0]["a"]["value"], "41");
    assert_eq!(
        j["results"]["bindings"][0]["a"]["datatype"],
        "http://www.w3.org/2001/XMLSchema#integer"
    );

    // -- LOAD is deny-by-default even when its source is reachable.
    let source = urlenc("urn:load:source");
    let r = c
        .put(format!("{base}/graphs?graph={source}"))
        .header("content-type", "text/turtle")
        .body("<urn:load:s> <urn:load:p> <urn:load:o> .")
        .send()
        .unwrap();
    assert_eq!(r.status(), 201);
    let source_url = format!("{base}/graphs?graph={source}");
    let r = c
        .post(format!("{base}/sparql"))
        .header("content-type", "application/sparql-update")
        .body(format!("LOAD <{source_url}> INTO GRAPH <urn:load:target>"))
        .send()
        .unwrap();
    assert_eq!(r.status(), 403);
    assert!(r.text().unwrap().contains("network access is disabled"));
    let r = c
        .get(format!(
            "{base}/sparql?query={}",
            urlenc("ASK { GRAPH <urn:load:target> { <urn:load:s> <urn:load:p> <urn:load:o> } }")
        ))
        .send()
        .unwrap();
    let j: serde_json::Value = serde_json::from_str(&r.text().unwrap()).unwrap();
    assert_eq!(j["boolean"], serde_json::json!(false));

    // -- Service description at GET /sparql without a query.
    let r = c.get(format!("{base}/sparql")).send().unwrap();
    let body = r.text().unwrap();
    assert!(body.contains("sd:Service"), "{body}");
}

#[test]
fn read_only_rejects_updates() {
    let base = spawn_server(true, false);
    let c = client();
    let r = c
        .post(format!("{base}/sparql"))
        .header("content-type", "application/sparql-update")
        .body("INSERT DATA { <http://x/z> <http://x/p> 1 }")
        .send()
        .unwrap();
    assert_eq!(r.status(), 403);
}

#[cfg(feature = "outbound-http")]
#[test]
fn load_requires_compiled_and_runtime_network_opt_in() {
    let base = spawn_server(false, true);
    let c = client();
    let source = urlenc("urn:load:source");
    let r = c
        .put(format!("{base}/graphs?graph={source}"))
        .header("content-type", "text/turtle")
        .body("<urn:load:s> <urn:load:p> <urn:load:o> .")
        .send()
        .unwrap();
    assert_eq!(r.status(), 201);

    let source_url = format!("{base}/graphs?graph={source}");
    let r = c
        .post(format!("{base}/sparql"))
        .header("content-type", "application/sparql-update")
        .body(format!("LOAD <{source_url}> INTO GRAPH <urn:load:target>"))
        .send()
        .unwrap();
    assert_eq!(r.status(), 204, "LOAD response: {}", r.text().unwrap());

    let r = c
        .get(format!(
            "{base}/sparql?query={}",
            urlenc("ASK { GRAPH <urn:load:target> { <urn:load:s> <urn:load:p> <urn:load:o> } }")
        ))
        .send()
        .unwrap();
    let j: serde_json::Value = serde_json::from_str(&r.text().unwrap()).unwrap();
    assert_eq!(j["boolean"], serde_json::json!(true));
}

fn urlenc(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

#[test]
fn graph_store_protocol() {
    let base = spawn_server(false, false);
    let c = client();

    // GET the default graph as N-Triples.
    let r = c
        .get(format!("{base}/graphs?default"))
        .header("accept", "application/n-triples")
        .send()
        .unwrap();
    assert_eq!(r.status(), 200);
    let body = r.text().unwrap();
    assert!(
        body.contains("<http://x/alice> <http://x/knows> <http://x/bob> ."),
        "{body}"
    );

    // GET an absent named graph → 404.
    let r = c
        .get(format!("{base}/graphs?graph={}", urlenc("http://x/nope")))
        .send()
        .unwrap();
    assert_eq!(r.status(), 404);

    // PUT creates a named graph → 201; GET returns its content.
    let g = urlenc("http://x/gsp");
    let r = c
        .put(format!("{base}/graphs?graph={g}"))
        .header("content-type", "text/turtle")
        .body("<http://x/e> <http://x/p> 1, 2 .")
        .send()
        .unwrap();
    assert_eq!(r.status(), 201);
    let body = c
        .get(format!("{base}/graphs?graph={g}"))
        .send()
        .unwrap()
        .text()
        .unwrap();
    // Same-subject triples group onto one compact stanza line, integer
    // literals in their bare terse spelling.
    assert!(body.contains(" 1, 2 ."), "{body}");
    assert_eq!(body.lines().count(), 1, "{body}");

    // POST merges → 204; PUT replaces.
    let r = c
        .post(format!("{base}/graphs?graph={g}"))
        .header("content-type", "text/turtle")
        .body("<http://x/e> <http://x/p> 3 .")
        .send()
        .unwrap();
    assert_eq!(r.status(), 204);
    let body = c
        .get(format!("{base}/graphs?graph={g}"))
        .send()
        .unwrap()
        .text()
        .unwrap();
    assert!(body.contains(" 1, 2, 3 ."), "{body}");
    let r = c
        .put(format!("{base}/graphs?graph={g}"))
        .header("content-type", "text/turtle")
        .body("<http://x/e> <http://x/p> 9 .")
        .send()
        .unwrap();
    assert_eq!(r.status(), 204);
    let body = c
        .get(format!("{base}/graphs?graph={g}"))
        .send()
        .unwrap()
        .text()
        .unwrap();
    assert!(body.contains(" 9 ."), "{body}");
    assert_eq!(body.lines().count(), 1, "{body}");

    // The graph participates in SPARQL immediately.
    let r = c
        .get(format!(
            "{base}/sparql?query={}",
            urlenc("SELECT ?o WHERE { GRAPH <http://x/gsp> { ?s ?p ?o } }")
        ))
        .send()
        .unwrap();
    let j: serde_json::Value = serde_json::from_str(&r.text().unwrap()).unwrap();
    assert_eq!(j["results"]["bindings"][0]["o"]["value"], "9");

    // DELETE removes it; a second DELETE 404s; bad payload → 400.
    let r = c.delete(format!("{base}/graphs?graph={g}")).send().unwrap();
    assert_eq!(r.status(), 204);
    let r = c.delete(format!("{base}/graphs?graph={g}")).send().unwrap();
    assert_eq!(r.status(), 404);
    let r = c
        .put(format!("{base}/graphs?graph={g}"))
        .header("content-type", "text/turtle")
        .body("this is not turtle")
        .send()
        .unwrap();
    assert_eq!(r.status(), 400);
}

#[test]
fn gsp_dataset_operations() {
    let base = spawn_server(false, false);
    let c = client();

    let count = |query: &str| -> i64 {
        let r = c
            .post(format!("{base}/sparql"))
            .header("Content-Type", "application/sparql-query")
            .body(query.to_string())
            .send()
            .unwrap();
        let j: serde_json::Value = serde_json::from_str(&r.text().unwrap()).unwrap();
        j["results"]["bindings"][0]["n"]["value"]
            .as_str()
            .unwrap()
            .parse()
            .unwrap()
    };
    let counts = || {
        (
            count("SELECT (COUNT(*) AS ?n) WHERE { ?s ?p ?o }"),
            count("SELECT (COUNT(*) AS ?n) WHERE { GRAPH ?g { ?s ?p ?o } }"),
        )
    };

    // PUT with no graph addressed replaces the entire dataset from TriG.
    let trig = r#"
        <urn:x:s> <urn:x:p> "default" .
        <urn:x:g1> { <urn:x:s> <urn:x:p> "one" . }
        <urn:x:g2> { <urn:x:s> <urn:x:p> "two" , "three" . }
    "#;
    let r = c
        .put(format!("{base}/graphs"))
        .header("Content-Type", "application/trig")
        .body(trig)
        .send()
        .unwrap();
    assert_eq!(r.status().as_u16(), 204, "dataset PUT");
    assert_eq!(counts(), (1, 3), "seeded store content fully replaced");

    // POST merges additional quads (N-Quads).
    let nq = "<urn:x:s2> <urn:x:p> <urn:x:o> <urn:x:g1> .\n<urn:x:s2> <urn:x:p> <urn:x:o> .\n";
    let r = c
        .post(format!("{base}/graphs"))
        .header("Content-Type", "application/n-quads")
        .body(nq)
        .send()
        .unwrap();
    assert_eq!(r.status().as_u16(), 204, "dataset POST");
    assert_eq!(counts(), (2, 4), "merge adds without replacing");

    // DELETE clears the dataset.
    let r = c.delete(format!("{base}/graphs")).send().unwrap();
    assert_eq!(r.status().as_u16(), 204, "dataset DELETE");
    assert_eq!(counts(), (0, 0), "dataset cleared");

    // Dataset retrieval is explicitly unimplemented.
    let r = c.get(format!("{base}/graphs")).send().unwrap();
    assert_eq!(r.status().as_u16(), 501, "dataset GET");

    // Conflicting addressing is rejected.
    let r = c
        .put(format!("{base}/graphs?graph=urn:x:g1&default"))
        .header("Content-Type", "text/turtle")
        .body("<urn:x:s> <urn:x:p> <urn:x:o> .")
        .send()
        .unwrap();
    assert_eq!(r.status().as_u16(), 400, "graph+default rejected");
}

#[test]
fn prefix_registry_from_uploads() {
    let base = spawn_server(false, false);
    let c = client();

    // upload turtle carrying prefix declarations
    let r = c
        .put(format!("{base}/graphs?graph=urn:x:reg"))
        .header("Content-Type", "text/turtle")
        .body("@prefix ex: <http://reg.example/> . ex:s ex:p ex:o .")
        .send()
        .unwrap();
    assert_eq!(r.status().as_u16(), 201);

    // GSP GET emits the registered prefix
    let r = c
        .get(format!("{base}/graphs?graph=urn:x:reg"))
        .header("Accept", "text/turtle")
        .send()
        .unwrap();
    let body = r.text().unwrap();
    assert!(
        body.contains("@prefix ex: <http://reg.example/> ."),
        "GSP GET should carry registry prefixes:\n{body}"
    );

    // construct responses merge the registry with the query prologue (query
    // wins), observable through compaction — the header carries only used
    // declarations, so the constructed triple exercises the ex: prefix.
    let r = c
        .post(format!("{base}/sparql"))
        .header("Content-Type", "application/sparql-query")
        .header("Accept", "text/turtle")
        .body(concat!(
            "PREFIX ex: <http://query.example/> ",
            "CONSTRUCT { ex:a ex:b ex:c } WHERE {}"
        ))
        .send()
        .unwrap();
    let body = r.text().unwrap();
    assert!(
        body.contains("@prefix ex: <http://query.example/> ."),
        "query prologue must win:\n{body}"
    );
    assert!(
        body.contains("ex:a ex:b ex:c ."),
        "body must compact:\n{body}"
    );
    assert!(
        !body.contains("http://reg.example/"),
        "shadowed registry entry must not duplicate:\n{body}"
    );
}

#[test]
fn post_accepts_query_string_protocol_params() {
    let base = spawn_server(false, false);
    let c = client();

    // update supplied in the URL query string of a POST with an empty form body
    let r = c
        .post(format!(
            "{base}/sparql?update=insert%20data%20%7B%20%3Curn:qs:s%3E%20%3Curn:qs:p%3E%20%3Curn:qs:o%3E%20%7D"
        ))
        .header("Content-Type", "application/x-www-form-urlencoded")
        .send()
        .unwrap();
    assert_eq!(r.status().as_u16(), 204, "query-string update on POST");

    let r = c
        .post(format!("{base}/sparql"))
        .header("Content-Type", "application/sparql-query")
        .body("ASK { <urn:qs:s> <urn:qs:p> <urn:qs:o> }")
        .send()
        .unwrap();
    let j: serde_json::Value = serde_json::from_str(&r.text().unwrap()).unwrap();
    assert_eq!(j["boolean"], serde_json::json!(true));
}
