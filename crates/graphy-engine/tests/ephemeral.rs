//! SPARQL end-to-end on an ephemeral store (docs/11 M12a): the browser-mode
//! store must serve Update execution and both evaluators exactly like a
//! directory-backed one.

use graphy_algebra::{rewrite, translate_query, translate_update};
use graphy_engine::{evaluate, evaluate_ref, execute_update, Output};
use graphy_sparql_syntax::{parse_query, parse_update};
use graphy_store::Store;

fn rows(out: Output) -> Vec<Vec<Option<Vec<u8>>>> {
    match out {
        Output::Solutions { rows, .. } => rows,
        other => panic!("expected solutions, got {other:?}"),
    }
}

#[test]
fn update_then_query_both_engines() {
    let store = Store::ephemeral().unwrap();

    // INSERT DATA with a named graph and a typed literal.
    let u = parse_update(
        "PREFIX ex: <http://e/>\n\
         INSERT DATA { ex:s ex:p 1, 2 . ex:s ex:q ex:o . GRAPH ex:g { ex:s ex:p 3 } }",
    )
    .unwrap();
    execute_update(&store, &translate_update(&u).unwrap()).unwrap();

    let q =
        parse_query("PREFIX ex: <http://e/> SELECT ?o WHERE { ex:s ex:p ?o } ORDER BY ?o").unwrap();
    let mut t = translate_query(&q).unwrap();
    t.root = rewrite(t.root.clone());
    let snap = store.snapshot();
    let vec_rows = rows(evaluate(&snap, &t).unwrap());
    let ref_rows = rows(evaluate_ref(&snap, &t).unwrap());
    assert_eq!(vec_rows.len(), 2, "default graph only");
    assert_eq!(vec_rows, ref_rows, "engines disagree on ephemeral store");

    // DELETE WHERE prunes; a fresh snapshot sees it.
    let u = parse_update("PREFIX ex: <http://e/> DELETE WHERE { ex:s ex:p ?o }").unwrap();
    execute_update(&store, &translate_update(&u).unwrap()).unwrap();
    let snap = store.snapshot();
    assert_eq!(rows(evaluate(&snap, &t).unwrap()).len(), 0);

    // The named graph is untouched.
    let q =
        parse_query("PREFIX ex: <http://e/> SELECT ?o WHERE { GRAPH ex:g { ?s ?p ?o } }").unwrap();
    let mut t = translate_query(&q).unwrap();
    t.root = rewrite(t.root.clone());
    assert_eq!(rows(evaluate(&snap, &t).unwrap()).len(), 1);

    // ASK through both engines.
    let q = parse_query("PREFIX ex: <http://e/> ASK { ex:s ex:q ex:o }").unwrap();
    let mut t = translate_query(&q).unwrap();
    t.root = rewrite(t.root.clone());
    assert!(matches!(
        evaluate(&snap, &t).unwrap(),
        Output::Boolean(true)
    ));
    assert!(matches!(
        evaluate_ref(&snap, &t).unwrap(),
        Output::Boolean(true)
    ));
}

#[test]
fn bnode_bindings_are_fresh_across_update_requests() {
    let store = Store::ephemeral().unwrap();
    for subject in ["a", "b"] {
        let update = parse_update(&format!(
            "INSERT {{ <http://e/{subject}> <http://e/p> ?b ; <http://e/q> ?b }} \
             WHERE {{ BIND(BNODE() AS ?b) }}"
        ))
        .unwrap();
        execute_update(&store, &translate_update(&update).unwrap()).unwrap();
    }

    let query =
        parse_query("SELECT ?s ?b WHERE { ?s <http://e/p> ?b ; <http://e/q> ?b } ORDER BY ?s")
            .unwrap();
    let mut translated = translate_query(&query).unwrap();
    translated.root = rewrite(translated.root.clone());
    let rows = rows(evaluate(&store.snapshot(), &translated).unwrap());
    assert_eq!(rows.len(), 2);
    let first = rows[0][1].as_ref().expect("first BNODE binding");
    let second = rows[1][1].as_ref().expect("second BNODE binding");
    assert!(first.starts_with(b"_u"));
    assert!(second.starts_with(b"_u"));
    assert_ne!(first, second, "separate updates reused one blank node");
}

#[test]
fn bnode_bindings_inside_triple_terms_are_freshened() {
    let store = Store::ephemeral().unwrap();
    for subject in ["a", "b"] {
        let update = parse_update(&format!(
            "INSERT {{ <http://e/{subject}> <http://e/p> ?t }} WHERE {{ \
             BIND(BNODE() AS ?b) \
             BIND(TRIPLE(?b, <http://e/q>, <http://e/o>) AS ?t) }}"
        ))
        .unwrap();
        execute_update(&store, &translate_update(&update).unwrap()).unwrap();
    }

    let query = parse_query("SELECT ?t WHERE { ?s <http://e/p> ?t } ORDER BY ?s").unwrap();
    let mut translated = translate_query(&query).unwrap();
    translated.root = rewrite(translated.root.clone());
    let rows = rows(evaluate(&store.snapshot(), &translated).unwrap());
    assert_eq!(rows.len(), 2);
    let first = rows[0][0].as_ref().expect("first triple term");
    let second = rows[1][0].as_ref().expect("second triple term");
    assert_ne!(first, second, "nested blanks were not freshened");
    for term in [first, second] {
        let graphy_core::TermRef::TripleTerm(view) = graphy_core::concise::decode(term).unwrap()
        else {
            panic!("expected triple term")
        };
        assert!(matches!(
            view.subject(),
            graphy_core::TermRef::BlankNode(label) if label.starts_with('u')
        ));
    }
}

#[test]
fn generated_bnodes_are_distinct_from_dataset_labels() {
    let store = Store::ephemeral().unwrap();
    store
        .apply(
            &[],
            &[(
                &b">http://e/s"[..],
                &b">http://e/p"[..],
                &b"_gen1"[..],
                None,
            )],
        )
        .unwrap();

    let query = parse_query(
        "SELECT ?stored ?generated WHERE { \
         <http://e/s> <http://e/p> ?stored . BIND(BNODE() AS ?generated) }",
    )
    .unwrap();
    let mut translated = translate_query(&query).unwrap();
    translated.root = rewrite(translated.root.clone());
    let rows = rows(evaluate(&store.snapshot(), &translated).unwrap());
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0][0].as_deref(), Some(&b"_gen1"[..]));
    assert_ne!(rows[0][0], rows[0][1]);
}

/// The scheduler's inline single-worker path (the wasm32 configuration —
/// `available_parallelism` resolves to 1 there) must agree with the
/// multi-worker path and the reference evaluator.
#[test]
fn single_worker_inline_path_agrees() {
    use graphy_engine::{evaluate_with, ExecOptions};
    let store = Store::ephemeral().unwrap();
    let mut inserts = String::from("PREFIX ex: <http://e/> INSERT DATA {");
    for i in 0..300 {
        inserts.push_str(&format!(" ex:s{} ex:p ex:o{} .", i % 40, i));
    }
    inserts.push('}');
    execute_update(
        &store,
        &translate_update(&parse_update(&inserts).unwrap()).unwrap(),
    )
    .unwrap();

    let q = parse_query(
        "PREFIX ex: <http://e/> SELECT ?s ?o WHERE { ?s ex:p ?o . ?s ex:p ?o2 } ORDER BY ?s ?o",
    )
    .unwrap();
    let mut t = translate_query(&q).unwrap();
    t.root = rewrite(t.root.clone());
    let snap = store.snapshot();
    let one = rows(
        evaluate_with(
            &snap,
            &t,
            &ExecOptions {
                threads: 1,
                ..ExecOptions::default()
            },
        )
        .unwrap(),
    );
    let many = rows(
        evaluate_with(
            &snap,
            &t,
            &ExecOptions {
                threads: 8,
                ..ExecOptions::default()
            },
        )
        .unwrap(),
    );
    let reference = rows(evaluate_ref(&snap, &t).unwrap());
    assert!(!one.is_empty());
    assert_eq!(one, many, "inline vs spawned workers");
    assert_eq!(one, reference, "inline vs reference");
}

/// Compaction may discard dead overlay terms and reissue the surviving
/// graph columns without advancing the write epoch. A cached physical plan
/// must therefore be scoped to the delta/id-space incarnation, not only to
/// `(generation, epoch)`.
#[test]
fn cached_named_graph_plan_survives_compaction() {
    let store = Store::ephemeral().unwrap();
    let insert = parse_update(
        "INSERT DATA { \
         GRAPH <http://e/dead> { <http://e/s> <http://e/p> 0 } \
         GRAPH <http://e/live> { <http://e/s> <http://e/p> 1 } }",
    )
    .unwrap();
    execute_update(&store, &translate_update(&insert).unwrap()).unwrap();
    let delete =
        parse_update("DELETE DATA { GRAPH <http://e/dead> { <http://e/s> <http://e/p> 0 } }")
            .unwrap();
    execute_update(&store, &translate_update(&delete).unwrap()).unwrap();

    let q =
        parse_query("SELECT ?o WHERE { GRAPH <http://e/live> { <http://e/s> <http://e/p> ?o } }")
            .unwrap();
    let mut t = translate_query(&q).unwrap();
    t.root = rewrite(t.root.clone());

    // Populate the process-wide vectorized plan cache with the old graph
    // column, then fold away the earlier/dead graph at the same epoch.
    assert_eq!(rows(evaluate(&store.snapshot(), &t).unwrap()).len(), 1);
    let before = store.snapshot();
    store.compact_ephemeral().unwrap();
    let after = store.snapshot();
    assert_eq!(before.epoch(), after.epoch());
    assert_ne!(before.storage_identity(), after.storage_identity());

    let reference = rows(evaluate_ref(&after, &t).unwrap());
    let vectorized = rows(evaluate(&after, &t).unwrap());
    assert_eq!(reference.len(), 1);
    assert_eq!(vectorized, reference);
}
