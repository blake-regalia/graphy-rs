//! AST → algebra translation, following SPARQL 1.1 §18.2 step by step
//! (doc 04 §3): path translation with fixed-length decomposition
//! (§18.2.2.4), group translation with OPTIONAL filter fusion (§18.2.2.6),
//! aggregate extraction (§18.2.4), and solution modifiers (§18.2.5).

use graphy_core::{concise, vocab};
use graphy_sparql_syntax::ast;
use graphy_sparql_syntax::Span;

use crate::algebra::*;

/// Translation failure (spans point into the original query text).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TranslateError {
    pub span: Span,
    pub message: String,
}

impl std::fmt::Display for TranslateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} at byte {}", self.message, self.span.start)
    }
}

impl std::error::Error for TranslateError {}

/// The translated query: an algebra tree plus form-specific data.
#[derive(Debug, Clone)]
pub struct TranslatedQuery {
    pub vars: VarTable,
    /// (default?, concise IRI) FROM / FROM NAMED clauses.
    pub dataset: Vec<(bool, Vec<u8>)>,
    /// Prologue `BASE` (runtime `IRI()` resolution, §17.4.2.8).
    pub base: Option<String>,
    pub form: Form,
    pub root: Algebra,
}

#[derive(Debug, Clone)]
pub enum Form {
    /// Projection & friends live in the tree (`Project`, `Distinct`, …).
    Select,
    Ask,
    /// Template patterns (variables + constants).
    Construct(Vec<TriplePat>),
    /// Targets (`DESCRIBE *` resolves to the pattern's visible variables).
    Describe(Vec<P>),
}

/// Translate a parsed query per §18.2.
pub fn translate_query(q: &ast::Query) -> Result<TranslatedQuery, TranslateError> {
    let mut cx = Cx {
        vars: VarTable::default(),
    };
    let dataset = q
        .dataset
        .iter()
        .map(|d| match d {
            ast::DatasetClause::Default(iri) => (true, encode_iri(iri)),
            ast::DatasetClause::Named(iri) => (false, encode_iri(iri)),
        })
        .collect();

    let pattern = cx.group(&q.pattern)?;
    let (form, root) = match &q.form {
        ast::QueryForm::Select(select) => {
            let root = cx.select_level(pattern, select, &q.modifiers, q.values.as_ref())?;
            (Form::Select, root)
        }
        ast::QueryForm::Ask => {
            let root = cx.select_level(
                pattern,
                &ast::SelectClause {
                    distinct: false,
                    reduced: false,
                    projection: Vec::new(),
                },
                &q.modifiers,
                q.values.as_ref(),
            )?;
            (Form::Ask, root)
        }
        ast::QueryForm::Construct(template) => {
            let template = template
                .iter()
                .map(|t| cx.triple_pat(t))
                .collect::<Result<_, _>>()?;
            let root = cx.select_level(
                pattern,
                &ast::SelectClause {
                    distinct: false,
                    reduced: false,
                    projection: Vec::new(),
                },
                &q.modifiers,
                q.values.as_ref(),
            )?;
            (Form::Construct(template), root)
        }
        ast::QueryForm::Describe { targets, star } => {
            let list = if *star {
                visible_vars(&pattern).into_iter().map(P::Var).collect()
            } else {
                targets
                    .iter()
                    .map(|t| cx.term(t))
                    .collect::<Result<_, _>>()?
            };
            let root = cx.select_level(
                pattern,
                &ast::SelectClause {
                    distinct: false,
                    reduced: false,
                    projection: Vec::new(),
                },
                &q.modifiers,
                q.values.as_ref(),
            )?;
            (Form::Describe(list), root)
        }
    };
    Ok(TranslatedQuery {
        vars: cx.vars,
        dataset,
        base: q.base.clone(),
        form,
        root,
    })
}

struct Cx {
    vars: VarTable,
}

impl Cx {
    // ------------------------------------------------------------ terms

    fn term(&mut self, t: &ast::Term) -> Result<P, TranslateError> {
        Ok(match &t.kind {
            ast::TermKind::Var(v) => P::Var(self.vars.intern(v)),
            ast::TermKind::Iri(iri) => P::Term(encode_iri(iri)),
            // Pattern blank nodes act as non-projectable variables
            // (§18.1.6 semantics); the `.`-prefix keeps them apart.
            ast::TermKind::BlankNode(label) => P::Var(self.vars.intern(&format!(".b:{label}"))),
            ast::TermKind::Literal { lexical, kind } => P::Term(encode_literal(lexical, kind)),
            ast::TermKind::TripleTerm(tp) => {
                let s = self.term(&tp.s)?;
                let p = match &tp.p {
                    ast::Verb::Term(p) => self.term(p)?,
                    ast::Verb::Path(_) => {
                        return Err(err(t.span, "property path inside a triple term"));
                    }
                };
                let o = self.term(&tp.o)?;
                match (&s, &p, &o) {
                    (P::Term(s), P::Term(p), P::Term(o)) => {
                        let mut out = Vec::new();
                        concise::encode_triple_term(&mut out, s, p, o);
                        P::Term(out)
                    }
                    _ => P::Triple(Box::new(TriplePat { s, p, o })),
                }
            }
        })
    }

    fn triple_pat(&mut self, t: &ast::TriplePattern) -> Result<TriplePat, TranslateError> {
        let p = match &t.p {
            ast::Verb::Term(p) => self.term(p)?,
            ast::Verb::Path(_) => {
                return Err(err(
                    t.s.span,
                    "property paths cannot appear in this position",
                ));
            }
        };
        Ok(TriplePat {
            s: self.term(&t.s)?,
            p,
            o: self.term(&t.o)?,
        })
    }

    // ------------------------------------------------------------ paths

    /// §18.2.2.4: fixed-length forms decompose; `*`,`+`,`?`, NPS stay
    /// path nodes.
    fn path_triple(&mut self, s: P, path: &ast::Path, o: P) -> Result<Algebra, TranslateError> {
        Ok(match path {
            ast::Path::Iri(iri) => Algebra::Bgp(vec![TriplePat {
                s,
                p: P::Term(encode_iri(iri)),
                o,
            }]),
            ast::Path::Inverse(inner) => self.path_triple(o, inner, s)?,
            ast::Path::Seq(a, b) => {
                let mid = P::Var(self.vars.fresh("p"));
                let left = self.path_triple(s, a, mid.clone())?;
                let right = self.path_triple(mid, b, o)?;
                join(left, right)
            }
            ast::Path::Alt(a, b) => {
                let left = self.path_triple(s.clone(), a, o.clone())?;
                let right = self.path_triple(s, b, o)?;
                Algebra::Union(Box::new(left), Box::new(right))
            }
            ast::Path::ZeroOrMore(_)
            | ast::Path::OneOrMore(_)
            | ast::Path::ZeroOrOne(_)
            | ast::Path::Nps(_) => Algebra::Path {
                s,
                path: path_expr(path),
                o,
            },
        })
    }

    // ------------------------------------------------------------ groups

    /// §18.2.2.6 TranslateGroupGraphPattern.
    fn group(&mut self, g: &ast::GroupPattern) -> Result<Algebra, TranslateError> {
        let mut acc = Algebra::Bgp(Vec::new());
        let mut filters: Vec<Expression> = Vec::new();
        for e in &g.elements {
            match e {
                ast::GroupElement::Triples(ts) => {
                    let translated = self.triples_run(ts)?;
                    acc = join(acc, translated);
                }
                ast::GroupElement::Filter(e) => filters.push(self.expr(e, None)?),
                ast::GroupElement::Optional(inner) => {
                    let t = self.group(inner)?;
                    acc = match t {
                        Algebra::Filter { expr, input } => Algebra::LeftJoin {
                            left: Box::new(acc),
                            right: input,
                            expr: Some(expr),
                        },
                        other => Algebra::LeftJoin {
                            left: Box::new(acc),
                            right: Box::new(other),
                            expr: None,
                        },
                    };
                }
                ast::GroupElement::Minus(inner) => {
                    let t = self.group(inner)?;
                    acc = Algebra::Minus(Box::new(acc), Box::new(t));
                }
                ast::GroupElement::Union(branches) => {
                    let mut it = branches.iter();
                    let first = it.next().expect("union has branches");
                    let mut u = self.group(first)?;
                    let nested_group = branches.len() == 1;
                    for b in it {
                        u = Algebra::Union(Box::new(u), Box::new(self.group(b)?));
                    }
                    if nested_group && !matches!(u, Algebra::ToMultiSet(_)) {
                        // A brace-only nested group is an evaluation-scope
                        // barrier: its FILTER cannot be hoisted into the
                        // enclosing OPTIONAL's LeftJoin expression.
                        u = Algebra::ToMultiSet(Box::new(u));
                    }
                    acc = join(acc, u);
                }
                ast::GroupElement::Graph(target, inner) => {
                    let graph = self.term(target)?;
                    let t = self.group(inner)?;
                    acc = join(
                        acc,
                        Algebra::Graph {
                            graph,
                            input: Box::new(t),
                        },
                    );
                }
                ast::GroupElement::Service {
                    silent,
                    target,
                    pattern,
                } => {
                    let target = self.term(target)?;
                    let t = self.group(pattern)?;
                    acc = join(
                        acc,
                        Algebra::Service {
                            silent: *silent,
                            target,
                            input: Box::new(t),
                        },
                    );
                }
                ast::GroupElement::Bind { expr, var, .. } => {
                    let e = self.expr(expr, None)?;
                    let var = self.vars.intern(var);
                    acc = Algebra::Extend {
                        input: Box::new(acc),
                        var,
                        expr: e,
                    };
                }
                ast::GroupElement::Values(v) => {
                    let t = self.values(v)?;
                    acc = join(acc, t);
                }
                ast::GroupElement::SubSelect(sub) => {
                    let inner = self.group(&sub.pattern)?;
                    let translated =
                        self.select_level(inner, &sub.select, &sub.modifiers, sub.values.as_ref())?;
                    acc = join(acc, Algebra::ToMultiSet(Box::new(translated)));
                }
            }
        }
        if let Some(fs) = filters
            .into_iter()
            .reduce(|a, b| Expression::And(Box::new(a), Box::new(b)))
        {
            acc = Algebra::Filter {
                expr: fs,
                input: Box::new(acc),
            };
        }
        Ok(acc)
    }

    /// One run of triples: plain patterns accumulate into BGPs, path
    /// triples decompose and join in order.
    fn triples_run(&mut self, ts: &[ast::TriplePattern]) -> Result<Algebra, TranslateError> {
        let mut acc: Option<Algebra> = None;
        let mut bgp: Vec<TriplePat> = Vec::new();
        for t in ts {
            match &t.p {
                ast::Verb::Term(_) => bgp.push(self.triple_pat(t)?),
                ast::Verb::Path(path) => {
                    if !bgp.is_empty() {
                        let b = Algebra::Bgp(std::mem::take(&mut bgp));
                        acc = Some(match acc {
                            Some(a) => join(a, b),
                            None => b,
                        });
                    }
                    let s = self.term(&t.s)?;
                    let o = self.term(&t.o)?;
                    let p = self.path_triple(s, path, o)?;
                    acc = Some(match acc {
                        Some(a) => join(a, p),
                        None => p,
                    });
                }
            }
        }
        if !bgp.is_empty() {
            let b = Algebra::Bgp(bgp);
            acc = Some(match acc {
                Some(a) => join(a, b),
                None => b,
            });
        }
        Ok(acc.unwrap_or(Algebra::Bgp(Vec::new())))
    }

    fn values(&mut self, v: &ast::ValuesBlock) -> Result<Algebra, TranslateError> {
        let vars = v.vars.iter().map(|n| self.vars.intern(n)).collect();
        let rows = v
            .rows
            .iter()
            .map(|row| {
                row.iter()
                    .map(|cell| {
                        cell.as_ref()
                            .map(|t| match self.term(t)? {
                                P::Term(bytes) => Ok(bytes),
                                _ => Err(err(t.span, "VALUES terms must be ground")),
                            })
                            .transpose()
                    })
                    .collect::<Result<Vec<_>, _>>()
            })
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Algebra::Table { vars, rows })
    }

    // ------------------------------------------------- select level

    /// §18.2.4–18.2.5: grouping/aggregates, HAVING, final VALUES, select
    /// expressions, ORDER BY, projection, DISTINCT/REDUCED, LIMIT/OFFSET.
    fn select_level(
        &mut self,
        pattern: Algebra,
        select: &ast::SelectClause,
        m: &ast::SolutionModifiers,
        values: Option<&ast::ValuesBlock>,
    ) -> Result<Algebra, TranslateError> {
        let mut g = pattern;

        // Grouping is triggered by GROUP BY or any aggregate use.
        let uses_aggregates = select.projection.iter().any(|p| match p {
            ast::Projection::Expr(e, _) => contains_aggregate(e),
            ast::Projection::Var(_) => false,
        }) || m.having.iter().any(contains_aggregate)
            || m.order_by.iter().any(|c| contains_aggregate(&c.expr));
        let grouped = !m.group_by.is_empty() || uses_aggregates;

        let mut aggs: Vec<(VarId, AggregateExpr)> = Vec::new();
        if grouped {
            let mut keys = Vec::new();
            for c in &m.group_by {
                match c {
                    ast::GroupCondition::Var(v) => keys.push((self.vars.intern(v), None)),
                    ast::GroupCondition::Expr(e, alias) => {
                        let expr = self.expr(e, None)?;
                        let var = match alias {
                            Some(v) => self.vars.intern(v),
                            None => self.vars.fresh("k"),
                        };
                        keys.push((var, Some(expr)));
                    }
                }
            }
            g = Algebra::Group {
                keys,
                aggregates: Vec::new(), // filled below (needs &mut aggs)
                input: Box::new(g),
            };
        }

        // HAVING (aggregates extracted).
        let mut having = Vec::new();
        for h in &m.having {
            having.push(self.expr(h, grouped.then_some(&mut aggs))?);
        }

        // Select expressions (aggregates extracted).
        let mut extends: Vec<(VarId, Expression)> = Vec::new();
        let mut projected: Vec<VarId> = Vec::new();
        for p in &select.projection {
            match p {
                ast::Projection::Var(v) => projected.push(self.vars.intern(v)),
                ast::Projection::Expr(e, v) => {
                    let expr = self.expr(e, grouped.then_some(&mut aggs))?;
                    let var = self.vars.intern(v);
                    extends.push((var, expr));
                    projected.push(var);
                }
            }
        }

        // ORDER BY conditions (aggregates extracted).
        let mut order = Vec::new();
        for c in &m.order_by {
            order.push((
                self.expr(&c.expr, grouped.then_some(&mut aggs))?,
                c.descending,
            ));
        }

        // Stamp the collected aggregates into the Group node.
        if grouped {
            if let Algebra::Group { aggregates, .. } = &mut g {
                *aggregates = aggs;
            }
        }

        for h in having {
            g = Algebra::Filter {
                expr: h,
                input: Box::new(g),
            };
        }
        if let Some(v) = values {
            let t = self.values(v)?;
            g = join(g, t);
        }
        for (var, expr) in extends {
            g = Algebra::Extend {
                input: Box::new(g),
                var,
                expr,
            };
        }
        if !order.is_empty() {
            g = Algebra::OrderBy {
                input: Box::new(g),
                conditions: order,
            };
        }
        // SELECT * projects the visible variables (ASK/CONSTRUCT/DESCRIBE
        // arrive with an empty projection and project everything too —
        // the engine ignores projection for those forms).
        let vars = if projected.is_empty() {
            visible_vars(&g)
                .into_iter()
                .filter(|v| !self.vars.name(*v).starts_with('.'))
                .collect()
        } else {
            projected
        };
        g = Algebra::Project {
            input: Box::new(g),
            vars,
        };
        if select.distinct {
            g = Algebra::Distinct(Box::new(g));
        } else if select.reduced {
            g = Algebra::Reduced(Box::new(g));
        }
        if m.limit.is_some() || m.offset.is_some() {
            g = Algebra::Slice {
                input: Box::new(g),
                offset: m.offset.unwrap_or(0),
                limit: m.limit,
            };
        }
        Ok(g)
    }

    // ------------------------------------------------------- expressions

    /// Translate an expression. `aggs` is the extraction sink: `Some`
    /// inside SELECT/HAVING/ORDER BY of a grouped level (each aggregate
    /// call becomes an internal variable), `None` where aggregates are
    /// illegal (WHERE filters, BIND).
    fn expr(
        &mut self,
        e: &ast::Expr,
        mut aggs: Option<&mut Vec<(VarId, AggregateExpr)>>,
    ) -> Result<Expression, TranslateError> {
        use ast::ExprKind as K;
        let bin = |cx: &mut Cx,
                   a: &ast::Expr,
                   b: &ast::Expr,
                   aggs: &mut Option<&mut Vec<(VarId, AggregateExpr)>>|
         -> Result<(Box<Expression>, Box<Expression>), TranslateError> {
            let left = cx.expr(a, aggs.as_deref_mut())?;
            let right = cx.expr(b, aggs.as_deref_mut())?;
            Ok((Box::new(left), Box::new(right)))
        };
        Ok(match &*e.kind {
            K::Or(a, b) => {
                let (l, r) = bin(self, a, b, &mut aggs)?;
                Expression::Or(l, r)
            }
            K::And(a, b) => {
                let (l, r) = bin(self, a, b, &mut aggs)?;
                Expression::And(l, r)
            }
            K::Cmp(op, a, b) => {
                let (l, r) = bin(self, a, b, &mut aggs)?;
                Expression::Cmp(*op, l, r)
            }
            K::Add(a, b) => {
                let (l, r) = bin(self, a, b, &mut aggs)?;
                Expression::Add(l, r)
            }
            K::Sub(a, b) => {
                let (l, r) = bin(self, a, b, &mut aggs)?;
                Expression::Sub(l, r)
            }
            K::Mul(a, b) => {
                let (l, r) = bin(self, a, b, &mut aggs)?;
                Expression::Mul(l, r)
            }
            K::Div(a, b) => {
                let (l, r) = bin(self, a, b, &mut aggs)?;
                Expression::Div(l, r)
            }
            K::In {
                expr,
                list,
                negated,
            } => {
                let inner = self.expr(expr, aggs.as_deref_mut())?;
                let list = list
                    .iter()
                    .map(|x| self.expr(x, aggs.as_deref_mut()))
                    .collect::<Result<_, _>>()?;
                Expression::In {
                    expr: Box::new(inner),
                    list,
                    negated: *negated,
                }
            }
            K::Not(a) => Expression::Not(Box::new(self.expr(a, aggs)?)),
            K::UnaryMinus(a) => Expression::UnaryMinus(Box::new(self.expr(a, aggs)?)),
            K::UnaryPlus(a) => Expression::UnaryPlus(Box::new(self.expr(a, aggs)?)),
            K::Builtin(b, args) => {
                let args = args
                    .iter()
                    .map(|x| self.expr(x, aggs.as_deref_mut()))
                    .collect::<Result<_, _>>()?;
                Expression::Builtin(*b, args)
            }
            K::Function {
                iri,
                args,
                distinct,
            } => {
                let args = args
                    .iter()
                    .map(|x| self.expr(x, aggs.as_deref_mut()))
                    .collect::<Result<_, _>>()?;
                Expression::Function {
                    iri: encode_iri(iri),
                    args,
                    distinct: *distinct,
                }
            }
            K::Exists(g) => Expression::Exists {
                negated: false,
                pattern: Box::new(self.group(g)?),
            },
            K::NotExists(g) => Expression::Exists {
                negated: true,
                pattern: Box::new(self.group(g)?),
            },
            K::Aggregate {
                func,
                distinct,
                expr,
                separator,
            } => {
                let Some(aggs) = aggs else {
                    return Err(err(
                        e.span,
                        "aggregate calls are only allowed in SELECT, HAVING, or ORDER BY \
                         of a grouped query",
                    ));
                };
                let inner = match expr {
                    Some(x) => Some(self.expr(x, None)?),
                    None => None,
                };
                let var = self.vars.fresh("agg");
                aggs.push((
                    var,
                    AggregateExpr {
                        func: *func,
                        distinct: *distinct,
                        expr: inner,
                        separator: separator.clone(),
                    },
                ));
                Expression::Var(var)
            }
            K::Term(t) => match &t.kind {
                ast::TermKind::TripleTerm(tp) if has_vars(tp) => {
                    let s = self.term_expr(&tp.s)?;
                    let p = match &tp.p {
                        ast::Verb::Term(p) => self.term_expr(p)?,
                        ast::Verb::Path(_) => {
                            return Err(err(t.span, "property path inside a triple term"));
                        }
                    };
                    let o = self.term_expr(&tp.o)?;
                    Expression::TripleTerm {
                        s: Box::new(s),
                        p: Box::new(p),
                        o: Box::new(o),
                    }
                }
                _ => match self.term(t)? {
                    P::Var(v) => Expression::Var(v),
                    P::Term(bytes) => Expression::Term(bytes),
                    P::Triple(_) => unreachable!("ground triple terms encode as bytes"),
                },
            },
        })
    }

    fn term_expr(&mut self, t: &ast::Term) -> Result<Expression, TranslateError> {
        match self.term(t)? {
            P::Var(v) => Ok(Expression::Var(v)),
            P::Term(bytes) => Ok(Expression::Term(bytes)),
            P::Triple(tp) => Ok(Expression::TripleTerm {
                s: Box::new(p_expr(tp.s)),
                p: Box::new(p_expr(tp.p)),
                o: Box::new(p_expr(tp.o)),
            }),
        }
    }
}

fn p_expr(p: P) -> Expression {
    match p {
        P::Var(v) => Expression::Var(v),
        P::Term(bytes) => Expression::Term(bytes),
        P::Triple(tp) => Expression::TripleTerm {
            s: Box::new(p_expr(tp.s)),
            p: Box::new(p_expr(tp.p)),
            o: Box::new(p_expr(tp.o)),
        },
    }
}

fn has_vars(tp: &ast::TriplePattern) -> bool {
    fn term(t: &ast::Term) -> bool {
        match &t.kind {
            ast::TermKind::Var(_) => true,
            ast::TermKind::TripleTerm(tp) => has_vars(tp),
            _ => false,
        }
    }
    term(&tp.s)
        || match &tp.p {
            ast::Verb::Term(p) => term(p),
            ast::Verb::Path(_) => false,
        }
        || term(&tp.o)
}

fn contains_aggregate(e: &ast::Expr) -> bool {
    use ast::ExprKind as K;
    match &*e.kind {
        K::Aggregate { .. } => true,
        K::Or(a, b)
        | K::And(a, b)
        | K::Cmp(_, a, b)
        | K::Add(a, b)
        | K::Sub(a, b)
        | K::Mul(a, b)
        | K::Div(a, b) => contains_aggregate(a) || contains_aggregate(b),
        K::In { expr, list, .. } => contains_aggregate(expr) || list.iter().any(contains_aggregate),
        K::Not(a) | K::UnaryMinus(a) | K::UnaryPlus(a) => contains_aggregate(a),
        K::Builtin(_, args) => args.iter().any(contains_aggregate),
        K::Function { args, .. } => args.iter().any(contains_aggregate),
        K::Exists(_) | K::NotExists(_) | K::Term(_) => false,
    }
}

/// `Join(unit, X) = X` (the spec's simplification step).
fn join(a: Algebra, b: Algebra) -> Algebra {
    match (a, b) {
        (Algebra::Bgp(v), b) if v.is_empty() => b,
        (a, Algebra::Bgp(v)) if v.is_empty() => a,
        // Adjacent BGPs merge (only produced by triples runs).
        (Algebra::Bgp(mut x), Algebra::Bgp(y)) => {
            x.extend(y);
            Algebra::Bgp(x)
        }
        (a, b) => Algebra::Join(Box::new(a), Box::new(b)),
    }
}

fn path_expr(p: &ast::Path) -> PathExpr {
    match p {
        ast::Path::Iri(iri) => PathExpr::Link(encode_iri(iri)),
        ast::Path::Inverse(x) => PathExpr::Inverse(Box::new(path_expr(x))),
        ast::Path::Seq(a, b) => PathExpr::Seq(Box::new(path_expr(a)), Box::new(path_expr(b))),
        ast::Path::Alt(a, b) => PathExpr::Alt(Box::new(path_expr(a)), Box::new(path_expr(b))),
        ast::Path::ZeroOrMore(x) => PathExpr::ZeroOrMore(Box::new(path_expr(x))),
        ast::Path::OneOrMore(x) => PathExpr::OneOrMore(Box::new(path_expr(x))),
        ast::Path::ZeroOrOne(x) => PathExpr::ZeroOrOne(Box::new(path_expr(x))),
        ast::Path::Nps(items) => PathExpr::Nps(
            items
                .iter()
                .map(|(iri, inv)| (encode_iri(iri), *inv))
                .collect(),
        ),
    }
}

/// §18.2.1 visible (in-scope) variables of an algebra tree, in first-
/// mention order.
pub fn visible_vars(a: &Algebra) -> Vec<VarId> {
    let mut out = Vec::new();
    fn push(out: &mut Vec<VarId>, v: VarId) {
        if !out.contains(&v) {
            out.push(v);
        }
    }
    fn p(out: &mut Vec<VarId>, x: &P) {
        match x {
            P::Var(v) => push(out, *v),
            P::Term(_) => {}
            P::Triple(tp) => {
                p(out, &tp.s);
                p(out, &tp.p);
                p(out, &tp.o);
            }
        }
    }
    fn walk(out: &mut Vec<VarId>, a: &Algebra) {
        match a {
            Algebra::Bgp(ts) => {
                for t in ts {
                    p(out, &t.s);
                    p(out, &t.p);
                    p(out, &t.o);
                }
            }
            Algebra::Path { s, o, .. } => {
                p(out, s);
                p(out, o);
            }
            Algebra::Join(a, b) | Algebra::Union(a, b) => {
                walk(out, a);
                walk(out, b);
            }
            Algebra::LeftJoin { left, right, .. } => {
                walk(out, left);
                walk(out, right);
            }
            Algebra::Filter { input, .. }
            | Algebra::ToMultiSet(input)
            | Algebra::Distinct(input)
            | Algebra::Reduced(input)
            | Algebra::OrderBy { input, .. }
            | Algebra::Slice { input, .. } => walk(out, input),
            Algebra::Graph { graph, input } => {
                p(out, graph);
                walk(out, input);
            }
            Algebra::Service { target, input, .. } => {
                p(out, target);
                walk(out, input);
            }
            Algebra::Extend { input, var, .. } => {
                walk(out, input);
                push(out, *var);
            }
            // MINUS's right side does not bind.
            Algebra::Minus(a, _) => walk(out, a),
            Algebra::Table { vars, .. } => {
                for v in vars {
                    push(out, *v);
                }
            }
            Algebra::Group {
                keys, aggregates, ..
            } => {
                for (v, _) in keys {
                    push(out, *v);
                }
                for (v, _) in aggregates {
                    push(out, *v);
                }
            }
            Algebra::Project { vars, .. } => {
                for v in vars {
                    push(out, *v);
                }
            }
        }
    }
    walk(&mut out, a);
    out
}

fn err(span: Span, message: impl Into<String>) -> TranslateError {
    TranslateError {
        span,
        message: message.into(),
    }
}

fn encode_iri(iri: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(iri.len() + 1);
    concise::encode_iri(&mut out, iri);
    out
}

fn encode_literal(lexical: &str, kind: &ast::LiteralKind) -> Vec<u8> {
    let mut out = Vec::new();
    match kind {
        ast::LiteralKind::Plain => concise::encode_simple(&mut out, lexical),
        ast::LiteralKind::Lang { tag, dir } => {
            let dir = dir.map(|d| match d {
                graphy_sparql_syntax::Dir::Ltr => graphy_core::Dir::Ltr,
                graphy_sparql_syntax::Dir::Rtl => graphy_core::Dir::Rtl,
            });
            concise::encode_lang(&mut out, lexical, &tag.to_ascii_lowercase(), dir)
        }
        ast::LiteralKind::Typed(dt) if dt == vocab::XSD_STRING => {
            concise::encode_simple(&mut out, lexical)
        }
        ast::LiteralKind::Typed(dt) => concise::encode_datatype(&mut out, lexical, dt),
    }
    out
}

// ---------------------------------------------------------------------------
// SPARQL Update translation (§18.2-adjacent; executed by graphy-engine
// through the M4 write pipeline).
// ---------------------------------------------------------------------------

/// A translated update request: operations in order, each self-contained
/// (its own variable table where patterns are involved).
#[derive(Debug, Clone)]
pub struct TranslatedUpdate {
    pub ops: Vec<UpdateOpT>,
}

/// One template/pattern quad. `g: None` = the operation's default target
/// (`WITH` when present, else the default graph).
#[derive(Debug, Clone)]
pub struct QuadPat {
    pub g: Option<P>,
    pub s: P,
    pub p: P,
    pub o: P,
}

/// One ground quad (concise bytes; `g: None` = default graph). InsertData
/// blank labels stay in the bytes — the executor freshens them per request.
pub type GroundQuad = (Option<Vec<u8>>, Vec<u8>, Vec<u8>, Vec<u8>);

#[derive(Debug, Clone)]
pub enum GraphTargetT {
    Default,
    Named(Vec<u8>),
    AllNamed,
    All,
}

#[derive(Debug, Clone)]
pub enum UpdateOpT {
    InsertData(Vec<GroundQuad>),
    DeleteData(Vec<GroundQuad>),
    /// Quads double as WHERE pattern and delete template (no blank nodes).
    DeleteWhere {
        vars: VarTable,
        quads: Vec<QuadPat>,
    },
    Modify {
        vars: VarTable,
        /// Concise IRI of the `WITH` graph.
        with: Option<Vec<u8>>,
        delete: Vec<QuadPat>,
        insert: Vec<QuadPat>,
        /// (default?, concise IRI) `USING` / `USING NAMED`.
        using: Vec<(bool, Vec<u8>)>,
        pattern: Algebra,
    },
    Load {
        silent: bool,
        source: Vec<u8>,
        into: Option<Vec<u8>>,
    },
    Clear {
        silent: bool,
        target: GraphTargetT,
    },
    Drop {
        silent: bool,
        target: GraphTargetT,
    },
    Create {
        silent: bool,
        graph: Vec<u8>,
    },
    Add {
        silent: bool,
        from: Option<Vec<u8>>,
        to: Option<Vec<u8>>,
    },
    Move {
        silent: bool,
        from: Option<Vec<u8>>,
        to: Option<Vec<u8>>,
    },
    Copy {
        silent: bool,
        from: Option<Vec<u8>>,
        to: Option<Vec<u8>>,
    },
}

/// Translate a parsed update request. Each operation gets an independent
/// variable table (blank-label scoping across operations is the parser's
/// job; solution scoping is per operation by definition).
pub fn translate_update(u: &ast::UpdateRequest) -> Result<TranslatedUpdate, TranslateError> {
    let mut ops = Vec::with_capacity(u.operations.len());
    for op in &u.operations {
        ops.push(translate_op(op)?);
    }
    Ok(TranslatedUpdate { ops })
}

fn translate_op(op: &ast::UpdateOp) -> Result<UpdateOpT, TranslateError> {
    let graph_ref = |g: &ast::GraphOrDefault| -> Option<Vec<u8>> {
        match g {
            ast::GraphOrDefault::Default => None,
            ast::GraphOrDefault::Graph(iri) => Some(encode_iri(iri)),
        }
    };
    Ok(match op {
        ast::UpdateOp::InsertData(quads) => UpdateOpT::InsertData(ground_quads(quads)?),
        ast::UpdateOp::DeleteData(quads) => UpdateOpT::DeleteData(ground_quads(quads)?),
        ast::UpdateOp::DeleteWhere(quads) => {
            let mut cx = Cx {
                vars: VarTable::default(),
            };
            let quads = quad_pats(&mut cx, quads)?;
            UpdateOpT::DeleteWhere {
                vars: cx.vars,
                quads,
            }
        }
        ast::UpdateOp::Modify {
            with,
            delete,
            insert,
            using,
            pattern,
        } => {
            let mut cx = Cx {
                vars: VarTable::default(),
            };
            // WHERE first: template variables must refer to pattern
            // bindings, and interning order keeps ids compact.
            let root = cx.group(pattern)?;
            let delete = match delete {
                Some(q) => quad_pats(&mut cx, q)?,
                None => Vec::new(),
            };
            let insert = match insert {
                Some(q) => quad_pats(&mut cx, q)?,
                None => Vec::new(),
            };
            UpdateOpT::Modify {
                vars: cx.vars,
                with: with.as_ref().map(|iri| encode_iri(iri)),
                delete,
                insert,
                using: using
                    .iter()
                    .map(|d| match d {
                        ast::DatasetClause::Default(iri) => (true, encode_iri(iri)),
                        ast::DatasetClause::Named(iri) => (false, encode_iri(iri)),
                    })
                    .collect(),
                pattern: root,
            }
        }
        ast::UpdateOp::Load {
            silent,
            source,
            into,
        } => UpdateOpT::Load {
            silent: *silent,
            source: encode_iri(source),
            into: into.as_ref().map(|iri| encode_iri(iri)),
        },
        ast::UpdateOp::Clear { silent, target } => UpdateOpT::Clear {
            silent: *silent,
            target: graph_target(target),
        },
        ast::UpdateOp::Drop { silent, target } => UpdateOpT::Drop {
            silent: *silent,
            target: graph_target(target),
        },
        ast::UpdateOp::Create { silent, graph } => UpdateOpT::Create {
            silent: *silent,
            graph: encode_iri(graph),
        },
        ast::UpdateOp::Add { silent, from, to } => UpdateOpT::Add {
            silent: *silent,
            from: graph_ref(from),
            to: graph_ref(to),
        },
        ast::UpdateOp::Move { silent, from, to } => UpdateOpT::Move {
            silent: *silent,
            from: graph_ref(from),
            to: graph_ref(to),
        },
        ast::UpdateOp::Copy { silent, from, to } => UpdateOpT::Copy {
            silent: *silent,
            from: graph_ref(from),
            to: graph_ref(to),
        },
    })
}

fn graph_target(t: &ast::GraphTarget) -> GraphTargetT {
    match t {
        ast::GraphTarget::Graph(iri) => GraphTargetT::Named(encode_iri(iri)),
        ast::GraphTarget::Default => GraphTargetT::Default,
        ast::GraphTarget::Named => GraphTargetT::AllNamed,
        ast::GraphTarget::All => GraphTargetT::All,
    }
}

/// Ground quads for INSERT/DELETE DATA (the parser enforced groundness;
/// blank nodes are allowed in INSERT DATA and encode as blank terms).
fn ground_quads(quads: &[ast::Quad]) -> Result<Vec<GroundQuad>, TranslateError> {
    quads
        .iter()
        .map(|q| {
            let g = match &q.graph {
                None => None,
                Some(t) => Some(ground_term(t)?),
            };
            let p = match &q.triple.p {
                ast::Verb::Term(p) => ground_term(p)?,
                ast::Verb::Path(_) => {
                    return Err(err(q.triple.s.span, "property path in ground data"));
                }
            };
            Ok((g, ground_term(&q.triple.s)?, p, ground_term(&q.triple.o)?))
        })
        .collect()
}

fn ground_term(t: &ast::Term) -> Result<Vec<u8>, TranslateError> {
    Ok(match &t.kind {
        ast::TermKind::Iri(iri) => encode_iri(iri),
        ast::TermKind::Literal { lexical, kind } => encode_literal(lexical, kind),
        ast::TermKind::BlankNode(label) => {
            let mut out = Vec::new();
            concise::encode_blank(&mut out, label);
            out
        }
        ast::TermKind::TripleTerm(tp) => {
            let s = ground_term(&tp.s)?;
            let p = match &tp.p {
                ast::Verb::Term(p) => ground_term(p)?,
                ast::Verb::Path(_) => {
                    return Err(err(t.span, "property path inside a triple term"));
                }
            };
            let o = ground_term(&tp.o)?;
            let mut out = Vec::new();
            concise::encode_triple_term(&mut out, &s, &p, &o);
            out
        }
        ast::TermKind::Var(v) => return Err(err(t.span, format!("variable ?{v} in ground data"))),
    })
}

/// Template/pattern quads: terms through the shared `Cx` so variables and
/// template blank nodes (`.b:` vars) line up with the WHERE pattern.
fn quad_pats(cx: &mut Cx, quads: &[ast::Quad]) -> Result<Vec<QuadPat>, TranslateError> {
    quads
        .iter()
        .map(|q| {
            let g = match &q.graph {
                None => None,
                Some(t) => Some(cx.term(t)?),
            };
            let p = match &q.triple.p {
                ast::Verb::Term(p) => cx.term(p)?,
                ast::Verb::Path(_) => {
                    return Err(err(q.triple.s.span, "property path in a template"));
                }
            };
            Ok(QuadPat {
                g,
                s: cx.term(&q.triple.s)?,
                p,
                o: cx.term(&q.triple.o)?,
            })
        })
        .collect()
}
