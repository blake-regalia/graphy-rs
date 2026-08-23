//! SSE-style s-expression printer for the algebra (doc 04 §5): stable,
//! deterministic output for golden tests, differential comparison, and
//! EXPLAIN. Terms print in N-Triples-ish surface form; variables as
//! `?name` (internal ones keep their `.`-prefixed names).

use graphy_core::{concise, TermRef};

use crate::algebra::*;

/// Render an algebra tree as an indented s-expression.
pub fn to_sse(a: &Algebra, vars: &VarTable) -> String {
    let mut out = String::new();
    node(&mut out, a, vars, 0);
    out
}

fn indent(out: &mut String, depth: usize) {
    for _ in 0..depth {
        out.push_str("  ");
    }
}

fn node(out: &mut String, a: &Algebra, vars: &VarTable, depth: usize) {
    indent(out, depth);
    match a {
        Algebra::Bgp(ts) => {
            if ts.is_empty() {
                out.push_str("(table unit)\n");
                return;
            }
            out.push_str("(bgp\n");
            for t in ts {
                indent(out, depth + 1);
                out.push_str(&format!(
                    "(triple {} {} {})\n",
                    p(&t.s, vars),
                    p(&t.p, vars),
                    p(&t.o, vars)
                ));
            }
            indent(out, depth);
            out.push_str(")\n");
        }
        Algebra::Path { s, path, o } => {
            out.push_str(&format!(
                "(path {} {} {})\n",
                p(s, vars),
                path_str(path),
                p(o, vars)
            ));
        }
        Algebra::Join(a, b) => wrap2(out, "join", a, b, vars, depth),
        Algebra::Union(a, b) => wrap2(out, "union", a, b, vars, depth),
        Algebra::Minus(a, b) => wrap2(out, "minus", a, b, vars, depth),
        Algebra::LeftJoin { left, right, expr } => {
            out.push_str("(leftjoin");
            if let Some(e) = expr {
                out.push(' ');
                out.push_str(&expr_str(e, vars));
            }
            out.push('\n');
            node(out, left, vars, depth + 1);
            node(out, right, vars, depth + 1);
            indent(out, depth);
            out.push_str(")\n");
        }
        Algebra::Filter { expr, input } => {
            out.push_str(&format!("(filter {}\n", expr_str(expr, vars)));
            node(out, input, vars, depth + 1);
            indent(out, depth);
            out.push_str(")\n");
        }
        Algebra::Graph { graph, input } => {
            out.push_str(&format!("(graph {}\n", p(graph, vars)));
            node(out, input, vars, depth + 1);
            indent(out, depth);
            out.push_str(")\n");
        }
        Algebra::Service {
            silent,
            target,
            input,
        } => {
            out.push_str(&format!(
                "(service{} {}\n",
                if *silent { " silent" } else { "" },
                p(target, vars)
            ));
            node(out, input, vars, depth + 1);
            indent(out, depth);
            out.push_str(")\n");
        }
        Algebra::Extend { input, var, expr } => {
            out.push_str(&format!(
                "(extend (?{} {})\n",
                vars.name(*var),
                expr_str(expr, vars)
            ));
            node(out, input, vars, depth + 1);
            indent(out, depth);
            out.push_str(")\n");
        }
        Algebra::Table { vars: vs, rows } => {
            out.push_str("(table (vars");
            for v in vs {
                out.push_str(&format!(" ?{}", vars.name(*v)));
            }
            if rows.is_empty() {
                out.push_str("))\n");
                return;
            }
            out.push_str(")\n");
            for row in rows {
                indent(out, depth + 1);
                out.push_str("(row");
                for (v, cell) in vs.iter().zip(row) {
                    if let Some(bytes) = cell {
                        out.push_str(&format!(" (?{} {})", vars.name(*v), term_str(bytes)));
                    }
                }
                out.push_str(")\n");
            }
            indent(out, depth);
            out.push_str(")\n");
        }
        Algebra::ToMultiSet(input) => wrap1(out, "tomultiset", input, vars, depth),
        Algebra::Group {
            keys,
            aggregates,
            input,
        } => {
            out.push_str("(group (");
            for (i, (v, e)) in keys.iter().enumerate() {
                if i > 0 {
                    out.push(' ');
                }
                match e {
                    Some(e) => {
                        out.push_str(&format!("(?{} {})", vars.name(*v), expr_str(e, vars)));
                    }
                    None => out.push_str(&format!("?{}", vars.name(*v))),
                }
            }
            out.push_str(") (");
            for (i, (v, agg)) in aggregates.iter().enumerate() {
                if i > 0 {
                    out.push(' ');
                }
                out.push_str(&format!("(?{} {})", vars.name(*v), agg_str(agg, vars)));
            }
            out.push_str(")\n");
            node(out, input, vars, depth + 1);
            indent(out, depth);
            out.push_str(")\n");
        }
        Algebra::OrderBy { input, conditions } => {
            out.push_str("(order (");
            for (i, (e, desc)) in conditions.iter().enumerate() {
                if i > 0 {
                    out.push(' ');
                }
                if *desc {
                    out.push_str(&format!("(desc {})", expr_str(e, vars)));
                } else {
                    out.push_str(&expr_str(e, vars));
                }
            }
            out.push_str(")\n");
            node(out, input, vars, depth + 1);
            indent(out, depth);
            out.push_str(")\n");
        }
        Algebra::Project { input, vars: vs } => {
            out.push_str("(project (");
            for (i, v) in vs.iter().enumerate() {
                if i > 0 {
                    out.push(' ');
                }
                out.push_str(&format!("?{}", vars.name(*v)));
            }
            out.push_str(")\n");
            node(out, input, vars, depth + 1);
            indent(out, depth);
            out.push_str(")\n");
        }
        Algebra::Distinct(input) => wrap1(out, "distinct", input, vars, depth),
        Algebra::Reduced(input) => wrap1(out, "reduced", input, vars, depth),
        Algebra::Slice {
            input,
            offset,
            limit,
        } => {
            out.push_str(&format!(
                "(slice {} {}\n",
                offset,
                limit.map_or("_".to_owned(), |l| l.to_string())
            ));
            node(out, input, vars, depth + 1);
            indent(out, depth);
            out.push_str(")\n");
        }
    }
}

fn wrap1(out: &mut String, tag: &str, input: &Algebra, vars: &VarTable, depth: usize) {
    out.push_str(&format!("({tag}\n"));
    node(out, input, vars, depth + 1);
    indent(out, depth);
    out.push_str(")\n");
}

fn wrap2(out: &mut String, tag: &str, a: &Algebra, b: &Algebra, vars: &VarTable, depth: usize) {
    out.push_str(&format!("({tag}\n"));
    node(out, a, vars, depth + 1);
    node(out, b, vars, depth + 1);
    indent(out, depth);
    out.push_str(")\n");
}

fn p(x: &P, vars: &VarTable) -> String {
    match x {
        P::Var(v) => format!("?{}", vars.name(*v)),
        P::Term(bytes) => term_str(bytes),
        P::Triple(tp) => format!(
            "(tripleterm {} {} {})",
            p(&tp.s, vars),
            p(&tp.p, vars),
            p(&tp.o, vars)
        ),
    }
}

/// Concise bytes → N-Triples-ish surface form.
fn term_str(bytes: &[u8]) -> String {
    match concise::decode(bytes) {
        Ok(t) => term_ref_str(&t),
        Err(_) => format!("{bytes:?}"),
    }
}

fn term_ref_str(t: &TermRef<'_>) -> String {
    match t {
        TermRef::Iri(iri) => format!("<{iri}>"),
        TermRef::BlankNode(label) => format!("_:{label}"),
        TermRef::Literal(l) => {
            let mut s = format!("{:?}", l.lexical());
            if let Some((tag, dir)) = l.lang() {
                s.push('@');
                s.push_str(tag);
                if let Some(dir) = dir {
                    s.push_str(match dir {
                        graphy_core::Dir::Ltr => "--ltr",
                        graphy_core::Dir::Rtl => "--rtl",
                    });
                }
            } else {
                let dt = l.datatype();
                if dt != graphy_core::vocab::XSD_STRING {
                    s.push_str("^^<");
                    s.push_str(dt);
                    s.push('>');
                }
            }
            s
        }
        TermRef::TripleTerm(view) => format!(
            "(tripleterm {} {} {})",
            term_ref_str(&view.subject()),
            term_ref_str(&view.predicate()),
            term_ref_str(&view.object())
        ),
    }
}

fn path_str(p: &PathExpr) -> String {
    match p {
        PathExpr::Link(iri) => term_str(iri),
        PathExpr::Inverse(x) => format!("(reverse {})", path_str(x)),
        PathExpr::Seq(a, b) => format!("(seq {} {})", path_str(a), path_str(b)),
        PathExpr::Alt(a, b) => format!("(alt {} {})", path_str(a), path_str(b)),
        PathExpr::ZeroOrMore(x) => format!("(path* {})", path_str(x)),
        PathExpr::OneOrMore(x) => format!("(path+ {})", path_str(x)),
        PathExpr::ZeroOrOne(x) => format!("(path? {})", path_str(x)),
        PathExpr::Nps(items) => {
            let mut s = String::from("(notoneof");
            for (iri, inv) in items {
                if *inv {
                    s.push_str(&format!(" (reverse {})", term_str(iri)));
                } else {
                    s.push(' ');
                    s.push_str(&term_str(iri));
                }
            }
            s.push(')');
            s
        }
    }
}

fn expr_str(e: &Expression, vars: &VarTable) -> String {
    match e {
        Expression::Term(bytes) => term_str(bytes),
        Expression::Var(v) => format!("?{}", vars.name(*v)),
        Expression::Or(a, b) => format!("(|| {} {})", expr_str(a, vars), expr_str(b, vars)),
        Expression::And(a, b) => format!("(&& {} {})", expr_str(a, vars), expr_str(b, vars)),
        Expression::Cmp(op, a, b) => {
            let op = match op {
                CmpOp::Eq => "=",
                CmpOp::Ne => "!=",
                CmpOp::Lt => "<",
                CmpOp::Le => "<=",
                CmpOp::Gt => ">",
                CmpOp::Ge => ">=",
            };
            format!("({op} {} {})", expr_str(a, vars), expr_str(b, vars))
        }
        Expression::In {
            expr,
            list,
            negated,
        } => {
            let mut s = format!(
                "({} {}",
                if *negated { "notin" } else { "in" },
                expr_str(expr, vars)
            );
            for x in list {
                s.push(' ');
                s.push_str(&expr_str(x, vars));
            }
            s.push(')');
            s
        }
        Expression::Add(a, b) => format!("(+ {} {})", expr_str(a, vars), expr_str(b, vars)),
        Expression::Sub(a, b) => format!("(- {} {})", expr_str(a, vars), expr_str(b, vars)),
        Expression::Mul(a, b) => format!("(* {} {})", expr_str(a, vars), expr_str(b, vars)),
        Expression::Div(a, b) => format!("(/ {} {})", expr_str(a, vars), expr_str(b, vars)),
        Expression::Not(a) => format!("(! {})", expr_str(a, vars)),
        Expression::UnaryMinus(a) => format!("(neg {})", expr_str(a, vars)),
        Expression::UnaryPlus(a) => format!("(pos {})", expr_str(a, vars)),
        Expression::Builtin(b, args) => {
            let mut s = format!("({}", builtin_tag(*b));
            for a in args {
                s.push(' ');
                s.push_str(&expr_str(a, vars));
            }
            s.push(')');
            s
        }
        Expression::Function {
            iri,
            args,
            distinct,
        } => {
            let mut s = format!("(call {}", term_str(iri));
            if *distinct {
                s.push_str(" distinct");
            }
            for a in args {
                s.push(' ');
                s.push_str(&expr_str(a, vars));
            }
            s.push(')');
            s
        }
        Expression::Exists { negated, pattern } => {
            let inner = to_sse(pattern, vars);
            format!(
                "({} {})",
                if *negated { "notexists" } else { "exists" },
                inner.split_whitespace().collect::<Vec<_>>().join(" ")
            )
        }
        Expression::TripleTerm { s, p, o } => format!(
            "(tripleterm {} {} {})",
            expr_str(s, vars),
            expr_str(p, vars),
            expr_str(o, vars)
        ),
    }
}

fn agg_str(a: &AggregateExpr, vars: &VarTable) -> String {
    let name = match a.func {
        Aggregate::Count => "count",
        Aggregate::Sum => "sum",
        Aggregate::Min => "min",
        Aggregate::Max => "max",
        Aggregate::Avg => "avg",
        Aggregate::Sample => "sample",
        Aggregate::GroupConcat => "group_concat",
    };
    let mut s = format!("({name}");
    if a.distinct {
        s.push_str(" distinct");
    }
    match &a.expr {
        Some(e) => {
            s.push(' ');
            s.push_str(&expr_str(e, vars));
        }
        None => s.push_str(" *"),
    }
    if let Some(sep) = &a.separator {
        s.push_str(&format!(" (separator {sep:?})"));
    }
    s.push(')');
    s
}

fn builtin_tag(b: Builtin) -> &'static str {
    use Builtin as B;
    match b {
        B::Str => "str",
        B::Lang => "lang",
        B::LangMatches => "langmatches",
        B::Datatype => "datatype",
        B::Bound => "bound",
        B::Iri => "iri",
        B::BNode => "bnode",
        B::Rand => "rand",
        B::Abs => "abs",
        B::Ceil => "ceil",
        B::Floor => "floor",
        B::Round => "round",
        B::Concat => "concat",
        B::StrLen => "strlen",
        B::UCase => "ucase",
        B::LCase => "lcase",
        B::EncodeForUri => "encode_for_uri",
        B::Contains => "contains",
        B::StrStarts => "strstarts",
        B::StrEnds => "strends",
        B::StrBefore => "strbefore",
        B::StrAfter => "strafter",
        B::Year => "year",
        B::Month => "month",
        B::Day => "day",
        B::Hours => "hours",
        B::Minutes => "minutes",
        B::Seconds => "seconds",
        B::Timezone => "timezone",
        B::Tz => "tz",
        B::Now => "now",
        B::Uuid => "uuid",
        B::StrUuid => "struuid",
        B::Md5 => "md5",
        B::Sha1 => "sha1",
        B::Sha256 => "sha256",
        B::Sha384 => "sha384",
        B::Sha512 => "sha512",
        B::Coalesce => "coalesce",
        B::If => "if",
        B::StrLang => "strlang",
        B::StrDt => "strdt",
        B::SameTerm => "sameterm",
        B::IsIri => "isiri",
        B::IsBlank => "isblank",
        B::IsLiteral => "isliteral",
        B::IsNumeric => "isnumeric",
        B::Regex => "regex",
        B::Substr => "substr",
        B::Replace => "replace",
        B::Triple => "triple",
        B::Subject => "subject",
        B::Predicate => "predicate",
        B::Object => "object",
        B::IsTriple => "istriple",
        B::LangDir => "langdir",
        B::HasLang => "haslang",
        B::HasLangDir => "haslangdir",
        B::StrLangDir => "strlangdir",
    }
}
