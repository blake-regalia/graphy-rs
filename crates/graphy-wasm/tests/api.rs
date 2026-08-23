//! The graphy-wasm API, exercised natively (the crate is target-agnostic;
//! only the wall-clock injection differs on wasm).

use graphy_wasm::GraphStore;

#[test]
fn load_query_update_export() {
    let store = GraphStore::new().unwrap();
    assert_eq!(store.size().unwrap(), 0);

    let n = store
        .load(
            "@prefix ex: <http://e/> .\n\
             ex:s ex:p 1, 2 ; ex:name \"Alice\"@en .\n\
             ex:t ex:list ( 1 2 ) .\n",
            "turtle",
            None,
        )
        .unwrap();
    assert_eq!(n, 8, "3 direct + 4 list-spine + 1 head quads");

    // SELECT → SPARQL results JSON.
    let srj = store
        .query("PREFIX ex: <http://e/> SELECT ?o WHERE { ex:s ex:p ?o } ORDER BY ?o")
        .unwrap();
    let v: serde_json::Value = serde_json::from_str(&srj).expect("valid SRJ");
    assert_eq!(v["head"]["vars"], serde_json::json!(["o"]));
    let bindings = v["results"]["bindings"].as_array().unwrap();
    assert_eq!(bindings.len(), 2, "{srj}");
    assert_eq!(bindings[0]["o"]["type"], "literal");
    assert_eq!(
        bindings[0]["o"]["datatype"],
        "http://www.w3.org/2001/XMLSchema#integer"
    );

    // Language tags surface as xml:lang.
    let srj = store
        .query("PREFIX ex: <http://e/> SELECT ?n WHERE { ex:s ex:name ?n }")
        .unwrap();
    assert!(srj.contains("\"xml:lang\":\"en\""), "{srj}");

    // ASK → boolean JSON; NOW() must not panic (ambient clock natively).
    let srj = store
        .query("PREFIX ex: <http://e/> ASK { ex:s ex:p 1 . FILTER(YEAR(NOW()) >= 2026) }")
        .unwrap();
    assert_eq!(srj, "{\"head\":{},\"boolean\":true}");

    // CONSTRUCT → N-Triples.
    let nt = store
        .query("PREFIX ex: <http://e/> CONSTRUCT { ?s ex:copied ?o } WHERE { ?s ex:p ?o }")
        .unwrap();
    assert_eq!(nt.lines().count(), 2, "{nt}");
    assert!(nt.contains("<http://e/copied>"), "{nt}");

    // Update: delete + insert atomically; then verify.
    store
        .update(
            "PREFIX ex: <http://e/>\n\
             DELETE { ex:s ex:p ?o } INSERT { ex:s ex:p 99 } WHERE { ex:s ex:p ?o }",
        )
        .unwrap();
    let srj = store
        .query("PREFIX ex: <http://e/> SELECT ?o WHERE { ex:s ex:p ?o }")
        .unwrap();
    let v: serde_json::Value = serde_json::from_str(&srj).unwrap();
    assert_eq!(v["results"]["bindings"].as_array().unwrap().len(), 1);
    assert!(srj.contains("\"99\""), "{srj}");

    // Exports: canonical N-Quads and pretty Turtle.
    let nq = store.export("nquads").unwrap();
    assert_eq!(nq.lines().count(), store.size().unwrap() as usize);
    let ttl = store.export("turtle").unwrap();
    assert!(ttl.contains("<http://e/s>"), "{ttl}");

    // Named graphs force trig.
    store
        .update("PREFIX ex: <http://e/> INSERT DATA { GRAPH ex:g { ex:a ex:b ex:c } }")
        .unwrap();
    assert!(store.export("turtle").is_err());
    let trig = store.export("trig").unwrap();
    assert!(trig.contains("{"), "{trig}");
}

#[test]
fn turtle_export_keeps_shared_blank_nodes() {
    // A store scan carries no single-reference guarantee for fresh-shaped
    // `b{n}` labels: an update can copy an existing blank node into new
    // triples. Anonymous-node reconstruction would splice the capture at the
    // first reference and strand the second as a different node.
    let store = GraphStore::new().unwrap();
    store
        .load(
            "@prefix ex: <http://e/> .\nex:a ex:p [ ex:q 1 ] .\n",
            "turtle",
            None,
        )
        .unwrap();
    store
        .update("PREFIX ex: <http://e/> INSERT { ex:b ex:r ?x } WHERE { ex:a ex:p ?x }")
        .unwrap();
    assert_eq!(store.size().unwrap(), 3);

    // The Turtle export must re-parse to the same graph: both subjects still
    // reach ONE node carrying `ex:q 1`.
    let ttl = store.export("turtle").unwrap();
    let round = GraphStore::new().unwrap();
    round.load(&ttl, "turtle", None).unwrap();
    assert_eq!(round.size().unwrap(), 3, "{ttl}");
    let srj = round
        .query(
            "PREFIX ex: <http://e/> \
             ASK { ex:a ex:p ?x . ex:b ex:r ?x . ?x ex:q 1 }",
        )
        .unwrap();
    assert_eq!(srj, "{\"head\":{},\"boolean\":true}", "{ttl}");
}

#[test]
fn sequential_loads_keep_blank_nodes_distinct() {
    // Blank labels are document-scoped: a parser restarted per load mints
    // `_b0` twice, and the store would silently unify the two anonymous
    // nodes. Each load must parse under its own label namespace.
    let store = GraphStore::new().unwrap();
    store
        .load(
            "@prefix ex: <http://e/> .\nex:a ex:p [ ex:q 1 ] .\n",
            "turtle",
            None,
        )
        .unwrap();
    store
        .load(
            "@prefix ex: <http://e/> .\nex:b ex:p [ ex:q 2 ] .\n",
            "turtle",
            None,
        )
        .unwrap();
    assert_eq!(store.size().unwrap(), 4);
    let srj = store
        .query("PREFIX ex: <http://e/> ASK { ?x ex:q 1 . ?x ex:q 2 }")
        .unwrap();
    assert_eq!(
        srj,
        "{\"head\":{},\"boolean\":false}",
        "one node captured both documents' blanks:\n{}",
        store.export("nquads").unwrap()
    );

    // Surface labels are document-scoped too: `_:x` in two loads is two
    // nodes (first-seen ordinals restart at `_s0` per parser).
    let store = GraphStore::new().unwrap();
    store
        .load("_:x <http://e/q> \"a\" .", "ntriples", None)
        .unwrap();
    store
        .load("_:x <http://e/q> \"b\" .", "ntriples", None)
        .unwrap();
    let srj = store
        .query("ASK { ?x <http://e/q> \"a\" . ?x <http://e/q> \"b\" }")
        .unwrap();
    assert_eq!(srj, "{\"head\":{},\"boolean\":false}");

    // Same document-scoping through the data-parallel NT/NQ path (content
    // labels `_s{surface}` are chunking-independent within a load, so only
    // the namespace keeps loads apart).
    let store = GraphStore::new().unwrap();
    store.set_threads(4);
    store
        .load("_:x <http://e/q> \"a\" .", "nquads", None)
        .unwrap();
    store
        .load("_:x <http://e/q> \"b\" .", "nquads", None)
        .unwrap();
    let srj = store
        .query("ASK { ?x <http://e/q> \"a\" . ?x <http://e/q> \"b\" }")
        .unwrap();
    assert_eq!(srj, "{\"head\":{},\"boolean\":false}");
}

#[test]
fn restored_log_loads_keep_blank_nodes_distinct() {
    // Session 1: one load bakes its (unprefixed first-load) blank labels
    // into the durable log.
    let store = GraphStore::with_log(None).unwrap();
    store
        .load(
            "@prefix ex: <http://e/> .\nex:a ex:p [ ex:q 1 ] .\n",
            "turtle",
            None,
        )
        .unwrap();
    let log = store.drain_log();

    // Session 2: no counter survived the process, so a restored store's
    // loads must start their label namespaces off the clock, not at 0 —
    // or this load would re-mint the baked `_b0` and capture it.
    let restored = GraphStore::with_log(Some(&log)).unwrap();
    restored
        .load(
            "@prefix ex: <http://e/> .\nex:b ex:p [ ex:q 2 ] .\n",
            "turtle",
            None,
        )
        .unwrap();
    assert_eq!(restored.size().unwrap(), 4);
    let srj = restored
        .query("PREFIX ex: <http://e/> ASK { ?x ex:q 1 . ?x ex:q 2 }")
        .unwrap();
    assert_eq!(
        srj,
        "{\"head\":{},\"boolean\":false}",
        "a restored-store load captured a baked-in blank:\n{}",
        restored.export("nquads").unwrap()
    );
}

#[test]
fn parse_and_query_errors_are_reported() {
    let store = GraphStore::new().unwrap();
    assert!(store.load("not turtle at all !!!", "turtle", None).is_err());
    assert!(store.load("ex:s ex:p ex:o .", "martian", None).is_err());
    assert!(store.query("SELECT WHERE").is_err());
    assert!(store.update("DELETE GARBAGE").is_err());
}

#[test]
fn persistence_log_round_trip() {
    let store = GraphStore::with_log(None).unwrap();
    store
        .load(
            "@prefix ex: <http://e/> .\nex:s ex:p 1, 2 .\n",
            "turtle",
            None,
        )
        .unwrap();
    store
        .update("PREFIX ex: <http://e/> INSERT DATA { GRAPH ex:g { ex:a ex:b ex:c } }")
        .unwrap();
    let mut log = store.drain_log();
    assert!(!log.is_empty());
    store
        .update("PREFIX ex: <http://e/> DELETE DATA { ex:s ex:p 1 }")
        .unwrap();
    log.extend(store.drain_log());

    let restored = GraphStore::with_log(Some(&log)).unwrap();
    assert_eq!(restored.size().unwrap(), 2);
    let srj = restored
        .query("PREFIX ex: <http://e/> SELECT ?o WHERE { ex:s ex:p ?o }")
        .unwrap();
    assert!(srj.contains("\"2\""), "{srj}");
    assert!(!srj.contains("\"1\""), "{srj}");

    // Pack + continue.
    let packed = restored.pack_log().unwrap();
    let from_pack = GraphStore::with_log(Some(&packed)).unwrap();
    assert_eq!(from_pack.size().unwrap(), 2);
}

#[test]
fn from_image_and_parallel_load() {
    use graphy_store::{BuilderConfig, SegmentBuilder};
    // Build a segment natively, read its files, open as an image store.
    let dir = std::env::temp_dir().join(format!("graphy-wasm-img-{}", std::process::id()));
    if dir.exists() {
        std::fs::remove_dir_all(&dir).unwrap();
    }
    std::fs::create_dir_all(&dir).unwrap();
    let mut b = SegmentBuilder::new(BuilderConfig::new(&dir)).unwrap();
    for i in 0..200u32 {
        b.push_quad(
            format!(">http://x/s{}", i % 20).as_bytes(),
            b">http://x/p".as_ref(),
            format!(">http://x/o{i}").as_bytes(),
            None,
        )
        .unwrap();
    }
    b.finish().unwrap();
    let mut files: Vec<(String, Vec<u8>)> = Vec::new();
    fn walk(root: &std::path::Path, d: &std::path::Path, out: &mut Vec<(String, Vec<u8>)>) {
        for e in std::fs::read_dir(d).unwrap() {
            let p = e.unwrap().path();
            if p.is_dir() {
                walk(root, &p, out);
            } else {
                out.push((
                    p.strip_prefix(root).unwrap().to_string_lossy().into_owned(),
                    std::fs::read(&p).unwrap(),
                ));
            }
        }
    }
    walk(&dir, &dir, &mut files);

    let store = GraphStore::from_image(&files, None).unwrap();
    assert_eq!(store.size().unwrap(), 200);
    let srj = store
        .query("SELECT ?o WHERE { <http://x/s1> <http://x/p> ?o }")
        .unwrap();
    assert!(srj.contains("http://x/o1"), "{srj}");

    // Edits layer over the image; the log holds only the edits.
    store
        .update("INSERT DATA { <http://x/new> <http://x/p> 1 }")
        .unwrap();
    let log = store.drain_log();
    assert!(!log.is_empty() && log.len() < 200);
    let again = GraphStore::from_image(&files, Some(&log)).unwrap();
    assert_eq!(again.size().unwrap(), 201);

    // Parallel N-Quads load (std threads natively; wasm_thread on the
    // threads build) agrees with the serial path.
    let nq: String = (0..3000)
        .map(|i| format!("<http://e/s{}> <http://e/p> <http://e/o{i}> .\n", i % 300))
        .collect();
    let par_store = GraphStore::new().unwrap();
    par_store.set_threads(4);
    assert_eq!(par_store.load(&nq, "nquads", None).unwrap(), 3000);
    let ser_store = GraphStore::new().unwrap();
    assert_eq!(ser_store.load(&nq, "nquads", None).unwrap(), 3000);
    // Arrival order differs across workers, so overlay TermIds (and hence
    // SPO export order) differ — content must be identical as a set.
    let sorted = |s: String| {
        let mut l: Vec<&str> = s.lines().collect();
        l.sort_unstable();
        l.join("\n")
    };
    assert_eq!(
        sorted(par_store.export("nquads").unwrap()),
        sorted(ser_store.export("nquads").unwrap()),
        "parallel and serial loads must agree"
    );
    std::fs::remove_dir_all(&dir).ok();
}
