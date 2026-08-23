//! Cross-position term identity through the delta layer: a term written by
//! an update must keep ONE public TermId whether it entered as subject or
//! object, and whether it already lived in the base's subject-only /
//! object-only sections — else joins through update-written bindings
//! silently return nothing (found via Flexo MMS's transaction-validation
//! CONSTRUCT returning empty against `graphy serve`).

use std::path::PathBuf;

use graphy_algebra::{rewrite, translate_query, translate_update};
use graphy_engine::{evaluate, execute_update, Output};
use graphy_sparql_syntax::{parse_query, parse_update};
use graphy_store::{BuilderConfig, Profile, SegmentBuilder, Store};

/// Base dataset (concise terms):
///   :alice :id "alice"     (alice: subject)
///   :policy :subject :alice (alice: object → shared section)
///   :bob :age 25            (bob: subject-only section)
///   :alice :likes :dave     (dave: object-only section)
fn build_base(dir: &PathBuf) {
    let iri = |s: &str| format!(">http://x/{s}").into_bytes();
    let mut cfg = BuilderConfig::new(dir);
    cfg.profile = Profile::Balanced;
    cfg.sort_budget = 1 << 14;
    let mut b = SegmentBuilder::new(cfg).unwrap();
    let quads: Vec<(Vec<u8>, Vec<u8>, Vec<u8>)> = vec![
        (iri("alice"), iri("id"), b"\"alice".to_vec()),
        (iri("policy"), iri("subject"), iri("alice")),
        (
            iri("bob"),
            iri("age"),
            b"^>http://www.w3.org/2001/XMLSchema#integer\"25".to_vec(),
        ),
        (iri("alice"), iri("likes"), iri("dave")),
    ];
    for (s, p, o) in &quads {
        b.push_quad(s, p, o, None).unwrap();
    }
    b.finish().unwrap();
}

fn update(store: &Store, src: &str) {
    let req = parse_update(src).unwrap_or_else(|e| panic!("parse `{src}`: {e}"));
    let tu = translate_update(&req).unwrap_or_else(|e| panic!("translate `{src}`: {e}"));
    execute_update(store, &tu).unwrap_or_else(|e| panic!("execute `{src}`: {e}"));
}

/// Run a query, rendering rows as sorted "var=term …" strings.
fn run(store: &Store, src: &str) -> Vec<String> {
    let q = parse_query(src).unwrap_or_else(|e| panic!("parse `{src}`: {e}"));
    let mut t = translate_query(&q).unwrap_or_else(|e| panic!("translate `{src}`: {e}"));
    t.root = rewrite(t.root.clone());
    let snap = store.snapshot();
    match evaluate(&snap, &t).unwrap_or_else(|e| panic!("evaluate `{src}`: {e}")) {
        Output::Solutions { vars, rows } => {
            let mut out: Vec<String> = rows
                .iter()
                .map(|r| {
                    vars.iter()
                        .zip(r)
                        .filter_map(|(v, cell)| {
                            cell.as_ref()
                                .map(|bytes| format!("{v}={}", String::from_utf8_lossy(bytes)))
                        })
                        .collect::<Vec<_>>()
                        .join(" ")
                })
                .collect();
            out.sort();
            out
        }
        Output::Boolean(b) => vec![format!("ask={b}")],
        other => panic!("expected solutions, got {other:?}"),
    }
}

fn scratch(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "graphy-engine-update-join-{tag}-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

const INT25: &str = "^>http://www.w3.org/2001/XMLSchema#integer\"25";
const INT40: &str = "^>http://www.w3.org/2001/XMLSchema#integer\"40";

fn assert_joins(store: &Store) {
    // Base subject-only term (:bob) written as object: object binding must
    // join back onto base subject triples.
    assert_eq!(
        run(
            store,
            "SELECT ?a WHERE { <http://x/txn> <http://x/user> ?u . ?u <http://x/age> ?a }"
        ),
        vec![format!("a={INT25}")]
    );
    // Delta-only term (:newpol) first seen as object, then as subject
    // within the same update.
    assert_eq!(
        run(
            store,
            "SELECT ?r WHERE { <http://x/txn2> <http://x/policy> ?p . ?p <http://x/role> ?r }"
        ),
        vec!["r=>http://x/r1".to_owned()]
    );
    // Delta-only term (:newgrp) first seen as subject, later as object.
    assert_eq!(
        run(
            store,
            "SELECT ?k WHERE { <http://x/txn3> <http://x/member> ?g . ?g <http://x/kind> ?k }"
        ),
        vec!["k=>http://x/grp".to_owned()]
    );
    // Base object-only term (:dave) written as subject.
    assert_eq!(
        run(
            store,
            "SELECT ?a WHERE { <http://x/alice> <http://x/likes> ?w . ?w <http://x/age> ?a }"
        ),
        vec![format!("a={INT40}")]
    );
    // Bound-constant pushdown of an aliased delta term.
    assert_eq!(
        run(
            store,
            "ASK { <http://x/txn2> <http://x/policy> <http://x/newpol> }"
        ),
        vec!["ask=true".to_owned()]
    );
}

#[test]
fn joins_through_delta_written_terms() {
    let dir = scratch("live");
    build_base(&dir);
    let store = Store::open(&dir).unwrap();

    update(
        &store,
        "INSERT DATA { <http://x/txn> <http://x/user> <http://x/bob> }",
    );
    update(
        &store,
        "INSERT DATA { <http://x/txn2> <http://x/policy> <http://x/newpol> . \
         <http://x/newpol> <http://x/role> <http://x/r1> }",
    );
    update(
        &store,
        "INSERT DATA { <http://x/newgrp> <http://x/kind> <http://x/grp> }",
    );
    update(
        &store,
        "INSERT DATA { <http://x/txn3> <http://x/member> <http://x/newgrp> }",
    );
    update(&store, "INSERT DATA { <http://x/dave> <http://x/age> 40 }");

    assert_joins(&store);

    // Same store through WAL replay: recovery re-interns via the same
    // aliasing path, so identities must survive a reopen.
    drop(store);
    let store = Store::open(&dir).unwrap();
    assert_joins(&store);
}
