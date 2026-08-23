//! Rewrite-pass tests (doc 04 §4): filter decomposition + pushdown,
//! constant folding under SPARQL error semantics, trivial eliminations,
//! BGP canonicalization, and well-designedness.

use graphy_algebra::{rewrite, to_sse, translate_query, well_designed};
use graphy_sparql_syntax::parse_query;

fn rewritten(src: &str) -> String {
    let q = parse_query(src).unwrap_or_else(|e| panic!("parse `{src}`: {e}"));
    let t = translate_query(&q).unwrap_or_else(|e| panic!("translate `{src}`: {e}"));
    to_sse(&rewrite(t.root), &t.vars)
}

fn assert_rw(src: &str, expected: &str) {
    let got = rewritten(src);
    let norm = |s: &str| {
        s.lines()
            .map(str::trim_end)
            .filter(|l| !l.trim().is_empty())
            .collect::<Vec<_>>()
            .join("\n")
    };
    assert_eq!(norm(&got), norm(expected), "for `{src}`\ngot:\n{got}");
}

#[test]
fn conjunction_splits_and_pushes_into_join_sides() {
    assert_rw(
        "SELECT ?a { ?a <http://p> ?x . GRAPH <http://g> { ?b <http://q> ?y } \
         FILTER(?x > 1 && ?y > 2) }",
        r#"
(project (?a)
  (join
    (filter (> ?x "1"^^<http://www.w3.org/2001/XMLSchema#integer>)
      (bgp
        (triple ?a <http://p> ?x)
      )
    )
    (graph <http://g>
      (filter (> ?y "2"^^<http://www.w3.org/2001/XMLSchema#integer>)
        (bgp
          (triple ?b <http://q> ?y)
        )
      )
    )
  )
)
"#,
    );
}

#[test]
fn filters_distribute_over_union_branches() {
    assert_rw(
        "ASK { { ?s <http://a> ?x } UNION { ?s <http://b> ?x } FILTER(?x = 1) }",
        r#"
(project (?s ?x)
  (union
    (filter (= ?x "1"^^<http://www.w3.org/2001/XMLSchema#integer>)
      (bgp
        (triple ?s <http://a> ?x)
      )
    )
    (filter (= ?x "1"^^<http://www.w3.org/2001/XMLSchema#integer>)
      (bgp
        (triple ?s <http://b> ?x)
      )
    )
  )
)
"#,
    );
}

#[test]
fn filter_pushes_into_leftjoin_left_but_never_right() {
    assert_rw(
        "ASK { ?s <http://p> ?x OPTIONAL { ?s <http://q> ?y } FILTER(?x > 1) }",
        r#"
(project (?s ?x ?y)
  (leftjoin
    (filter (> ?x "1"^^<http://www.w3.org/2001/XMLSchema#integer>)
      (bgp
        (triple ?s <http://p> ?x)
      )
    )
    (bgp
      (triple ?s <http://q> ?y)
    )
  )
)
"#,
    );
    // A condition over the optional side's variable must stay put.
    let s = rewritten("ASK { ?s <http://p> ?x OPTIONAL { ?s <http://q> ?y } FILTER(?y > 1) }");
    assert!(
        s.trim_start().starts_with("(project (?s ?x ?y)\n  (filter"),
        "{s}"
    );
}

#[test]
fn exists_and_nondeterministic_conjuncts_stay_put() {
    let s = rewritten(
        "ASK { ?s <http://p> ?x . ?s <http://q> ?y \
         FILTER(EXISTS { ?s <http://r> ?x } && ?y > 1) }",
    );
    // ?y > 1 sinks; EXISTS stays above the join.
    assert!(s.contains("(filter (exists"), "{s}");
    assert!(s.contains("(filter (> ?y"), "{s}");

    let s = rewritten("ASK { ?s <http://p> ?x . ?s <http://q> ?y FILTER(RAND() < ?x) }");
    assert!(
        s.trim_start()
            .starts_with("(project (?s ?x ?y)\n  (filter (< (rand)"),
        "{s}"
    );
}

#[test]
fn constant_folding_and_eliminations() {
    // FALSE filter annihilates the group.
    assert_rw(
        "SELECT ?s { ?s <http://p> ?o FILTER(1 > 2) }",
        r#"
(project (?s)
  (table (vars))
)
"#,
    );
    // TRUE filters vanish; arithmetic folds.
    assert_rw(
        "SELECT ?s { ?s <http://p> ?o FILTER(2 + 3 = 5) }",
        r#"
(project (?s)
  (bgp
    (triple ?s <http://p> ?o)
  )
)
"#,
    );
    // FALSE && error = FALSE (an erroring operand folds away safely).
    assert_rw(
        "SELECT ?s { ?s <http://p> ?o FILTER(1 = 2 && <http://x> = 1) }",
        r#"
(project (?s)
  (table (vars))
)
"#,
    );
    // TRUE || error = TRUE.
    assert_rw(
        "SELECT ?s { ?s <http://p> ?o FILTER(1 = 1 || <http://x> = 1) }",
        r#"
(project (?s)
  (bgp
    (triple ?s <http://p> ?o)
  )
)
"#,
    );
    // error && TRUE must NOT fold to TRUE (the row must still error out).
    let s = rewritten("SELECT ?s { ?s <http://p> ?o FILTER(<http://x> = 1 && 1 = 1) }");
    assert!(s.contains("(filter"), "{s}");
    // LIMIT 0 is empty at evaluation time but must retain the projected
    // variables for the result-set header.
    let s = rewritten("SELECT ?s { ?s <http://p> ?o } LIMIT 0");
    assert!(s.contains("(slice 0 0"), "{s}");
    assert!(s.contains("(project (?s)"), "{s}");
}

#[test]
fn bind_of_constant_folds_and_extends_survive() {
    assert_rw(
        "SELECT ?f { ?s <http://p> ?o BIND(2 * 21 AS ?f) }",
        r#"
(project (?f)
  (extend (?f "42"^^<http://www.w3.org/2001/XMLSchema#integer>)
    (bgp
      (triple ?s <http://p> ?o)
    )
  )
)
"#,
    );
}

#[test]
fn bgp_patterns_canonicalize() {
    // Same BGP written in two orders yields identical rewritten SSE.
    let a = rewritten("ASK { ?s <http://b> ?x . ?s <http://a> ?y }");
    let b = rewritten("ASK { ?s <http://a> ?y . ?s <http://b> ?x }");
    // Projection order differs (?x/?y first-mention), so compare BGPs.
    let bgp = |s: &str| {
        s.lines()
            .filter(|l| l.trim_start().starts_with("(triple"))
            .map(str::trim)
            .collect::<Vec<_>>()
            .join("\n")
    };
    assert_eq!(bgp(&a), bgp(&b), "{a}\nvs\n{b}");
}

#[test]
fn filter_does_not_cross_subquery_or_group_boundaries() {
    let s = rewritten(
        "SELECT ?s ?n { { SELECT ?s (COUNT(*) AS ?n) { ?s <http://p> ?o } GROUP BY ?s } \
         FILTER(?n > 3) }",
    );
    // The filter stays outside the ToMultiSet boundary.
    let filter_at = s.find("(filter").expect("filter present");
    let boundary_at = s.find("(tomultiset").expect("subquery boundary");
    assert!(filter_at < boundary_at, "{s}");
}

#[test]
fn well_designedness() {
    let check = |src: &str| {
        let q = parse_query(src).unwrap();
        let t = translate_query(&q).unwrap();
        well_designed(&t.root)
    };
    // Classic well-designed OPTIONAL.
    assert!(check(
        "ASK { ?x <http://a> ?y OPTIONAL { ?y <http://b> ?z } }"
    ));
    // Non-well-designed: ?z appears in the optional and elsewhere, but
    // not on the left side.
    assert!(!check(
        "ASK { ?x <http://a> ?y OPTIONAL { ?y <http://b> ?z } . ?z <http://c> ?w }"
    ));
    // Nested optionals that stay well-designed.
    assert!(check(
        "ASK { ?x <http://a> ?y OPTIONAL { ?y <http://b> ?z OPTIONAL { ?z <http://c> ?w } } }"
    ));
}

#[test]
fn rewrite_is_idempotent() {
    for src in [
        "SELECT ?a { ?a <http://p> ?x FILTER(?x > 1 && ?a != <http://z>) }",
        "ASK { { ?s <http://a> ?x } UNION { ?s <http://b> ?x } FILTER(?x = 1) }",
        "SELECT ?s (COUNT(*) AS ?n) { ?s <http://p> ?o } GROUP BY ?s HAVING(COUNT(*) > 2)",
    ] {
        let q = parse_query(src).unwrap();
        let t = translate_query(&q).unwrap();
        let once = rewrite(t.root.clone());
        let twice = rewrite(once.clone());
        assert_eq!(to_sse(&once, &t.vars), to_sse(&twice, &t.vars), "{src}");
    }
}

// ------------------------------------------------------- §M13d combinator

#[test]
fn transform_bottom_up_identity_and_custom_pass() {
    use graphy_algebra::{to_sse, transform_bottom_up, translate_query, Algebra};
    use graphy_sparql_syntax::parse_query;

    let q = parse_query(
        "SELECT ?s WHERE { ?s <http://x/p> ?o . FILTER(?o > 1) OPTIONAL { ?s <http://x/q> ?y } }",
    )
    .unwrap();
    let t = translate_query(&q).unwrap();

    // Identity reproduces the tree.
    let id = transform_bottom_up(t.root.clone(), &mut |a| a);
    assert_eq!(to_sse(&id, &t.vars), to_sse(&t.root, &t.vars));

    // A custom pass: drop every FILTER wrapper.
    let stripped = transform_bottom_up(t.root.clone(), &mut |a| match a {
        Algebra::Filter { input, .. } => *input,
        other => other,
    });
    let sse = to_sse(&stripped, &t.vars);
    assert!(!sse.contains("(filter"), "{sse}");
    assert!(sse.contains("(leftjoin"), "{sse}");
}
