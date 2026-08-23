//! §M13a printer tests: expressions, paths, and terms print with exact
//! precedence-driven parenthesization and prologue-aware pnames, and the
//! output is a fixpoint — parse(print(x)) prints back byte-identically
//! (span-insensitive round-trip; the corpus-wide gates are §M13c).

use graphy_sparql_syntax::ast::{GroupElement, Projection, QueryForm};
use graphy_sparql_syntax::{parse_query, print_expr, print_term, Printer};

const PROLOGUE: &str = "PREFIX ex: <http://ex/> PREFIX exa: <http://ex/a/> ";

/// Parse a query, pull the first FILTER expression, print it.
fn filter_expr(body: &str) -> String {
    let src = format!("{PROLOGUE}SELECT * WHERE {{ ?s ?p ?o . FILTER({body}) }}");
    let q = parse_query(&src).expect("parse");
    let e = q
        .pattern
        .elements
        .iter()
        .find_map(|el| match el {
            GroupElement::Filter(e) => Some(e),
            _ => None,
        })
        .expect("filter");
    print_expr(e, &q.prefixes)
}

/// Assert the printed form, then that it is a print fixpoint.
fn check_expr(body: &str, want: &str) {
    let got = filter_expr(body);
    assert_eq!(got, want, "print of {body:?}");
    assert_eq!(filter_expr(&got), want, "fixpoint of {body:?}");
}

/// Parse a triple, print its verb (term or path — `Printer::verb` owns
/// the `a` shorthand either way).
fn verb_path(path: &str) -> String {
    let src = format!("{PROLOGUE}SELECT * WHERE {{ ?s {path} ?o }}");
    let q = parse_query(&src).expect("parse");
    let Some(GroupElement::Triples(ts)) = q.pattern.elements.first() else {
        panic!("triples");
    };
    let mut p = Printer::new(&q.prefixes);
    p.verb(&ts[0].p);
    p.finish()
}

fn check_path(path: &str, want: &str) {
    let got = verb_path(path);
    assert_eq!(got, want, "print of {path:?}");
    assert_eq!(verb_path(&got), want, "fixpoint of {path:?}");
}

/// Parse a triple, print its object term.
fn object_term(object: &str) -> String {
    let src = format!("{PROLOGUE}SELECT * WHERE {{ ?s ex:p {object} }}");
    let q = parse_query(&src).expect("parse");
    let Some(GroupElement::Triples(ts)) = q.pattern.elements.first() else {
        panic!("triples");
    };
    print_term(&ts[0].o, &q.prefixes)
}

fn check_term(object: &str, want: &str) {
    let got = object_term(object);
    assert_eq!(got, want, "print of {object:?}");
    assert_eq!(object_term(&got), want, "fixpoint of {object:?}");
}

// ------------------------------------------------------------ expressions

#[test]
fn arithmetic_precedence() {
    check_expr("1 + 2 * 3", "1 + 2 * 3");
    check_expr("(1 + 2) * 3", "(1 + 2) * 3");
    check_expr("1 - 2 - 3", "1 - 2 - 3");
    check_expr("1 - (2 - 3)", "1 - (2 - 3)");
    check_expr("2 * 3 / 4", "2 * 3 / 4");
    check_expr("2 * (3 / 4)", "2 * (3 / 4)");
}

#[test]
fn logical_and_relational() {
    check_expr(
        "?x = ?y || ?z < 3 && ?w >= 4",
        "?x = ?y || ?z < 3 && ?w >= 4",
    );
    check_expr("(?x || ?y) && ?z", "(?x || ?y) && ?z");
    check_expr("(?x = ?y) = true", "(?x = ?y) = true");
    check_expr("?x != ?y", "?x != ?y");
    check_expr("(?x + 1) > (?y - 2)", "?x + 1 > ?y - 2");
}

#[test]
fn unary() {
    check_expr("!?x", "!?x");
    check_expr("!(?x || ?y)", "!(?x || ?y)");
    check_expr("!(!?x)", "!(!?x)");
    check_expr("-?x", "-?x");
    check_expr("- 2", "- 2");
    check_expr("-(?x + 1)", "-(?x + 1)");
    check_expr("!BOUND(?x)", "!BOUND(?x)");
}

#[test]
fn in_lists() {
    check_expr("?x IN (1, 2, ex:c)", "?x IN (1, 2, ex:c)");
    check_expr("?x NOT IN ()", "?x NOT IN ()");
    check_expr("?x + 1 IN (2)", "?x + 1 IN (2)");
}

#[test]
fn builtins_and_functions() {
    check_expr("REGEX(?x, \"^a\", \"i\")", "REGEX(?x, \"^a\", \"i\")");
    check_expr("IF(?x, 1, 2)", "IF(?x, 1, 2)");
    check_expr("COALESCE()", "COALESCE()");
    check_expr("sameterm(?x, ?y)", "sameTerm(?x, ?y)");
    check_expr("isiri(?x)", "isIRI(?x)");
    check_expr("ex:f(?x, 2)", "ex:f(?x, 2)");
    check_expr("ENCODE_FOR_URI(?x)", "ENCODE_FOR_URI(?x)");
}

#[test]
fn exists_bodies() {
    check_expr("EXISTS { ?s ex:p ?o }", "EXISTS { ?s ex:p ?o . }");
    check_expr(
        "NOT EXISTS { ?s a ex:T . FILTER(?o > 1) }",
        "NOT EXISTS { ?s a ex:T . FILTER(?o > 1) }",
    );
}

#[test]
fn aggregates_in_projection() {
    let src = format!(
        "{PROLOGUE}SELECT (COUNT(*) AS ?n) (GROUP_CONCAT(DISTINCT ?x; SEPARATOR = \", \") AS ?g) \
         WHERE {{ ?s ?p ?x }} GROUP BY ?s"
    );
    let q = parse_query(&src).expect("parse");
    let QueryForm::Select(sc) = &q.form else {
        panic!("select");
    };
    let printed: Vec<String> = sc
        .projection
        .iter()
        .map(|p| match p {
            Projection::Expr(e, _) => print_expr(e, &q.prefixes),
            Projection::Var(v) => format!("?{v}"),
        })
        .collect();
    assert_eq!(printed[0], "COUNT(*)");
    assert_eq!(printed[1], "GROUP_CONCAT(DISTINCT ?x; SEPARATOR = \", \")");
}

// ------------------------------------------------------------------ paths

#[test]
fn path_precedence() {
    check_path("ex:a/ex:b|ex:c", "ex:a/ex:b|ex:c");
    check_path("(ex:a|ex:b)/ex:c", "(ex:a|ex:b)/ex:c");
    check_path("ex:a|ex:b|ex:c", "ex:a|ex:b|ex:c");
    check_path("ex:a/(ex:b/ex:c)", "ex:a/(ex:b/ex:c)");
}

#[test]
fn path_inverse_and_mods() {
    check_path("^ex:a*", "^ex:a*");
    check_path("(^ex:a)*", "(^ex:a)*");
    check_path("(ex:a/ex:b)+", "(ex:a/ex:b)+");
    check_path("ex:a?/ex:b+", "ex:a?/ex:b+");
    check_path("(ex:a*)*", "(ex:a*)*");
}

#[test]
fn path_negated_sets_and_a() {
    check_path("!(ex:a|^ex:b)", "!(ex:a|^ex:b)");
    check_path("!ex:a", "!(ex:a)");
    check_path("a/ex:b", "a/ex:b");
    check_path("!(a)", "!(a)");
    // Bare `a` parses as a plain term verb, not a path.
    assert_eq!(verb_path("a"), "a");
}

// ------------------------------------------------------------------ terms

#[test]
fn literals() {
    check_term("42", "42");
    check_term("-7", "-7");
    check_term("4.5", "4.5");
    check_term(".5", ".5");
    check_term("4.2e0", "4.2e0");
    check_term("true", "true");
    check_term(
        "\"1.\"^^<http://www.w3.org/2001/XMLSchema#decimal>",
        "\"1.\"^^<http://www.w3.org/2001/XMLSchema#decimal>",
    );
    check_term("\"plain\"", "\"plain\"");
    check_term("\"hi\"@en", "\"hi\"@en");
    check_term("\"hi\"@en--ltr", "\"hi\"@en--ltr");
    check_term("\"שלום\"@he--rtl", "\"שלום\"@he--rtl");
    check_term("\"q\\\"uote\\n\"", "\"q\\\"uote\\n\"");
    check_term("\"x\"^^ex:dt", "\"x\"^^ex:dt");
}

#[test]
fn iris_and_pnames() {
    // Longest namespace wins: http://ex/a/ over http://ex/.
    check_term("<http://ex/a/x>", "exa:x");
    check_term("<http://ex/foo>", "ex:foo");
    check_term("ex:foo", "ex:foo");
    // Empty local part.
    check_term("<http://ex/>", "ex:");
    // No declared namespace covers it.
    check_term("<http://other/x>", "<http://other/x>");
    // Trailing dot must be escaped; mid dots ride raw.
    check_term("<http://ex/a.b.>", "ex:a.b\\.");
    // PN_LOCAL_ESC characters.
    check_term("<http://ex/a,b>", "ex:a\\,b");
    // Percent triples pass through verbatim.
    check_term("<http://ex/a%20b>", "ex:a%20b");
    // A leading hyphen is escapable, never raw-first.
    check_term("<http://ex/-x>", "ex:\\-x");
}

#[test]
fn blanks_and_triple_terms() {
    let src = format!("{PROLOGUE}SELECT * WHERE {{ _:b1 ex:p _:_u }}");
    let q = parse_query(&src).expect("parse");
    let Some(GroupElement::Triples(ts)) = q.pattern.elements.first() else {
        panic!("triples");
    };
    assert_eq!(print_term(&ts[0].s, &q.prefixes), "_:b1");
    // User labels print verbatim — fresh-label renaming avoids them via
    // the collision set instead of mangling them.
    assert_eq!(print_term(&ts[0].o, &q.prefixes), "_:_u");

    check_term("<<( ?s ex:q 1 )>>", "<<( ?s ex:q 1 )>>");
    check_term("<<( ?s a ?t )>>", "<<( ?s a ?t )>>");
}

#[test]
fn prefix_shadowing() {
    // Redeclaration wins; the winning target compresses, the old one
    // no longer does.
    let src = "PREFIX p: <http://one/> PREFIX p: <http://two/> \
               SELECT * WHERE { ?s ?p <http://two/x> . ?s ?q <http://one/y> }";
    let q = parse_query(src).expect("parse");
    let Some(GroupElement::Triples(ts)) = q.pattern.elements.first() else {
        panic!("triples");
    };
    assert_eq!(print_term(&ts[0].o, &q.prefixes), "p:x");
    assert_eq!(print_term(&ts[1].o, &q.prefixes), "<http://one/y>");
}
