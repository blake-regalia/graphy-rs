//! §18.2 translation tests: SSE goldens over the algebra shapes the spec
//! mandates — join/filter collection, OPTIONAL fusion, path
//! decomposition, aggregation extraction, subqueries, modifiers.

use graphy_algebra::{to_sse, translate_query, Algebra, Form, P};
use graphy_sparql_syntax::parse_query;

fn sse(src: &str) -> String {
    let q = parse_query(src).unwrap_or_else(|e| panic!("parse `{src}`: {e}"));
    let t = translate_query(&q).unwrap_or_else(|e| panic!("translate `{src}`: {e}"));
    to_sse(&t.root, &t.vars)
}

fn assert_sse(src: &str, expected: &str) {
    let got = sse(src);
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
fn bgp_and_projection() {
    assert_sse(
        "SELECT ?s WHERE { ?s <http://p> ?o }",
        r#"
(project (?s)
  (bgp
    (triple ?s <http://p> ?o)
  )
)
"#,
    );
}

#[test]
fn select_star_projects_visible_vars_in_order() {
    assert_sse(
        "SELECT * WHERE { ?b <http://p> ?a . ?a <http://q> ?c }",
        r#"
(project (?b ?a ?c)
  (bgp
    (triple ?b <http://p> ?a)
    (triple ?a <http://q> ?c)
  )
)
"#,
    );
}

#[test]
fn filters_collect_over_the_whole_group() {
    // Filter position within the group does not matter (§18.2.2.6).
    assert_sse(
        "SELECT ?s { FILTER(?o > 3) ?s <http://p> ?o . OPTIONAL { ?s <http://q> ?x } }",
        r#"
(project (?s)
  (filter (> ?o "3"^^<http://www.w3.org/2001/XMLSchema#integer>)
    (leftjoin
      (bgp
        (triple ?s <http://p> ?o)
      )
      (bgp
        (triple ?s <http://q> ?x)
      )
    )
  )
)
"#,
    );
}

#[test]
fn optional_filter_fuses_into_leftjoin() {
    assert_sse(
        "ASK { ?s <http://p> ?o OPTIONAL { ?s <http://q> ?x FILTER(?x != ?o) } }",
        r#"
(project (?s ?o ?x)
  (leftjoin (!= ?x ?o)
    (bgp
      (triple ?s <http://p> ?o)
    )
    (bgp
      (triple ?s <http://q> ?x)
    )
  )
)
"#,
    );
}

#[test]
fn union_minus_graph_bind_values() {
    assert_sse(
        "SELECT ?s { { ?s <http://a> 1 } UNION { ?s <http://b> 2 } \
         MINUS { ?s <http://c> 3 } \
         GRAPH ?g { ?s <http://d> ?e } \
         BIND(?e + 1 AS ?f) \
         VALUES ?v { <http://x> } }",
        // The in-group VALUES joins after the BIND extend (group order).
        r#"
(project (?s)
  (join
    (extend (?f (+ ?e "1"^^<http://www.w3.org/2001/XMLSchema#integer>))
      (join
        (minus
          (union
            (bgp
              (triple ?s <http://a> "1"^^<http://www.w3.org/2001/XMLSchema#integer>)
            )
            (bgp
              (triple ?s <http://b> "2"^^<http://www.w3.org/2001/XMLSchema#integer>)
            )
          )
          (bgp
            (triple ?s <http://c> "3"^^<http://www.w3.org/2001/XMLSchema#integer>)
          )
        )
        (graph ?g
          (bgp
            (triple ?s <http://d> ?e)
          )
        )
      )
    )
    (table (vars ?v)
      (row (?v <http://x>))
    )
  )
)
"#,
    );
    // The VALUES join lands after the BIND extend in group order.
    let s = sse("SELECT ?s { ?s <http://a> ?x VALUES ?v { <http://x> } }");
    assert!(s.contains("(table (vars ?v)"), "{s}");
}

#[test]
fn path_decomposition() {
    // seq splits with a fresh middle var; alt becomes union; * stays.
    assert_sse(
        "ASK { ?s <http://a>/<http://b> ?o }",
        r#"
(project (?s ?o)
  (bgp
    (triple ?s <http://a> ?.p0)
    (triple ?.p0 <http://b> ?o)
  )
)
"#,
    );
    assert_sse(
        "ASK { ?s <http://a>|^<http://b> ?o }",
        r#"
(project (?s ?o)
  (union
    (bgp
      (triple ?s <http://a> ?o)
    )
    (bgp
      (triple ?o <http://b> ?s)
    )
  )
)
"#,
    );
    assert_sse(
        "ASK { ?s <http://a>* ?o }",
        r#"
(project (?s ?o)
  (path ?s (path* <http://a>) ?o)
)
"#,
    );
    assert_sse(
        "ASK { ?s !(<http://a>|^<http://b>) ?o }",
        r#"
(project (?s ?o)
  (path ?s (notoneof <http://a> (reverse <http://b>)) ?o)
)
"#,
    );
}

#[test]
fn aggregation_extraction() {
    assert_sse(
        "SELECT ?s (SUM(?x) AS ?total) WHERE { ?s <http://p> ?x } \
         GROUP BY ?s HAVING(SUM(?x) > 10) ORDER BY DESC(?total)",
        // HAVING translates before the projection, so its aggregate gets
        // the first internal variable.
        r#"
(project (?s ?total)
  (order ((desc ?total))
    (extend (?total ?.agg1)
      (filter (> ?.agg0 "10"^^<http://www.w3.org/2001/XMLSchema#integer>)
        (group (?s) ((?.agg0 (sum ?x)) (?.agg1 (sum ?x)))
          (bgp
            (triple ?s <http://p> ?x)
          )
        )
      )
    )
  )
)
"#,
    );
}

#[test]
fn implicit_grouping_and_count_star() {
    assert_sse(
        "SELECT (COUNT(*) AS ?n) WHERE { ?s ?p ?o }",
        r#"
(project (?n)
  (extend (?n ?.agg0)
    (group () ((?.agg0 (count *)))
      (bgp
        (triple ?s ?p ?o)
      )
    )
  )
)
"#,
    );
}

#[test]
fn subquery_boundary_and_modifiers() {
    assert_sse(
        "SELECT ?s { { SELECT DISTINCT ?s { ?s <http://p> ?o } LIMIT 5 } } OFFSET 2 LIMIT 10",
        r#"
(slice 2 10
  (project (?s)
    (tomultiset
      (slice 0 5
        (distinct
          (project (?s)
            (bgp
              (triple ?s <http://p> ?o)
            )
          )
        )
      )
    )
  )
)
"#,
    );
}

#[test]
fn exists_carries_subtree_and_bnodes_are_internal_vars() {
    let s = sse("SELECT ?s { ?s <http://p> _:b FILTER EXISTS { _:c <http://q> ?s } }");
    assert!(s.contains("(exists"), "{s}");
    assert!(s.contains("?.b:"), "{s}");
    // Internal vars never leak into SELECT *.
    let s = sse("SELECT * { ?s <http://p> _:b }");
    assert!(s.contains("(project (?s)"), "{s}");
}

#[test]
fn construct_and_describe_forms() {
    let q = parse_query("CONSTRUCT { ?s <http://made> ?o } WHERE { ?o <http://by> ?s }").unwrap();
    let t = translate_query(&q).unwrap();
    let Form::Construct(template) = &t.form else {
        panic!()
    };
    assert_eq!(template.len(), 1);

    let q = parse_query("DESCRIBE * WHERE { ?a <http://p> ?b }").unwrap();
    let t = translate_query(&q).unwrap();
    let Form::Describe(targets) = &t.form else {
        panic!()
    };
    assert_eq!(targets.len(), 2);
    assert!(matches!(targets[0], P::Var(_)));
}

#[test]
fn dataset_clauses_and_aggregate_placement_errors() {
    let q = parse_query("SELECT ?s FROM <http://g> FROM NAMED <http://n> { ?s ?p ?o }").unwrap();
    let t = translate_query(&q).unwrap();
    assert_eq!(t.dataset.len(), 2);
    assert!(t.dataset[0].0 && !t.dataset[1].0);

    // Aggregates in WHERE filters are a translation error.
    let q = parse_query("SELECT ?s { ?s ?p ?o FILTER(SUM(?o) > 1) }").unwrap();
    let e = translate_query(&q).expect_err("aggregate in filter");
    assert!(e.message.contains("aggregate"), "{}", e.message);
}

#[test]
fn ground_triple_terms_encode_variable_ones_stay_structural() {
    let q = parse_query(
        "SELECT * { ?s <http://p> <<( <http://a> <http://b> 1 )>> . ?s <http://q> <<( ?x <http://b> 2 )>> }",
    )
    .unwrap();
    let t = translate_query(&q).unwrap();
    let Algebra::Project { input, .. } = &t.root else {
        panic!()
    };
    let Algebra::Bgp(ts) = &**input else {
        panic!("{input:?}")
    };
    assert!(matches!(ts[0].o, P::Term(_)));
    assert!(matches!(ts[1].o, P::Triple(_)));
}
