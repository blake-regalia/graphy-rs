//! Parser tests: query forms, group patterns, paths, expressions,
//! 1.2 constructs, expansion of syntactic sugar, and span-carrying errors.

use graphy_sparql_syntax::ast::*;
use graphy_sparql_syntax::{parse_query, parse_update};

fn parse(src: &str) -> Query {
    parse_query(src).unwrap_or_else(|e| panic!("parse `{src}`: {e}"))
}

fn triples(q: &Query) -> &[TriplePattern] {
    match &q.pattern.elements[..] {
        [GroupElement::Triples(t), ..] => t,
        other => panic!("expected triples first, got {other:?}"),
    }
}

fn iri(t: &Term) -> &str {
    match &t.kind {
        TermKind::Iri(i) => i,
        other => panic!("expected IRI, got {other:?}"),
    }
}

fn var(t: &Term) -> &str {
    match &t.kind {
        TermKind::Var(v) => v,
        other => panic!("expected var, got {other:?}"),
    }
}

#[test]
fn select_with_prefixes_and_base() {
    let q = parse(
        "BASE <http://example.org/base/>\n\
         PREFIX ex: <http://example.org/ns#>\n\
         PREFIX : <rel/>\n\
         SELECT ?s WHERE { ?s ex:p :o ; <also/rel> 42 }",
    );
    let QueryForm::Select(s) = &q.form else {
        panic!()
    };
    assert_eq!(s.projection, vec![Projection::Var("s".into())]);
    let t = triples(&q);
    assert_eq!(t.len(), 2);
    let Verb::Term(p0) = &t[0].p else { panic!() };
    assert_eq!(iri(p0), "http://example.org/ns#p");
    assert_eq!(iri(&t[0].o), "http://example.org/base/rel/o");
    let Verb::Term(p1) = &t[1].p else { panic!() };
    assert_eq!(iri(p1), "http://example.org/base/also/rel");
    match &t[1].o.kind {
        TermKind::Literal { lexical, kind } => {
            assert_eq!(lexical, "42");
            assert_eq!(
                kind,
                &LiteralKind::Typed("http://www.w3.org/2001/XMLSchema#integer".into())
            );
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn group_pattern_elements() {
    let q = parse(
        "SELECT * WHERE {\n\
           ?s ?p ?o .\n\
           OPTIONAL { ?s <p2> ?x }\n\
           FILTER(?x > 3)\n\
           { ?a <b> ?c } UNION { ?d <e> ?f }\n\
           MINUS { ?s <bad> ?y }\n\
           GRAPH ?g { ?s <in> ?g2 }\n\
           SERVICE SILENT <http://remote/> { ?s <r> ?v }\n\
           BIND(?x + 1 AS ?y2)\n\
           VALUES ?z { 1 2 UNDEF }\n\
         }",
    );
    let e = &q.pattern.elements;
    assert!(matches!(e[0], GroupElement::Triples(_)));
    assert!(matches!(e[1], GroupElement::Optional(_)));
    assert!(matches!(e[2], GroupElement::Filter(_)));
    match &e[3] {
        GroupElement::Union(branches) => assert_eq!(branches.len(), 2),
        other => panic!("{other:?}"),
    }
    assert!(matches!(e[4], GroupElement::Minus(_)));
    assert!(matches!(e[5], GroupElement::Graph(_, _)));
    match &e[6] {
        GroupElement::Service { silent, .. } => assert!(silent),
        other => panic!("{other:?}"),
    }
    match &e[7] {
        GroupElement::Bind { var, .. } => assert_eq!(var, "y2"),
        other => panic!("{other:?}"),
    }
    match &e[8] {
        GroupElement::Values(v) => {
            assert_eq!(v.vars, vec!["z"]);
            assert_eq!(v.rows.len(), 3);
            assert_eq!(v.rows[2], vec![None]);
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn property_paths() {
    let q = parse("SELECT * WHERE { ?s (^<p>/<q>)|<r>+ ?o . ?s !(<a>|^<b>) ?t . ?u a ?class }");
    let t = triples(&q);
    match &t[0].p {
        Verb::Path(Path::Alt(l, r)) => {
            assert!(matches!(**l, Path::Seq(_, _)));
            assert!(matches!(**r, Path::OneOrMore(_)));
        }
        other => panic!("{other:?}"),
    }
    match &t[1].p {
        Verb::Path(Path::Nps(items)) => {
            assert_eq!(items.len(), 2);
            assert!(!items[0].1 && items[1].1);
        }
        other => panic!("{other:?}"),
    }
    // Bare `a` and single IRIs collapse to plain terms, not paths.
    let Verb::Term(p2) = &t[2].p else { panic!() };
    assert_eq!(iri(p2), "http://www.w3.org/1999/02/22-rdf-syntax-ns#type");
}

#[test]
fn collections_and_bnode_lists_expand() {
    let q = parse("SELECT * WHERE { ?s <p> (1 2) . [ <q> ?v ] <r> ?w }");
    let t = triples(&q);
    // (1 2) → 4 first/rest triples; the object triple; then the bnode list.
    let firsts = t
        .iter()
        .filter(|tp| match &tp.p {
            Verb::Term(p) => iri(p).ends_with("#first"),
            _ => false,
        })
        .count();
    assert_eq!(firsts, 2);
    let rests = t
        .iter()
        .filter(|tp| match &tp.p {
            Verb::Term(p) => iri(p).ends_with("#rest"),
            _ => false,
        })
        .count();
    assert_eq!(rests, 2);
    // Fresh labels are collision-free (grammar cannot produce `.`-leading).
    let fresh: Vec<&TriplePattern> = t
        .iter()
        .filter(|tp| matches!(&tp.s.kind, TermKind::BlankNode(l) if l.starts_with('.')))
        .collect();
    assert!(!fresh.is_empty());
}

#[test]
fn sparql12_reification_desugars() {
    let q = parse("SELECT * WHERE { << ?s <p> ?o ~?r >> <said> ?who }");
    let t = triples(&q);
    // r rdf:reifies <<(s p o)>> plus the outer triple with subject r.
    assert_eq!(t.len(), 2);
    let Verb::Term(p0) = &t[0].p else { panic!() };
    assert_eq!(
        iri(p0),
        "http://www.w3.org/1999/02/22-rdf-syntax-ns#reifies"
    );
    assert_eq!(var(&t[0].s), "r");
    match &t[0].o.kind {
        TermKind::TripleTerm(inner) => {
            assert_eq!(var(&inner.s), "s");
            assert_eq!(var(&inner.o), "o");
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(var(&t[1].s), "r");

    // Annotation block: asserted triple, fresh-reifier rdf:reifies, then
    // the annotation with the reifier as subject.
    let q = parse("SELECT * WHERE { ?s <p> ?o {| <since> 2024 |} }");
    let t = triples(&q);
    assert_eq!(t.len(), 3);
    assert_eq!(var(&t[0].s), "s");
    let Verb::Term(p1) = &t[1].p else { panic!() };
    assert_eq!(
        iri(p1),
        "http://www.w3.org/1999/02/22-rdf-syntax-ns#reifies"
    );
    let Verb::Term(p2) = &t[2].p else { panic!() };
    assert_eq!(iri(p2), "since");
    match (&t[1].s.kind, &t[2].s.kind) {
        (TermKind::BlankNode(a), TermKind::BlankNode(b)) => {
            assert_eq!(a, b);
            assert!(a.starts_with('.'));
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn triple_terms_in_values_and_objects() {
    let q = parse("SELECT * WHERE { ?s <p> <<( <a> <b> 1 )>> }");
    let t = triples(&q);
    assert!(matches!(&t[0].o.kind, TermKind::TripleTerm(_)));
    let q = parse("SELECT * WHERE { ?s ?p ?o } VALUES ?t { <<( <a> <b> <c> )>> }");
    let v = q.values.unwrap();
    assert!(matches!(
        v.rows[0][0].as_ref().unwrap().kind,
        TermKind::TripleTerm(_)
    ));
}

#[test]
fn expressions_and_precedence() {
    let q = parse("SELECT * WHERE { ?s ?p ?o FILTER(?a || ?b && ?c = ?d + ?e * ?f) }");
    let GroupElement::Filter(e) = &q.pattern.elements[1] else {
        panic!()
    };
    // || at top.
    let ExprKind::Or(_, rhs) = &*e.kind else {
        panic!("{:?}", e.kind)
    };
    let ExprKind::And(_, rhs) = &*rhs.kind else {
        panic!()
    };
    let ExprKind::Cmp(CmpOp::Eq, _, rhs) = &*rhs.kind else {
        panic!()
    };
    let ExprKind::Add(_, rhs) = &*rhs.kind else {
        panic!()
    };
    assert!(matches!(&*rhs.kind, ExprKind::Mul(_, _)));
}

#[test]
fn builtins_aggregates_and_functions() {
    let q = parse(
        "PREFIX fn: <http://fns/>\n\
         SELECT (COUNT(*) AS ?n) (GROUP_CONCAT(DISTINCT ?x; SEPARATOR=\", \") AS ?g)\n\
         WHERE { ?s ?p ?x FILTER(REGEX(STR(?x), \"^a\", \"i\") && fn:custom(?x, 1)) }\n\
         GROUP BY ?s",
    );
    let QueryForm::Select(s) = &q.form else {
        panic!()
    };
    match &s.projection[0] {
        Projection::Expr(e, n) => {
            assert_eq!(n, "n");
            assert!(matches!(
                &*e.kind,
                ExprKind::Aggregate {
                    func: Aggregate::Count,
                    expr: None,
                    ..
                }
            ));
        }
        other => panic!("{other:?}"),
    }
    match &s.projection[1] {
        Projection::Expr(e, _) => match &*e.kind {
            ExprKind::Aggregate {
                func: Aggregate::GroupConcat,
                distinct,
                separator,
                ..
            } => {
                assert!(distinct);
                assert_eq!(separator.as_deref(), Some(", "));
            }
            other => panic!("{other:?}"),
        },
        other => panic!("{other:?}"),
    }
    let GroupElement::Filter(e) = &q.pattern.elements[1] else {
        panic!()
    };
    let ExprKind::And(l, r) = &*e.kind else {
        panic!()
    };
    assert!(matches!(&*l.kind, ExprKind::Builtin(Builtin::Regex, args) if args.len() == 3));
    assert!(
        matches!(&*r.kind, ExprKind::Function { iri, args, .. } if iri == "http://fns/custom" && args.len() == 2)
    );
}

#[test]
fn exists_and_in() {
    let q = parse(
        "SELECT * WHERE { ?s ?p ?o \
         FILTER(EXISTS { ?s <q> ?o } && ?o NOT IN (1, 2) && ?p IN ()) }",
    );
    let GroupElement::Filter(e) = &q.pattern.elements[1] else {
        panic!()
    };
    let ExprKind::And(l, r) = &*e.kind else {
        panic!()
    };
    let ExprKind::And(ll, lr) = &*l.kind else {
        panic!()
    };
    assert!(matches!(&*ll.kind, ExprKind::Exists(_)));
    assert!(matches!(
        &*lr.kind,
        ExprKind::In { negated: true, list, .. } if list.len() == 2
    ));
    assert!(matches!(
        &*r.kind,
        ExprKind::In { negated: false, list, .. } if list.is_empty()
    ));
}

#[test]
fn subselect_and_modifiers() {
    let q = parse(
        "SELECT ?s WHERE { { SELECT DISTINCT ?s WHERE { ?s ?p ?o } ORDER BY ?s LIMIT 5 } }\n\
         GROUP BY ?s HAVING(COUNT(?s) > 1) ORDER BY DESC(?s) OFFSET 2 LIMIT 10",
    );
    // `{ SELECT … }` is a one-branch GroupOrUnion wrapping the subselect
    // group (the AST preserves the grammar shape).
    match &q.pattern.elements[0] {
        GroupElement::Union(branches) if branches.len() == 1 => match &branches[0].elements[0] {
            GroupElement::SubSelect(sub) => {
                assert!(sub.select.distinct);
                assert_eq!(sub.modifiers.limit, Some(5));
                assert_eq!(sub.modifiers.order_by.len(), 1);
            }
            other => panic!("{other:?}"),
        },
        other => panic!("{other:?}"),
    }
    assert_eq!(q.modifiers.group_by.len(), 1);
    assert_eq!(q.modifiers.having.len(), 1);
    assert!(q.modifiers.order_by[0].descending);
    assert_eq!(q.modifiers.limit, Some(10));
    assert_eq!(q.modifiers.offset, Some(2));
}

#[test]
fn construct_describe_ask() {
    let q = parse("CONSTRUCT { ?s <made> ?o } WHERE { ?o <by> ?s }");
    let QueryForm::Construct(template) = &q.form else {
        panic!()
    };
    assert_eq!(template.len(), 1);

    // Short form: template = pattern.
    let q = parse("CONSTRUCT WHERE { ?s <p> ?o }");
    let QueryForm::Construct(template) = &q.form else {
        panic!()
    };
    assert_eq!(template.len(), 1);
    assert_eq!(triples(&q).len(), 1);

    let q = parse("DESCRIBE ?s <http://thing/> WHERE { ?s a <T> }");
    let QueryForm::Describe { targets, star } = &q.form else {
        panic!()
    };
    assert!(!star);
    assert_eq!(targets.len(), 2);

    let q = parse("ASK { ?s ?p ?o }");
    assert!(matches!(q.form, QueryForm::Ask));

    let q = parse("PREFIX ex: <http://x/> ASK FROM <http://g/> { ?s ex:p ?o }");
    assert_eq!(q.dataset, vec![DatasetClause::Default("http://g/".into())]);
}

#[test]
fn version_declaration() {
    let q = parse("VERSION \"1.2\" SELECT * WHERE { ?s ?p ?o }");
    assert_eq!(q.version.as_deref(), Some("1.2"));
}

#[test]
fn values_full_form() {
    let q = parse("SELECT * WHERE { ?s ?p ?o } VALUES (?a ?b) { (1 \"x\") (UNDEF <http://i/>) }");
    let v = q.values.unwrap();
    assert_eq!(v.vars, vec!["a", "b"]);
    assert_eq!(v.rows.len(), 2);
    assert!(v.rows[1][0].is_none());
}

#[test]
fn errors() {
    for (src, what) in [
        ("SELECT ?s WHERE { ?s ex:p ?o }", "undeclared prefix"),
        ("SELECT * WHERE { ?s <p> ?o } trailing", "unknown keyword"),
        ("SELECT * WHERE { ?s <p> ?o } <extra>", "trailing input"),
        ("SELECT *", "expected `{`"),
        ("ASK { _:b <p> 1 . OPTIONAL {} _:b <q> 2 }", "reused across"),
        (
            "SELECT * WHERE { ?s <p> ?o FILTER(REGEX(?o)) }",
            "wrong number of arguments",
        ),
        ("SELECT * WHERE { ?s <p>/ ?o }", "expected a property path"),
        (
            "SELECT * WHERE { ?s ?p ?o } LIMIT 1 LIMIT 2",
            "duplicate LIMIT",
        ),
        (
            "SELECT * WHERE { ?s ?p ?o } VALUES (?a ?b) { (1) }",
            "row arity",
        ),
    ] {
        let e = parse_query(src).expect_err(src);
        assert!(
            e.message.contains(what),
            "`{src}` → `{}` (wanted `{what}`)",
            e.message
        );
    }
}

#[test]
fn depth_bomb_is_an_error_not_a_crash() {
    let mut src = String::from("SELECT * WHERE { ?s <p> ");
    for _ in 0..500 {
        src.push_str("[ <q> ");
    }
    src.push('1');
    for _ in 0..500 {
        src.push_str(" ]");
    }
    src.push_str(" }");
    let e = parse_query(&src).expect_err("depth bomb");
    assert!(e.message.contains("deeply nested"), "{}", e.message);

    let mut src = String::from("SELECT * WHERE { ?s ?p ?o FILTER(");
    src.push_str(&"(".repeat(500));
    src.push('1');
    src.push_str(&")".repeat(500));
    src.push_str(") }");
    let e = parse_query(&src).expect_err("paren bomb");
    assert!(e.message.contains("deeply nested"), "{}", e.message);
}

#[test]
fn bnode_labels_ok_within_one_bgp() {
    // Same label twice in one run of triples is fine.
    parse("SELECT * WHERE { _:b <p> 1 . _:b <q> 2 }");
    // Anonymous bnodes never collide with user labels.
    parse("SELECT * WHERE { [] <p> _:b . [] <q> _:b }");
}

#[test]
fn update_operations() {
    let u = parse_update(
        "PREFIX ex: <http://x/>\n\
         INSERT DATA { ex:s ex:p 1 . GRAPH ex:g { ex:s ex:q _:b } } ;\n\
         WITH ex:g DELETE { ?s ex:old ?o } INSERT { ?s ex:new ?o } USING NAMED ex:u WHERE { ?s ex:old ?o } ;\n\
         DELETE WHERE { ?s ex:gone ?o } ;\n\
         LOAD SILENT ex:src INTO GRAPH ex:dst ;\n\
         CLEAR NAMED ; DROP DEFAULT ; CREATE GRAPH ex:new ;\n\
         COPY DEFAULT TO GRAPH ex:g2",
    )
    .unwrap();
    assert_eq!(u.operations.len(), 8);
    match &u.operations[0] {
        UpdateOp::InsertData(quads) => {
            assert_eq!(quads.len(), 2);
            assert!(quads[0].graph.is_none());
            assert!(quads[1].graph.is_some());
            assert!(matches!(quads[1].triple.o.kind, TermKind::BlankNode(_)));
        }
        other => panic!("{other:?}"),
    }
    match &u.operations[1] {
        UpdateOp::Modify {
            with,
            delete,
            insert,
            using,
            ..
        } => {
            assert_eq!(with.as_deref(), Some("http://x/g"));
            assert!(delete.is_some() && insert.is_some());
            assert_eq!(using, &vec![DatasetClause::Named("http://x/u".into())]);
        }
        other => panic!("{other:?}"),
    }
    assert!(matches!(&u.operations[2], UpdateOp::DeleteWhere(q) if q.len() == 1));
    assert!(matches!(
        &u.operations[3],
        UpdateOp::Load {
            silent: true,
            into: Some(_),
            ..
        }
    ));
    assert!(matches!(
        &u.operations[4],
        UpdateOp::Clear {
            target: GraphTarget::Named,
            ..
        }
    ));
    assert!(matches!(
        &u.operations[5],
        UpdateOp::Drop {
            target: GraphTarget::Default,
            ..
        }
    ));
    assert!(matches!(&u.operations[6], UpdateOp::Create { .. }));
    assert!(matches!(
        &u.operations[7],
        UpdateOp::Copy {
            from: GraphOrDefault::Default,
            to: GraphOrDefault::Graph(_),
            ..
        }
    ));
}

#[test]
fn update_groundedness_rules() {
    // Variables are illegal in ground data.
    assert!(parse_update("INSERT DATA { ?s <p> 1 }").is_err());
    // Blank nodes: allowed in INSERT DATA, not in DELETE DATA/WHERE.
    assert!(parse_update("INSERT DATA { _:b <p> 1 }").is_ok());
    assert!(parse_update("DELETE DATA { _:b <p> 1 }").is_err());
    assert!(parse_update("DELETE WHERE { _:b <p> ?o }").is_err());
    // Label reuse across operations is illegal; within one is fine.
    assert!(parse_update("INSERT DATA { _:b <p> 1 . _:b <q> 2 }").is_ok());
    assert!(parse_update("INSERT DATA { _:b <p> 1 } ; INSERT DATA { _:b <p> 2 }").is_err());
    // A bare prologue is a valid empty request.
    assert_eq!(
        parse_update("PREFIX ex: <http://x/>")
            .unwrap()
            .operations
            .len(),
        0
    );
}
