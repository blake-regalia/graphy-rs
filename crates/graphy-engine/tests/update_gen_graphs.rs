//! Named graphs created by updates must remain resolvable to update operations across
//! merge generations: a graph applied AFTER a merge fold must be visible to COPY/DROP and
//! to pattern matching in subsequent updates (found via Flexo MMS's model-load flow, whose
//! per-transaction load graphs intermittently "did not exist" for the follow-up
//! diff/drop updates once the store had been through background merges).

use std::path::PathBuf;

use graphy_algebra::{rewrite, translate_query, translate_update};
use graphy_engine::{evaluate, execute_update, Output};
use graphy_sparql_syntax::{parse_query, parse_update};
use graphy_store::{BuilderConfig, MergeConfig, Profile, SegmentBuilder, Store};

fn build_store(dir: &PathBuf) -> Store {
    let _ = std::fs::remove_dir_all(dir);
    let mut cfg = BuilderConfig::new(dir);
    cfg.profile = Profile::Balanced;
    cfg.sort_budget = 1 << 14;
    let mut b = SegmentBuilder::new(cfg).unwrap();
    b.push_quad(b">http://x/s", b">http://x/p", b">http://x/o", None)
        .unwrap();
    b.finish().unwrap();
    Store::open(dir).unwrap()
}

fn run_update(store: &Store, src: &str) {
    let req = parse_update(src).unwrap_or_else(|e| panic!("parse `{src}`: {e}"));
    let tu = translate_update(&req).unwrap_or_else(|e| panic!("translate `{src}`: {e}"));
    execute_update(store, &tu).unwrap_or_else(|e| panic!("execute `{src}`: {e}"));
}

fn count_in_graph(store: &Store, graph: &str) -> usize {
    let src = format!("select ?s ?p ?o {{ graph <{graph}> {{ ?s ?p ?o }} }}");
    let q = parse_query(&src).unwrap();
    let mut t = translate_query(&q).unwrap();
    t.root = rewrite(t.root.clone());
    match evaluate(&store.snapshot(), &t).unwrap() {
        Output::Solutions { rows, .. } => rows.len(),
        other => panic!("expected solutions, got {other:?}"),
    }
}

#[test]
fn graphs_created_after_a_merge_are_resolvable_to_updates() {
    let dir = std::env::temp_dir().join(format!(
        "graphy-engine-update-gen-graphs-{}",
        std::process::id()
    ));
    let store = build_store(&dir);

    // generation 0 delta: create g1
    run_update(
        &store,
        "insert data { graph <urn:x:g1> { <urn:x:s1> <urn:x:p> <urn:x:o1> } }",
    );

    // fold the delta into a new base generation
    store.merge_with(&MergeConfig::default()).unwrap();

    // post-merge delta: create g2
    run_update(
        &store,
        "insert data { graph <urn:x:g2> { <urn:x:s2> <urn:x:p> <urn:x:o2> } }",
    );

    // both graphs must be visible to queries…
    assert_eq!(
        count_in_graph(&store, "urn:x:g1"),
        1,
        "pre-merge graph query"
    );
    assert_eq!(
        count_in_graph(&store, "urn:x:g2"),
        1,
        "post-merge graph query"
    );

    // …and resolvable to update operations (COPY sources it, DROP removes it)
    run_update(&store, "copy graph <urn:x:g2> to graph <urn:x:copy>");
    assert_eq!(
        count_in_graph(&store, "urn:x:copy"),
        1,
        "copied post-merge graph"
    );
    run_update(&store, "drop graph <urn:x:g2>");
    run_update(&store, "drop graph <urn:x:g1>");
    assert_eq!(count_in_graph(&store, "urn:x:g1"), 0);
    assert_eq!(count_in_graph(&store, "urn:x:g2"), 0);
}

#[test]
fn graphs_recreated_after_drop_all_and_merge_are_resolvable() {
    let dir = std::env::temp_dir().join(format!(
        "graphy-engine-update-gen-graphs-recreate-{}",
        std::process::id()
    ));
    let store = build_store(&dir);

    // several drop-all / re-create cycles straddling merges, like a test suite
    // resetting its dataset between tests
    for cycle in 0..4 {
        run_update(&store, "drop all");
        run_update(
            &store,
            "insert data { graph <urn:x:meta> { <urn:x:s> <urn:x:p> <urn:x:o> } }",
        );
        run_update(
            &store,
            &format!(
                "insert data {{ graph <urn:x:load{cycle}> {{ <urn:x:s> <urn:x:p> <urn:x:o> }} }}"
            ),
        );
        if cycle % 2 == 1 {
            store.merge_with(&MergeConfig::default()).unwrap();
        }
        assert_eq!(
            count_in_graph(&store, &format!("urn:x:load{cycle}")),
            1,
            "cycle {cycle}: fresh graph query-visible"
        );
        run_update(
            &store,
            &format!("copy graph <urn:x:load{cycle}> to graph <urn:x:model{cycle}>"),
        );
        run_update(&store, &format!("drop graph <urn:x:load{cycle}>"));
    }
}
