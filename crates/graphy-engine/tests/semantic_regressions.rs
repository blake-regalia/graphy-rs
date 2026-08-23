use graphy_algebra::{rewrite, translate_query, translate_update};
use graphy_engine::{evaluate_ref, execute_update_with_loader, EngineError, Output};
use graphy_sparql_syntax::{parse_query, parse_update};
use graphy_store::{BuilderConfig, SegmentBuilder, Store};

fn query(data: &[(&[u8], &[u8], &[u8])], sparql: &str) -> Output {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let dir = std::env::temp_dir().join(format!(
        "graphy-semantic-regression-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    let mut builder = SegmentBuilder::new(BuilderConfig::new(&dir)).unwrap();
    for (s, p, o) in data {
        builder.push_quad(s, p, o, None).unwrap();
    }
    builder.finish().unwrap();
    let store = Store::open(&dir).unwrap();
    let parsed = parse_query(sparql).unwrap();
    let mut translated = translate_query(&parsed).unwrap();
    translated.root = rewrite(translated.root);
    let output = evaluate_ref(&store.snapshot(), &translated).unwrap();
    drop(store);
    let _ = std::fs::remove_dir_all(dir);
    output
}

#[test]
fn rdf_term_identity_is_position_independent() {
    let data = [(
        b">http://example/a".as_slice(),
        b">http://example/a".as_slice(),
        b"@en\"value".as_slice(),
    )];

    assert_eq!(
        query(&data, "ASK { ?x ?x ?o }"),
        Output::Boolean(true),
        "a repeated variable must compare RDF terms, not dictionary columns"
    );
    assert_eq!(
        query(&data, "ASK { ?s ?p ?o FILTER(sameTerm(?s, ?p)) }"),
        Output::Boolean(true),
        "sameTerm must be independent of the term's triple position"
    );
    assert_eq!(
        query(&data, "ASK { ?s <http://example/a> ?o . ?p ?s ?o }"),
        Output::Boolean(true),
        "join compatibility must be independent of the binding position"
    );
}

#[test]
fn zero_length_path_retains_an_absent_constant() {
    let data = [(
        b">http://example/present".as_slice(),
        b">http://example/p".as_slice(),
        b">http://example/object".as_slice(),
    )];
    let Output::Solutions { vars, rows } = query(
        &data,
        "SELECT ?y WHERE { <http://example/missing> <http://example/p>* ?y }",
    ) else {
        panic!("SELECT result");
    };
    assert_eq!(vars, ["y"]);
    assert_eq!(rows, [vec![Some(b">http://example/missing".to_vec())]]);

    assert_eq!(
        query(
            &data,
            "ASK { <http://example/missing> <http://example/p>* <http://example/missing> }"
        ),
        Output::Boolean(true)
    );
}

#[test]
fn construct_drops_every_illegal_literal_subject() {
    let data = [(
        b">http://example/s".as_slice(),
        b">http://example/p".as_slice(),
        b"@en\"literal subject".as_slice(),
    )];
    assert_eq!(
        query(
            &data,
            "CONSTRUCT { ?o <http://example/out> <http://example/value> } \
             WHERE { ?s <http://example/p> ?o }"
        ),
        Output::Triples(Vec::new())
    );
}

#[test]
fn numeric_and_temporal_value_semantics_preserve_declared_types() {
    let empty: [(&[u8], &[u8], &[u8]); 0] = [];
    assert_eq!(
        query(
            &empty,
            "PREFIX xsd: <http://www.w3.org/2001/XMLSchema#> \
             ASK { FILTER(isNumeric(\"1\"^^xsd:int)) }"
        ),
        Output::Boolean(true)
    );
    assert_eq!(
        query(
            &empty,
            "PREFIX xsd: <http://www.w3.org/2001/XMLSchema#> \
             ASK { FILTER(datatype(\"1\"^^xsd:float) = xsd:float) }"
        ),
        Output::Boolean(true)
    );
    assert_eq!(
        query(
            &empty,
            "PREFIX xsd: <http://www.w3.org/2001/XMLSchema#> ASK { \
             FILTER(\"2002-10-10T12:00:00-05:00\"^^xsd:dateTime = \
                    \"2002-10-10T17:00:00Z\"^^xsd:dateTime) }"
        ),
        Output::Boolean(true)
    );
}

#[test]
fn absolute_iri_identity_is_not_normalized_as_a_relative_reference() {
    let data = [(
        b">http://example/s".as_slice(),
        b">http://example/vocab#p".as_slice(),
        b">eXAMPLE://a/./b/../b/%63/%7bfoo%7d#xyz".as_slice(),
    )];
    assert_eq!(
        query(
            &data,
            "BASE <file:///tmp/> \
             PREFIX p1: <eXAMPLE://a/./b/../b/%63/%7bfoo%7d#> \
             ASK { ?s <http://example/vocab#p> p1:xyz }"
        ),
        Output::Boolean(true)
    );
}

#[test]
fn load_uses_the_injected_retriever_and_honors_into_and_silent() {
    let dir = std::env::temp_dir().join(format!("graphy-load-regression-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    SegmentBuilder::new(BuilderConfig::new(&dir))
        .unwrap()
        .finish()
        .unwrap();
    let store = Store::open(&dir).unwrap();
    let update = translate_update(
        &parse_update("LOAD <https://example.test/data.ttl> INTO GRAPH <https://example.test/g>")
            .unwrap(),
    )
    .unwrap();
    execute_update_with_loader(&store, &update, &mut |source| {
        assert_eq!(source, b">https://example.test/data.ttl");
        Ok(vec![(
            b">https://example.test/s".to_vec(),
            b">https://example.test/p".to_vec(),
            b"\"value".to_vec(),
        )])
    })
    .unwrap();
    let snap = store.snapshot();
    let mut scan = snap.scan_best(&graphy_store::Pattern::default()).unwrap();
    let mut batch = graphy_store::QuadBatch::new();
    assert!(scan.next_batch(&mut batch).unwrap());
    assert_eq!(batch.len(), 1);
    assert_eq!(
        snap.decode_value(batch.g[0], graphy_store::TermPos::Graph)
            .unwrap(),
        b">https://example.test/g"
    );
    drop(scan);
    drop(snap);

    let silent =
        translate_update(&parse_update("LOAD SILENT <https://example.test/missing>").unwrap())
            .unwrap();
    execute_update_with_loader(&store, &silent, &mut |_| {
        Err(EngineError("retrieval failed".into()))
    })
    .unwrap();
    drop(store);
    let _ = std::fs::remove_dir_all(dir);
}
