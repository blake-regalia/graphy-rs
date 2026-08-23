//! Lexer tests: token streams over representative queries, the grammar's
//! disambiguation corners, and span-carrying errors.

use graphy_sparql_syntax::{tokenize, Dir, Kw, StringForm, TokenKind};
use TokenKind::*;

fn kinds(src: &str) -> Vec<TokenKind> {
    tokenize(src)
        .unwrap_or_else(|e| panic!("lex `{src}`: {e}"))
        .into_iter()
        .map(|t| t.kind)
        .collect()
}

fn texts(src: &str) -> Vec<std::string::String> {
    tokenize(src)
        .unwrap()
        .into_iter()
        .map(|t| t.span.text(src).to_owned())
        .collect()
}

#[test]
fn select_query() {
    let src = "PREFIX ex: <http://example.org/>\n\
               SELECT ?name (COUNT(?friend) AS ?n) WHERE {\n\
               \t?person ex:name ?name ; ex:knows ?friend .\n\
               } GROUP BY ?name HAVING (COUNT(?friend) > 2) ORDER BY DESC(?n) LIMIT 10";
    assert_eq!(
        kinds(src),
        vec![
            Keyword(Kw::Prefix),
            PNameNs,
            IriRef,
            Keyword(Kw::Select),
            Var,
            LParen,
            Keyword(Kw::Count),
            LParen,
            Var,
            RParen,
            Keyword(Kw::As),
            Var,
            RParen,
            Keyword(Kw::Where),
            LBrace,
            Var,
            PNameLn,
            Var,
            Semicolon,
            PNameLn,
            Var,
            Dot,
            RBrace,
            Keyword(Kw::Group),
            Keyword(Kw::By),
            Var,
            Keyword(Kw::Having),
            LParen,
            Keyword(Kw::Count),
            LParen,
            Var,
            RParen,
            Gt,
            Integer,
            RParen,
            Keyword(Kw::Order),
            Keyword(Kw::By),
            Keyword(Kw::Desc),
            LParen,
            Var,
            RParen,
            Keyword(Kw::Limit),
            Integer,
        ]
    );
}

#[test]
fn keywords_are_case_insensitive_and_pnames_win() {
    assert_eq!(
        kinds("select SeLeCt select: select:x filter"),
        vec![
            Keyword(Kw::Select),
            Keyword(Kw::Select),
            PNameNs,
            PNameLn,
            Keyword(Kw::Filter),
        ]
    );
}

#[test]
fn iri_vs_relational_operators() {
    // `<` starts an IRI only when a well-formed IRIREF follows.
    assert_eq!(kinds("?x < 5"), vec![Var, Lt, Integer]);
    assert_eq!(kinds("?x <= ?y"), vec![Var, Le, Var]);
    assert_eq!(kinds("<http://a/b#c>"), vec![IriRef]);
    assert_eq!(kinds("?x < <http://a>"), vec![Var, Lt, IriRef]);
    assert_eq!(kinds("<>"), vec![IriRef]); // relative empty reference
    assert_eq!(kinds("?x > ?y >= ?z"), vec![Var, Gt, Var, Ge, Var]);
}

#[test]
fn vars_and_path_question() {
    assert_eq!(kinds("?x $y ?_1 ?名"), vec![Var, Var, Var, Var]);
    // `?` not followed by a name is the zero-or-one path operator.
    assert_eq!(kinds("ex:p? "), vec![PNameLn, Question]);
    assert_eq!(
        kinds("^ex:p/ex:q|ex:r"),
        vec![Caret, PNameLn, Slash, PNameLn, Pipe, PNameLn]
    );
}

#[test]
fn numbers() {
    assert_eq!(
        kinds("1 1.5 .5 1e0 1.5e-3 .5E+2"),
        vec![Integer, Decimal, Decimal, Double, Double, Double]
    );
    // Signs attach when adjacent (NumericLiteralPositive/Negative).
    assert_eq!(kinds("?x+1"), vec![Var, Integer]);
    assert_eq!(texts("?x+1"), vec!["?x", "+1"]);
    assert_eq!(kinds("?x + 1"), vec![Var, Plus, Integer]);
    assert_eq!(kinds("-2.5"), vec![Decimal]);
    // `1.` is INTEGER then Dot (DECIMAL needs digits after the point).
    assert_eq!(kinds("ex:o 1. "), vec![PNameLn, Integer, Dot]);
}

#[test]
fn strings_and_langtags() {
    assert_eq!(
        kinds(r#""hi" 'yo' """long "quotes" ok""" '''x''' "esc\nA""#),
        vec![
            String(StringForm::Quote),
            String(StringForm::Apos),
            String(StringForm::LongQuote),
            String(StringForm::LongApos),
            String(StringForm::Quote),
        ]
    );
    let toks = tokenize(r#""x"@en "y"@en-US "z"@ar--rtl"#).unwrap();
    assert_eq!(toks[1].kind, LangTag(None));
    assert_eq!(toks[3].kind, LangTag(None));
    assert_eq!(toks[5].kind, LangTag(Some(Dir::Rtl)));
    assert_eq!(
        kinds(r#""1"^^<http://www.w3.org/2001/XMLSchema#integer>"#),
        vec![String(StringForm::Quote), CaretCaret, IriRef]
    );
}

#[test]
fn nil_anon_blank() {
    // Nil + (LParen Var RParen) + Anon + Anon + [5 bracket-pattern tokens]
    assert_eq!(kinds("( ) (?x) [ ] [] [?x ex:p 1]").len(), 11);
    assert_eq!(kinds("()")[0], Nil);
    assert_eq!(kinds("(  )")[0], Nil);
    assert_eq!(kinds("[ ]")[0], Anon);
    assert_eq!(kinds("_:b1 _:0x.y"), vec![BlankNode, BlankNode]);
    // Trailing dot belongs to the statement.
    assert_eq!(kinds("_:b. "), vec![BlankNode, Dot]);
}

#[test]
fn sparql12_terminals() {
    assert_eq!(
        kinds("<<( ?s ex:p ?o )>> << ?s ex:p ?o >> ~?r"),
        vec![LtLtParen, Var, PNameLn, Var, RParenGtGt, LtLt, Var, PNameLn, Var, GtGt, Tilde, Var,]
    );
    assert_eq!(
        kinds("?s ex:p ?o {| ex:since 2024 |}"),
        vec![Var, PNameLn, Var, LBraceBar, PNameLn, Integer, RBarBrace]
    );
    assert_eq!(
        kinds("TRIPLE(?s, ?p, ?o) isTRIPLE(?t) LANGDIR(?l)"),
        vec![
            Keyword(Kw::Triple),
            LParen,
            Var,
            Comma,
            Var,
            Comma,
            Var,
            RParen,
            Keyword(Kw::IsTriple),
            LParen,
            Var,
            RParen,
            Keyword(Kw::LangDir),
            LParen,
            Var,
            RParen,
        ]
    );
    assert_eq!(
        kinds("VERSION \"1.2\""),
        vec![Keyword(Kw::Version), String(StringForm::Quote)]
    );
}

#[test]
fn a_true_false_and_expressions() {
    assert_eq!(
        kinds("?s a ex:T . FILTER(!?x && ?y || ?z != true)"),
        vec![
            Var,
            A,
            PNameLn,
            Dot,
            Keyword(Kw::Filter),
            LParen,
            Bang,
            Var,
            AndAnd,
            Var,
            OrOr,
            Var,
            Ne,
            True,
            RParen,
        ]
    );
    assert_eq!(kinds("false"), vec![False]);
}

#[test]
fn pname_locals() {
    // Interior dots, colons, %-escapes, and \-escapes in local names.
    assert_eq!(kinds(":x :x.y :x:y :%41b :a\\#b"), vec![PNameLn; 5]);
    assert_eq!(kinds(": ex:"), vec![PNameNs, PNameNs]);
    // Trailing dot returns to the stream.
    assert_eq!(kinds("ex:o. "), vec![PNameLn, Dot]);
    assert_eq!(texts("ex:o. "), vec!["ex:o", "."]);
}

#[test]
fn comments_and_spans() {
    let src = "SELECT # trailing comment\n?x";
    let toks = tokenize(src).unwrap();
    assert_eq!(toks.len(), 2);
    assert_eq!(toks[1].span.text(src), "?x");
}

#[test]
fn update_keywords() {
    assert_eq!(
        kinds("INSERT DATA { GRAPH <g> { <s> <p> 1 } } ; DELETE WHERE { ?s ?p ?o }"),
        vec![
            Keyword(Kw::Insert),
            Keyword(Kw::Data),
            LBrace,
            Keyword(Kw::Graph),
            IriRef,
            LBrace,
            IriRef,
            IriRef,
            Integer,
            RBrace,
            RBrace,
            Semicolon,
            Keyword(Kw::Delete),
            Keyword(Kw::Where),
            LBrace,
            Var,
            Var,
            Var,
            RBrace,
        ]
    );
}

#[test]
fn errors_carry_spans() {
    for (src, what) in [
        ("\"unterminated", "unterminated"),
        ("\"bad\\q\"", "escape"),
        ("@", "language tag"),
        ("&x", "&&"),
        ("bogusword", "unknown keyword"),
        ("1e", "exponent"),
        ("$ ", "variable name"),
        (":a%GG", "%-escape"),
        ("\"nl\n\"", "newline"),
        ("\"\\uD800\"", "scalar"),
    ] {
        let e = tokenize(src).expect_err(src);
        assert!(
            e.message.contains(what),
            "`{src}` → `{}` (wanted `{what}`)",
            e.message
        );
        assert!((e.span.start as usize) < src.len().max(1), "span in range");
    }
}

#[test]
fn empty_and_trivia_only() {
    assert!(tokenize("").unwrap().is_empty());
    assert!(tokenize("  # only a comment\n\t").unwrap().is_empty());
}
