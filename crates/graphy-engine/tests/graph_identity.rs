//! Cross-section term identity for graph variables: a pre-bound `?g` (VALUES/BIND) must
//! join against `GRAPH ?g` enumeration bindings even when the graph's IRI also exists in
//! other term sections (subject/object), where `Evaluator::intern` canonicalizes constants
//! to a different section's id than the graphs section.
//!
//! Regression test for the empty-join bug surfaced by the Flexo layer1 dialect gate: the
//! store's graph IRIs all appear as subjects in registry graphs, so every
//! `values ?g {...} graph ?g {...}` (and bind-derived equivalent) returned nothing.

use std::path::PathBuf;

use graphy_algebra::{rewrite, translate_query};
use graphy_engine::exec::{evaluate_with, ExecOptions};
use graphy_engine::{evaluate_ref, Output};
use graphy_sparql_syntax::parse_query;
use graphy_store::{BuilderConfig, Profile, SegmentBuilder, Store};

/// The graph IRI `<http://x/g>` exists BOTH as a named graph and as a subject in the
/// default graph (the layer1 store shape: graph IRIs are registered as subjects).
fn build_store(dir: &PathBuf) -> Store {
    let iri = |s: &str| format!(">http://x/{s}").into_bytes();
    let mut cfg = BuilderConfig::new(dir);
    cfg.profile = Profile::Balanced;
    cfg.sort_budget = 1 << 14;
    let mut b = SegmentBuilder::new(cfg).unwrap();
    // registry triple: the graph IRI as a subject in the default graph
    b.push_quad(&iri("g"), &iri("a"), &iri("ModelGraph"), None)
        .unwrap();
    // content in the named graph
    b.push_quad(&iri("s"), &iri("p"), &iri("o"), Some(&iri("g")))
        .unwrap();
    b.finish().unwrap();
    Store::open(dir).unwrap()
}

fn count_rows(name: &str, src: &str) -> (usize, usize) {
    let dir = std::env::temp_dir().join(format!(
        "graphy-engine-graph-identity-{}-{name}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    let store = build_store(&dir);
    let snap = store.snapshot();

    let q = parse_query(src).unwrap_or_else(|e| panic!("parse `{src}`: {e}"));
    let mut t = translate_query(&q).unwrap();
    t.root = rewrite(t.root.clone());

    let rows_of = |out: Output| match out {
        Output::Solutions { rows, .. } => rows.len(),
        other => panic!("expected solutions, got {other:?}"),
    };
    let reference = rows_of(evaluate_ref(&snap, &t).unwrap());
    let vectorized = rows_of(evaluate_with(&snap, &t, &ExecOptions::default()).unwrap());
    (reference, vectorized)
}

#[test]
fn prebound_values_graph_var_joins_enumeration() {
    let (reference, vectorized) = count_rows(
        "values",
        "select ?o { values ?g { <http://x/g> } graph ?g { ?s <http://x/p> ?o } }",
    );
    assert_eq!(reference, 1, "reference engine");
    assert_eq!(vectorized, 1, "vectorized engine");
}

#[test]
fn bind_derived_graph_var_joins_enumeration() {
    let (reference, vectorized) = count_rows(
        "bind",
        "select ?o { <http://x/g> <http://x/a> ?t . bind(iri(concat(str(<http://x/g>), \"\")) as ?g) graph ?g { ?s <http://x/p> ?o } }",
    );
    assert_eq!(reference, 1, "reference engine");
    assert_eq!(vectorized, 1, "vectorized engine");
}

#[test]
fn graph_var_binding_joins_registry_subject() {
    // ?g flows the other way: bound by enumeration first, then matched as a subject
    let (reference, vectorized) = count_rows(
        "flip",
        "select ?g { graph ?g { ?s <http://x/p> ?o } ?g <http://x/a> ?t }",
    );
    assert_eq!(reference, 1, "reference engine");
    assert_eq!(vectorized, 1, "vectorized engine");
}

#[test]
fn filter_on_graph_var_stays_outside_enumeration() {
    // regression: push_filters moved conditions referencing the graph VARIABLE inside the
    // GRAPH node, where the enumeration has not bound it yet — `filter(bound(?g))`
    // (and any str(?g) test) dropped every row
    let (reference, vectorized) = count_rows(
        "filter-bound",
        "select ?g { graph ?g { ?s <http://x/p> ?o } filter(bound(?g)) }",
    );
    assert_eq!(reference, 1, "reference engine");
    assert_eq!(vectorized, 1, "vectorized engine");

    let (reference, vectorized) = count_rows(
        "filter-strends",
        "select ?g { graph ?g { ?s <http://x/p> ?o } filter(strends(str(?g), \"/g\")) }",
    );
    assert_eq!(reference, 1, "reference engine");
    assert_eq!(vectorized, 1, "vectorized engine");
}
