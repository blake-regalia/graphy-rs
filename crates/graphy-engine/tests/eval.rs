//! End-to-end reference-evaluator tests: parse → translate → rewrite →
//! evaluate against a real store, asserting solution multisets in
//! N-Triples-ish surface form.

use std::path::PathBuf;

use graphy_algebra::{rewrite, translate_query, TranslatedQuery};
use graphy_engine::{evaluate, Output};
use graphy_sparql_syntax::parse_query;
use graphy_store::{BuilderConfig, Profile, SegmentBuilder, Store};

/// Test dataset (concise terms):
///   default graph:
///     :alice :knows :bob ;      :age 30 ; :name "Alice"
///     :bob   :knows :carol ;    :age 25 ; :name "Bob"@en
///     :carol :age  35 ;         :name "Carol"
///     :alice :likes :carol
///   graph :g1:
///     :dave :knows :alice ;     :age 40
fn build_store(dir: &PathBuf) -> Store {
    let iri = |s: &str| format!(">http://x/{s}").into_bytes();
    let int = |i: i64| format!("^>http://www.w3.org/2001/XMLSchema#integer\"{i}").into_bytes();
    let lit = |s: &str| format!("\"{s}").into_bytes();
    let mut cfg = BuilderConfig::new(dir);
    cfg.profile = Profile::Balanced;
    cfg.sort_budget = 1 << 14;
    let mut b = SegmentBuilder::new(cfg).unwrap();
    type CQuad = (Vec<u8>, Vec<u8>, Vec<u8>, Option<Vec<u8>>);
    let quads: Vec<CQuad> = vec![
        (iri("alice"), iri("knows"), iri("bob"), None),
        (iri("alice"), iri("age"), int(30), None),
        (iri("alice"), iri("name"), lit("Alice"), None),
        (iri("alice"), iri("likes"), iri("carol"), None),
        (iri("bob"), iri("knows"), iri("carol"), None),
        (iri("bob"), iri("age"), int(25), None),
        (iri("bob"), iri("name"), b"@en\"Bob".to_vec(), None),
        (iri("carol"), iri("age"), int(35), None),
        (iri("carol"), iri("name"), lit("Carol"), None),
        (iri("dave"), iri("knows"), iri("alice"), Some(iri("g1"))),
        (iri("dave"), iri("age"), int(40), Some(iri("g1"))),
    ];
    for (s, p, o, g) in &quads {
        b.push_quad(s, p, o, g.as_deref()).unwrap();
    }
    b.finish().unwrap();
    Store::open(dir).unwrap()
}

fn store() -> &'static Store {
    use std::sync::OnceLock;
    static STORE: OnceLock<Store> = OnceLock::new();
    STORE.get_or_init(|| {
        let dir = std::env::temp_dir().join(format!("graphy-engine-eval-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        build_store(&dir)
    })
}

fn translated(src: &str) -> TranslatedQuery {
    let q = parse_query(src).unwrap_or_else(|e| panic!("parse `{src}`: {e}"));
    let mut t = translate_query(&q).unwrap_or_else(|e| panic!("translate `{src}`: {e}"));
    t.root = rewrite(t.root.clone());
    t
}

/// Run and render rows as sorted "var=term …" strings (multiset via
/// sorted list).
fn run(src: &str) -> Vec<String> {
    let t = translated(src);
    let snap = store().snapshot();
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
        other => panic!("expected solutions, got {other:?}"),
    }
}

fn ask(src: &str) -> bool {
    let t = translated(src);
    let snap = store().snapshot();
    match evaluate(&snap, &t).unwrap() {
        Output::Boolean(b) => b,
        other => panic!("expected boolean, got {other:?}"),
    }
}

#[test]
fn bgp_join_across_positions() {
    // ?x appears as object (knows) and subject (age): the join runs in
    // TermId space across position-local column spaces.
    let rows =
        run("SELECT ?x ?a WHERE { <http://x/alice> <http://x/knows> ?x . ?x <http://x/age> ?a }");
    assert_eq!(
        rows,
        vec!["x=>http://x/bob a=^>http://www.w3.org/2001/XMLSchema#integer\"25"]
    );
}

#[test]
fn filters_and_arithmetic() {
    let rows = run("SELECT ?s WHERE { ?s <http://x/age> ?a FILTER(?a > 28 && ?a < 36) }");
    assert_eq!(rows, vec!["s=>http://x/alice", "s=>http://x/carol"]);
    let rows = run("SELECT ?s WHERE { ?s <http://x/age> ?a FILTER(?a + 5 = 30) }");
    assert_eq!(rows, vec!["s=>http://x/bob"]);
}

#[test]
fn optional_union_minus_bind_values() {
    // OPTIONAL keeps carol (no :knows) with unbound ?y.
    let rows =
        run("SELECT ?s ?y WHERE { ?s <http://x/age> ?a OPTIONAL { ?s <http://x/knows> ?y } }");
    assert_eq!(rows.len(), 3);
    assert!(rows.iter().any(|r| r == "s=>http://x/carol"), "{rows:?}");

    let rows = run(
        "SELECT ?s WHERE { { ?s <http://x/knows> <http://x/bob> } UNION { ?s <http://x/knows> <http://x/carol> } }",
    );
    assert_eq!(rows, vec!["s=>http://x/alice", "s=>http://x/bob"]);

    let rows = run("SELECT ?s WHERE { ?s <http://x/age> ?a MINUS { ?s <http://x/knows> ?y } }");
    assert_eq!(rows, vec!["s=>http://x/carol"]);

    let rows = run("SELECT ?n WHERE { <http://x/bob> <http://x/age> ?a BIND(?a * 2 AS ?n) }");
    assert_eq!(
        rows,
        vec!["n=^>http://www.w3.org/2001/XMLSchema#integer\"50"]
    );

    let rows = run(
        "SELECT ?s WHERE { ?s <http://x/age> ?a } VALUES ?s { <http://x/bob> <http://x/nobody> }",
    );
    assert_eq!(rows, vec!["s=>http://x/bob"]);
}

#[test]
fn string_builtins_and_regex() {
    let rows =
        run("SELECT ?s WHERE { ?s <http://x/name> ?n FILTER(REGEX(STR(?n), \"^[AC]\", \"\")) }");
    assert_eq!(rows, vec!["s=>http://x/alice", "s=>http://x/carol"]);
    let rows = run("SELECT ?u WHERE { <http://x/alice> <http://x/name> ?n BIND(UCASE(?n) AS ?u) }");
    assert_eq!(rows, vec!["u=\"ALICE"]);
    // LANG + language-tagged compare.
    let rows = run("SELECT ?s WHERE { ?s <http://x/name> ?n FILTER(LANG(?n) = \"en\") }");
    assert_eq!(rows, vec!["s=>http://x/bob"]);
}

#[test]
fn aggregation() {
    let rows = run("SELECT (COUNT(*) AS ?n) (AVG(?a) AS ?avg) WHERE { ?s <http://x/age> ?a }");
    assert_eq!(rows.len(), 1);
    assert!(
        rows[0].contains("n=^>http://www.w3.org/2001/XMLSchema#integer\"3"),
        "{rows:?}"
    );
    assert!(
        rows[0].contains("avg=^>http://www.w3.org/2001/XMLSchema#decimal\"30"),
        "{rows:?}"
    );

    let rows = run("SELECT ?s (COUNT(?y) AS ?n) WHERE { ?s <http://x/knows> ?y } GROUP BY ?s");
    assert_eq!(rows.len(), 2);
    assert!(rows.iter().all(|r| r.contains("\"1")), "{rows:?}");

    let rows = run(
        "SELECT (GROUP_CONCAT(?n; SEPARATOR=\"|\") AS ?all) WHERE { ?s <http://x/name> ?n } ORDER BY ?n",
    );
    assert_eq!(rows.len(), 1);
    // Concatenation of all three names (input order not guaranteed
    // through grouping — check membership).
    for name in ["Alice", "Bob", "Carol"] {
        assert!(rows[0].contains(name), "{rows:?}");
    }
}

#[test]
fn order_distinct_slice() {
    // Top-2 ages in the default graph (dave's 40 is in :g1, out of scope).
    let rows = run("SELECT ?a WHERE { ?s <http://x/age> ?a } ORDER BY DESC(?a) LIMIT 2");
    assert_eq!(rows.len(), 2);
    assert!(
        rows.iter().any(|r| r.contains("\"35")) && rows.iter().any(|r| r.contains("\"30")),
        "{rows:?}"
    );
    let t = translated("SELECT ?a WHERE { ?s <http://x/age> ?a } ORDER BY ?a");
    let snap = store().snapshot();
    let Output::Solutions { rows, .. } = evaluate(&snap, &t).unwrap() else {
        panic!()
    };
    let vals: Vec<String> = rows
        .iter()
        .map(|r| String::from_utf8_lossy(r[0].as_ref().unwrap()).into_owned())
        .collect();
    assert!(vals.windows(2).all(|w| w[0] <= w[1]), "{vals:?}");

    let rows = run("SELECT DISTINCT ?p WHERE { ?s ?p ?o }");
    assert_eq!(rows.len(), 4); // knows, age, name, likes
}

#[test]
fn graphs() {
    // Default-graph BGPs must not see :g1.
    assert!(!ask("ASK { <http://x/dave> <http://x/age> ?a }"));
    // GRAPH <g1> scopes in.
    let rows = run("SELECT ?s WHERE { GRAPH <http://x/g1> { ?s <http://x/knows> ?o } }");
    assert_eq!(rows, vec!["s=>http://x/dave"]);
    // GRAPH ?g binds the graph and joins patterns within one graph.
    let rows =
        run("SELECT ?g ?s WHERE { GRAPH ?g { ?s <http://x/knows> ?o . ?s <http://x/age> ?a } }");
    assert_eq!(rows, vec!["g=>http://x/g1 s=>http://x/dave"]);
}

#[test]
fn paths() {
    // knows+ from alice reaches bob and carol.
    let rows = run("SELECT ?x WHERE { <http://x/alice> <http://x/knows>+ ?x }");
    assert_eq!(rows, vec!["x=>http://x/bob", "x=>http://x/carol"]);
    // knows* includes alice herself.
    let rows = run("SELECT ?x WHERE { <http://x/alice> <http://x/knows>* ?x }");
    assert_eq!(rows.len(), 3);
    // Inverse closure: who reaches carol.
    let rows = run("SELECT ?x WHERE { ?x <http://x/knows>+ <http://x/carol> }");
    assert_eq!(rows, vec!["x=>http://x/alice", "x=>http://x/bob"]);
    // Sequence decomposed into BGP (knows/name).
    let rows = run("SELECT ?n WHERE { <http://x/alice> <http://x/knows>/<http://x/name> ?n }");
    assert_eq!(rows, vec!["n=@en\"Bob"]);
    // Negated property set: alice's non-knows edges.
    let rows = run("SELECT ?o WHERE { <http://x/alice> !(<http://x/knows>|<http://x/age>|<http://x/name>) ?o }");
    assert_eq!(rows, vec!["o=>http://x/carol"]);
}

#[test]
fn exists_and_subquery() {
    let rows =
        run("SELECT ?s WHERE { ?s <http://x/age> ?a FILTER EXISTS { ?s <http://x/knows> ?y } }");
    assert_eq!(rows, vec!["s=>http://x/alice", "s=>http://x/bob"]);
    let rows = run(
        "SELECT ?s WHERE { ?s <http://x/age> ?a FILTER NOT EXISTS { ?s <http://x/knows> ?y } }",
    );
    assert_eq!(rows, vec!["s=>http://x/carol"]);

    let rows = run("SELECT ?s ?m WHERE { ?s <http://x/knows> ?o \
         { SELECT (MAX(?a) AS ?m) WHERE { ?x <http://x/age> ?a } } }");
    assert_eq!(rows.len(), 2);
    assert!(rows[0].contains("m=^>http://www.w3.org/2001/XMLSchema#integer\"35"));
}

#[test]
fn ask_and_construct() {
    assert!(ask(
        "ASK { <http://x/alice> <http://x/knows> <http://x/bob> }"
    ));
    assert!(!ask(
        "ASK { <http://x/bob> <http://x/knows> <http://x/alice> }"
    ));

    let t = translated("CONSTRUCT { ?s <http://x/knownBy> ?o } WHERE { ?o <http://x/knows> ?s }");
    let snap = store().snapshot();
    let Output::Triples(triples) = evaluate(&snap, &t).unwrap() else {
        panic!()
    };
    assert_eq!(triples.len(), 2);
    assert!(triples.iter().all(|(_, p, _)| p == b">http://x/knownBy"));
}

#[test]
fn computed_terms_join_back_to_store_terms() {
    // STRLEN etc. produce Ext terms; a computed value equal to a store
    // term must intern back to the store id (sameTerm/DISTINCT correct).
    let rows = run(
        "SELECT ?s WHERE { ?s <http://x/age> ?a BIND(25 + 10 AS ?b) FILTER(?b = 35 && ?a = ?b) }",
    );
    assert_eq!(rows, vec!["s=>http://x/carol"]);
}

#[test]
fn describe_cbd() {
    // DESCRIBE <iri>: every outgoing triple of the resource in the
    // default graph.
    let t = translated("DESCRIBE <http://x/alice>");
    let snap = store().snapshot();
    let Output::Triples(triples) = evaluate(&snap, &t).unwrap() else {
        panic!()
    };
    assert_eq!(triples.len(), 4);
    assert!(triples.iter().all(|(s, _, _)| s == b">http://x/alice"));

    // DESCRIBE ?v WHERE { … }: describes every binding of the target.
    let t = translated("DESCRIBE ?w WHERE { <http://x/alice> <http://x/knows> ?w }");
    let Output::Triples(triples) = evaluate(&snap, &t).unwrap() else {
        panic!()
    };
    // bob's three outgoing triples.
    assert_eq!(triples.len(), 3);
    assert!(triples.iter().all(|(s, _, _)| s == b">http://x/bob"));

    // Reference and vectorized engines agree.
    let t = translated("DESCRIBE ?w WHERE { ?w <http://x/age> ?a FILTER(?a > 28) }");
    let a = graphy_engine::evaluate_ref(&snap, &t).unwrap();
    let b = graphy_engine::exec::evaluate_vec(&snap, &t).unwrap();
    let norm = |o: Output| {
        let Output::Triples(mut ts) = o else { panic!() };
        ts.sort();
        ts
    };
    assert_eq!(norm(a), norm(b));
}
