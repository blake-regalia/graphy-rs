//! Engine-independent rewrites (doc 04 §4): only semantics-preserving,
//! cost-model-free rules — filter decomposition and pushdown, constant
//! folding that respects SPARQL's error semantics, trivial eliminations,
//! and canonical BGP ordering (plan-cache keying). Cost-based work
//! belongs to the engine (doc 05).

use std::collections::HashSet;

use graphy_core::{concise, vocab, TermRef};

use crate::algebra::*;
use crate::translate::visible_vars;

/// Apply the standard rewrite pipeline: fold constants and eliminate
/// trivial nodes bottom-up, split and push filters, then canonicalize
/// BGP order. Deterministic; idempotent on its own output.
pub fn rewrite(root: Algebra) -> Algebra {
    canonicalize(push_filters(simplify(root)))
}

/// Rebuild the tree bottom-up through `f`: children are transformed
/// first, then the (rebuilt) node itself is handed to `f`. The identity
/// closure reproduces the tree; composable with the named passes
/// ([`simplify`], [`push_filters`], [`canonicalize`]) for custom rewrite
/// pipelines (plan §M13d).
pub fn transform_bottom_up(a: Algebra, f: &mut impl FnMut(Algebra) -> Algebra) -> Algebra {
    let rebuilt = match a {
        Algebra::Join(l, r) => Algebra::Join(
            Box::new(transform_bottom_up(*l, f)),
            Box::new(transform_bottom_up(*r, f)),
        ),
        Algebra::Union(l, r) => Algebra::Union(
            Box::new(transform_bottom_up(*l, f)),
            Box::new(transform_bottom_up(*r, f)),
        ),
        Algebra::Minus(l, r) => Algebra::Minus(
            Box::new(transform_bottom_up(*l, f)),
            Box::new(transform_bottom_up(*r, f)),
        ),
        Algebra::LeftJoin { left, right, expr } => Algebra::LeftJoin {
            left: Box::new(transform_bottom_up(*left, f)),
            right: Box::new(transform_bottom_up(*right, f)),
            expr,
        },
        Algebra::Filter { expr, input } => Algebra::Filter {
            expr,
            input: Box::new(transform_bottom_up(*input, f)),
        },
        Algebra::Graph { graph, input } => Algebra::Graph {
            graph,
            input: Box::new(transform_bottom_up(*input, f)),
        },
        Algebra::Service {
            silent,
            target,
            input,
        } => Algebra::Service {
            silent,
            target,
            input: Box::new(transform_bottom_up(*input, f)),
        },
        Algebra::Extend { input, var, expr } => Algebra::Extend {
            input: Box::new(transform_bottom_up(*input, f)),
            var,
            expr,
        },
        Algebra::ToMultiSet(input) => Algebra::ToMultiSet(Box::new(transform_bottom_up(*input, f))),
        Algebra::Group {
            keys,
            aggregates,
            input,
        } => Algebra::Group {
            keys,
            aggregates,
            input: Box::new(transform_bottom_up(*input, f)),
        },
        Algebra::OrderBy { input, conditions } => Algebra::OrderBy {
            input: Box::new(transform_bottom_up(*input, f)),
            conditions,
        },
        Algebra::Project { input, vars } => Algebra::Project {
            input: Box::new(transform_bottom_up(*input, f)),
            vars,
        },
        Algebra::Distinct(input) => Algebra::Distinct(Box::new(transform_bottom_up(*input, f))),
        Algebra::Reduced(input) => Algebra::Reduced(Box::new(transform_bottom_up(*input, f))),
        Algebra::Slice {
            input,
            offset,
            limit,
        } => Algebra::Slice {
            input: Box::new(transform_bottom_up(*input, f)),
            offset,
            limit,
        },
        leaf @ (Algebra::Bgp(_) | Algebra::Path { .. } | Algebra::Table { .. }) => leaf,
    };
    f(rebuilt)
}

/// The empty table (joins annihilate on it; unions/left-joins elide it).
fn empty() -> Algebra {
    Algebra::Table {
        vars: Vec::new(),
        rows: Vec::new(),
    }
}

fn is_empty(a: &Algebra) -> bool {
    matches!(a, Algebra::Table { rows, .. } if rows.is_empty())
}

fn is_unit(a: &Algebra) -> bool {
    matches!(a, Algebra::Bgp(ts) if ts.is_empty())
}

// ---------------------------------------------------------------------------
// Bottom-up simplification: constant folding + trivial eliminations.
// ---------------------------------------------------------------------------

pub fn simplify(a: Algebra) -> Algebra {
    match a {
        Algebra::Bgp(_) | Algebra::Table { .. } | Algebra::Path { .. } => a,
        Algebra::Join(l, r) => {
            let l = simplify(*l);
            let r = simplify(*r);
            if is_empty(&l) || is_empty(&r) {
                return empty();
            }
            if is_unit(&l) {
                return r;
            }
            if is_unit(&r) {
                return l;
            }
            Algebra::Join(Box::new(l), Box::new(r))
        }
        Algebra::LeftJoin { left, right, expr } => {
            let left = simplify(*left);
            let right = simplify(*right);
            if is_empty(&left) {
                return empty();
            }
            // An empty optional side never extends anything.
            if is_empty(&right) {
                return left;
            }
            let expr = expr.map(fold);
            if let Some(e) = &expr {
                if ebv_const(e) == Some(false) {
                    // The condition never holds: rows pass through bare.
                    return left;
                }
            }
            let expr = match expr {
                Some(e) if ebv_const(&e) == Some(true) => None,
                other => other,
            };
            Algebra::LeftJoin {
                left: Box::new(left),
                right: Box::new(right),
                expr,
            }
        }
        Algebra::Filter { expr, input } => {
            let input = simplify(*input);
            if is_empty(&input) {
                return empty();
            }
            let expr = fold(expr);
            match ebv_const(&expr) {
                Some(true) => input,
                Some(false) => empty(),
                None => Algebra::Filter {
                    expr,
                    input: Box::new(input),
                },
            }
        }
        Algebra::Union(l, r) => {
            let l = simplify(*l);
            let r = simplify(*r);
            if is_empty(&l) {
                return r;
            }
            if is_empty(&r) {
                return l;
            }
            Algebra::Union(Box::new(l), Box::new(r))
        }
        Algebra::Graph { graph, input } => {
            let input = simplify(*input);
            if is_empty(&input) {
                return empty();
            }
            Algebra::Graph {
                graph,
                input: Box::new(input),
            }
        }
        Algebra::Service {
            silent,
            target,
            input,
        } => Algebra::Service {
            silent,
            target,
            // SERVICE subtrees are shipped elsewhere; simplify but never
            // assume emptiness semantics across the boundary.
            input: Box::new(simplify(*input)),
        },
        Algebra::Extend { input, var, expr } => {
            let input = simplify(*input);
            if is_empty(&input) {
                return empty();
            }
            Algebra::Extend {
                input: Box::new(input),
                var,
                expr: fold(expr),
            }
        }
        Algebra::Minus(l, r) => {
            let l = simplify(*l);
            let r = simplify(*r);
            if is_empty(&l) {
                return empty();
            }
            if is_empty(&r) {
                return l;
            }
            Algebra::Minus(Box::new(l), Box::new(r))
        }
        Algebra::ToMultiSet(x) => Algebra::ToMultiSet(Box::new(simplify(*x))),
        Algebra::Group {
            keys,
            aggregates,
            input,
        } => Algebra::Group {
            keys: keys.into_iter().map(|(v, e)| (v, e.map(fold))).collect(),
            aggregates,
            // A Group over the empty input still yields no groups (with
            // keys) or one empty group (without) — leave that to the
            // engine; only the input simplifies.
            input: Box::new(simplify(*input)),
        },
        Algebra::OrderBy { input, conditions } => {
            let input = simplify(*input);
            if is_empty(&input) {
                return empty();
            }
            Algebra::OrderBy {
                input: Box::new(input),
                conditions: conditions.into_iter().map(|(e, d)| (fold(e), d)).collect(),
            }
        }
        Algebra::Project { input, vars } => {
            let input = simplify(*input);
            Algebra::Project {
                input: Box::new(input),
                vars,
            }
        }
        Algebra::Distinct(x) => {
            let x = simplify(*x);
            if is_empty(&x) {
                return empty();
            }
            Algebra::Distinct(Box::new(x))
        }
        Algebra::Reduced(x) => {
            let x = simplify(*x);
            if is_empty(&x) {
                return empty();
            }
            Algebra::Reduced(Box::new(x))
        }
        Algebra::Slice {
            input,
            offset,
            limit,
        } => {
            let input = simplify(*input);
            if is_empty(&input) && limit != Some(0) {
                return empty();
            }
            Algebra::Slice {
                input: Box::new(input),
                offset,
                limit,
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Constant folding (SPARQL error semantics: an erroring operand only
// folds where the spec's truth table ignores it).
// ---------------------------------------------------------------------------

fn fold(e: Expression) -> Expression {
    use Expression as E;
    match e {
        E::And(a, b) => {
            let a = fold(*a);
            let b = fold(*b);
            // FALSE && anything = FALSE (even error); TRUE && x = x.
            match (ebv_const(&a), ebv_const(&b)) {
                (Some(false), _) | (_, Some(false)) => bool_expr(false),
                (Some(true), _) => b,
                (_, Some(true)) => a,
                _ => E::And(Box::new(a), Box::new(b)),
            }
        }
        E::Or(a, b) => {
            let a = fold(*a);
            let b = fold(*b);
            match (ebv_const(&a), ebv_const(&b)) {
                (Some(true), _) | (_, Some(true)) => bool_expr(true),
                (Some(false), _) => b,
                (_, Some(false)) => a,
                _ => E::Or(Box::new(a), Box::new(b)),
            }
        }
        E::Not(a) => {
            let a = fold(*a);
            match ebv_const(&a) {
                Some(v) => bool_expr(!v),
                None => E::Not(Box::new(a)),
            }
        }
        E::Cmp(op, a, b) => {
            let a = fold(*a);
            let b = fold(*b);
            if let (Some(x), Some(y)) = (int_const(&a), int_const(&b)) {
                let v = match op {
                    CmpOp::Eq => x == y,
                    CmpOp::Ne => x != y,
                    CmpOp::Lt => x < y,
                    CmpOp::Le => x <= y,
                    CmpOp::Gt => x > y,
                    CmpOp::Ge => x >= y,
                };
                return bool_expr(v);
            }
            E::Cmp(op, Box::new(a), Box::new(b))
        }
        E::Add(a, b) => fold_arith(*a, *b, i64::checked_add, E::Add),
        E::Sub(a, b) => fold_arith(*a, *b, i64::checked_sub, E::Sub),
        E::Mul(a, b) => fold_arith(*a, *b, i64::checked_mul, E::Mul),
        E::Div(a, b) => {
            // Integer division yields xsd:decimal (§17.3 op:numeric-divide);
            // no integer-only fold.
            E::Div(Box::new(fold(*a)), Box::new(fold(*b)))
        }
        E::UnaryMinus(a) => {
            let a = fold(*a);
            match int_const(&a).and_then(i64::checked_neg) {
                Some(v) => int_expr(v),
                None => E::UnaryMinus(Box::new(a)),
            }
        }
        E::UnaryPlus(a) => {
            let a = fold(*a);
            match int_const(&a) {
                Some(_) => a,
                None => E::UnaryPlus(Box::new(a)),
            }
        }
        E::In {
            expr,
            list,
            negated,
        } => E::In {
            expr: Box::new(fold(*expr)),
            list: list.into_iter().map(fold).collect(),
            negated,
        },
        E::Builtin(b, args) => E::Builtin(b, args.into_iter().map(fold).collect()),
        E::Function {
            iri,
            args,
            distinct,
        } => E::Function {
            iri,
            args: args.into_iter().map(fold).collect(),
            distinct,
        },
        E::TripleTerm { s, p, o } => E::TripleTerm {
            s: Box::new(fold(*s)),
            p: Box::new(fold(*p)),
            o: Box::new(fold(*o)),
        },
        E::Term(_) | E::Var(_) | E::Exists { .. } => e,
    }
}

fn fold_arith(
    a: Expression,
    b: Expression,
    op: fn(i64, i64) -> Option<i64>,
    ctor: fn(Box<Expression>, Box<Expression>) -> Expression,
) -> Expression {
    let a = fold(a);
    let b = fold(b);
    if let (Some(x), Some(y)) = (int_const(&a), int_const(&b)) {
        if let Some(v) = op(x, y) {
            return int_expr(v);
        }
    }
    ctor(Box::new(a), Box::new(b))
}

/// Effective boolean value of a constant expression (§17.2.2), `None`
/// when unknown, non-constant, or a type error (errors must surface at
/// evaluation, not fold away).
fn ebv_const(e: &Expression) -> Option<bool> {
    let Expression::Term(bytes) = e else {
        return None;
    };
    match concise::decode(bytes).ok()? {
        TermRef::Literal(l) => {
            if l.lang().is_some() {
                return Some(!l.lexical().is_empty());
            }
            match l.datatype() {
                vocab::XSD_BOOLEAN => match l.lexical() {
                    "true" | "1" => Some(true),
                    "false" | "0" => Some(false),
                    _ => None,
                },
                vocab::XSD_STRING => Some(!l.lexical().is_empty()),
                vocab::XSD_INTEGER => Some(l.lexical().parse::<i64>().ok()? != 0),
                vocab::XSD_DECIMAL | vocab::XSD_DOUBLE | vocab::XSD_FLOAT => {
                    let v: f64 = l.lexical().parse().ok()?;
                    Some(v != 0.0 && !v.is_nan())
                }
                _ => None,
            }
        }
        _ => None,
    }
}

fn int_const(e: &Expression) -> Option<i64> {
    let Expression::Term(bytes) = e else {
        return None;
    };
    match concise::decode(bytes).ok()? {
        TermRef::Literal(l) if l.lang().is_none() && l.datatype() == vocab::XSD_INTEGER => {
            l.lexical().parse().ok()
        }
        _ => None,
    }
}

fn bool_expr(v: bool) -> Expression {
    let mut out = Vec::new();
    concise::encode_datatype(
        &mut out,
        if v { "true" } else { "false" },
        vocab::XSD_BOOLEAN,
    );
    Expression::Term(out)
}

fn int_expr(v: i64) -> Expression {
    let mut out = Vec::new();
    concise::encode_datatype(&mut out, &v.to_string(), vocab::XSD_INTEGER);
    Expression::Term(out)
}

// ---------------------------------------------------------------------------
// Filter decomposition + pushdown.
// ---------------------------------------------------------------------------

pub fn push_filters(a: Algebra) -> Algebra {
    match a {
        Algebra::Filter { expr, input } => {
            // Flatten a stack of filters into one conjunct list (keeps
            // the pass idempotent), push each, and re-wrap the leftovers
            // in collection order.
            let mut conjuncts = Vec::new();
            split_and(expr, &mut conjuncts);
            let mut inner = *input;
            while let Algebra::Filter { expr, input } = inner {
                split_and(expr, &mut conjuncts);
                inner = *input;
            }
            let mut acc = push_filters(inner);
            let mut kept = Vec::new();
            for c in conjuncts {
                match try_push(c, acc) {
                    (None, next) => acc = next,
                    (Some(c), next) => {
                        acc = next;
                        kept.push(c);
                    }
                }
            }
            for c in kept.into_iter().rev() {
                acc = Algebra::Filter {
                    expr: c,
                    input: Box::new(acc),
                };
            }
            acc
        }
        // Recurse structurally.
        Algebra::Join(l, r) => {
            Algebra::Join(Box::new(push_filters(*l)), Box::new(push_filters(*r)))
        }
        Algebra::LeftJoin { left, right, expr } => Algebra::LeftJoin {
            left: Box::new(push_filters(*left)),
            right: Box::new(push_filters(*right)),
            expr,
        },
        Algebra::Union(l, r) => {
            Algebra::Union(Box::new(push_filters(*l)), Box::new(push_filters(*r)))
        }
        Algebra::Graph { graph, input } => Algebra::Graph {
            graph,
            input: Box::new(push_filters(*input)),
        },
        Algebra::Service {
            silent,
            target,
            input,
        } => Algebra::Service {
            silent,
            target,
            input: Box::new(push_filters(*input)),
        },
        Algebra::Extend { input, var, expr } => Algebra::Extend {
            input: Box::new(push_filters(*input)),
            var,
            expr,
        },
        Algebra::Minus(l, r) => {
            Algebra::Minus(Box::new(push_filters(*l)), Box::new(push_filters(*r)))
        }
        Algebra::ToMultiSet(x) => Algebra::ToMultiSet(Box::new(push_filters(*x))),
        Algebra::Group {
            keys,
            aggregates,
            input,
        } => Algebra::Group {
            keys,
            aggregates,
            input: Box::new(push_filters(*input)),
        },
        Algebra::OrderBy { input, conditions } => Algebra::OrderBy {
            input: Box::new(push_filters(*input)),
            conditions,
        },
        Algebra::Project { input, vars } => Algebra::Project {
            input: Box::new(push_filters(*input)),
            vars,
        },
        Algebra::Distinct(x) => Algebra::Distinct(Box::new(push_filters(*x))),
        Algebra::Reduced(x) => Algebra::Reduced(Box::new(push_filters(*x))),
        Algebra::Slice {
            input,
            offset,
            limit,
        } => Algebra::Slice {
            input: Box::new(push_filters(*input)),
            offset,
            limit,
        },
        leaf => leaf,
    }
}

fn split_and(e: Expression, out: &mut Vec<Expression>) {
    match e {
        Expression::And(a, b) => {
            split_and(*a, out);
            split_and(*b, out);
        }
        other => out.push(other),
    }
}

/// Try to sink one conjunct into `input`. Returns `(leftover, tree)` —
/// `None` leftover means it was absorbed.
fn try_push(c: Expression, input: Algebra) -> (Option<Expression>, Algebra) {
    // EXISTS references the full outer binding via substitution — never
    // move it.
    let Some(cv) = pushable_vars(&c) else {
        return (Some(c), input);
    };
    match input {
        Algebra::Join(l, r) => {
            let lv: HashSet<VarId> = visible_vars(&l).into_iter().collect();
            if cv.is_subset(&lv) {
                let (left, l2) = try_push(c, *l);
                let tree = Algebra::Join(Box::new(wrap(left, l2)), r);
                return (None, tree);
            }
            let rv: HashSet<VarId> = visible_vars(&r).into_iter().collect();
            if cv.is_subset(&rv) {
                let (left, r2) = try_push(c, *r);
                let tree = Algebra::Join(l, Box::new(wrap(left, r2)));
                return (None, tree);
            }
            (Some(c), Algebra::Join(l, r))
        }
        // Filters distribute over union branches unconditionally
        // (per-row semantics; unbound variables error → row dropped,
        // same on either side of the union).
        Algebra::Union(l, r) => {
            let (cl, l2) = try_push(c.clone(), *l);
            let (cr, r2) = try_push(c, *r);
            (
                None,
                Algebra::Union(Box::new(wrap(cl, l2)), Box::new(wrap(cr, r2))),
            )
        }
        Algebra::Graph { graph, input } => {
            // a condition referencing the graph VARIABLE must stay outside:
            // the enumeration binds it onto solutions only after the inner
            // pattern evaluates (§18.3), so inside the node it is unbound
            if let P::Var(v) = &graph {
                if cv.contains(v) {
                    return (Some(c), Algebra::Graph { graph, input });
                }
            }
            let (left, inner) = try_push(c, *input);
            (
                None,
                Algebra::Graph {
                    graph,
                    input: Box::new(wrap(left, inner)),
                },
            )
        }
        // Left side of an OPTIONAL: rows filtered before or after the
        // left join agree when the condition only sees left variables.
        Algebra::LeftJoin { left, right, expr } => {
            let lv: HashSet<VarId> = visible_vars(&left).into_iter().collect();
            if cv.is_subset(&lv) {
                let (leftover, l2) = try_push(c, *left);
                return (
                    None,
                    Algebra::LeftJoin {
                        left: Box::new(wrap(leftover, l2)),
                        right,
                        expr,
                    },
                );
            }
            (Some(c), Algebra::LeftJoin { left, right, expr })
        }
        // Below an Extend only when the condition ignores the new
        // binding.
        Algebra::Extend { input, var, expr } => {
            if !cv.contains(&var) {
                let (leftover, inner) = try_push(c, *input);
                return (
                    None,
                    Algebra::Extend {
                        input: Box::new(wrap(leftover, inner)),
                        var,
                        expr,
                    },
                );
            }
            (Some(c), Algebra::Extend { input, var, expr })
        }
        // MINUS's left side keeps its rows independently of the right.
        Algebra::Minus(l, r) => {
            let (leftover, l2) = try_push(c, *l);
            (None, Algebra::Minus(Box::new(wrap(leftover, l2)), r))
        }
        // Scope / semantics boundaries: Service (shipped), ToMultiSet +
        // Project (subquery scope), Group (pre- vs post-aggregation),
        // Slice/OrderBy/Distinct (cardinality-sensitive), leaves.
        other => (Some(c), other),
    }
}

fn wrap(leftover: Option<Expression>, tree: Algebra) -> Algebra {
    match leftover {
        Some(expr) => Algebra::Filter {
            expr,
            input: Box::new(tree),
        },
        None => tree,
    }
}

/// The variables a conjunct depends on, or `None` when it must not move
/// (contains EXISTS, whose substitute semantics see every outer binding,
/// or a non-deterministic call).
fn pushable_vars(e: &Expression) -> Option<HashSet<VarId>> {
    let mut out = HashSet::new();
    fn walk(e: &Expression, out: &mut HashSet<VarId>) -> bool {
        use Expression as E;
        match e {
            E::Exists { .. } => false,
            E::Builtin(b, args) => {
                // Non-deterministic builtins must not change evaluation
                // count or position.
                if matches!(
                    b,
                    Builtin::Rand | Builtin::BNode | Builtin::Uuid | Builtin::StrUuid
                ) {
                    return false;
                }
                for a in args {
                    if !walk(a, out) {
                        return false;
                    }
                }
                true
            }
            E::Var(v) => {
                out.insert(*v);
                true
            }
            E::Term(_) => true,
            E::Or(a, b)
            | E::And(a, b)
            | E::Cmp(_, a, b)
            | E::Add(a, b)
            | E::Sub(a, b)
            | E::Mul(a, b)
            | E::Div(a, b) => walk(a, out) && walk(b, out),
            E::In { expr, list, .. } => walk(expr, out) && list.iter().all(|x| walk(x, out)),
            E::Not(a) | E::UnaryMinus(a) | E::UnaryPlus(a) => walk(a, out),
            E::Function { args, .. } => args.iter().all(|x| walk(x, out)),
            E::TripleTerm { s, p, o } => walk(s, out) && walk(p, out) && walk(o, out),
        }
    }
    walk(e, &mut out).then_some(out)
}

// ---------------------------------------------------------------------------
// Canonical BGP ordering (plan-cache keying; BGPs are pattern sets).
// ---------------------------------------------------------------------------

pub fn canonicalize(a: Algebra) -> Algebra {
    map_tree(a, &mut |node| {
        if let Algebra::Bgp(ts) = node {
            let mut ts = ts;
            ts.sort();
            Algebra::Bgp(ts)
        } else {
            node
        }
    })
}

/// Bottom-up structural map.
fn map_tree(a: Algebra, f: &mut impl FnMut(Algebra) -> Algebra) -> Algebra {
    let a = match a {
        Algebra::Join(l, r) => Algebra::Join(Box::new(map_tree(*l, f)), Box::new(map_tree(*r, f))),
        Algebra::LeftJoin { left, right, expr } => Algebra::LeftJoin {
            left: Box::new(map_tree(*left, f)),
            right: Box::new(map_tree(*right, f)),
            expr,
        },
        Algebra::Filter { expr, input } => Algebra::Filter {
            expr,
            input: Box::new(map_tree(*input, f)),
        },
        Algebra::Union(l, r) => {
            Algebra::Union(Box::new(map_tree(*l, f)), Box::new(map_tree(*r, f)))
        }
        Algebra::Graph { graph, input } => Algebra::Graph {
            graph,
            input: Box::new(map_tree(*input, f)),
        },
        Algebra::Service {
            silent,
            target,
            input,
        } => Algebra::Service {
            silent,
            target,
            input: Box::new(map_tree(*input, f)),
        },
        Algebra::Extend { input, var, expr } => Algebra::Extend {
            input: Box::new(map_tree(*input, f)),
            var,
            expr,
        },
        Algebra::Minus(l, r) => {
            Algebra::Minus(Box::new(map_tree(*l, f)), Box::new(map_tree(*r, f)))
        }
        Algebra::ToMultiSet(x) => Algebra::ToMultiSet(Box::new(map_tree(*x, f))),
        Algebra::Group {
            keys,
            aggregates,
            input,
        } => Algebra::Group {
            keys,
            aggregates,
            input: Box::new(map_tree(*input, f)),
        },
        Algebra::OrderBy { input, conditions } => Algebra::OrderBy {
            input: Box::new(map_tree(*input, f)),
            conditions,
        },
        Algebra::Project { input, vars } => Algebra::Project {
            input: Box::new(map_tree(*input, f)),
            vars,
        },
        Algebra::Distinct(x) => Algebra::Distinct(Box::new(map_tree(*x, f))),
        Algebra::Reduced(x) => Algebra::Reduced(Box::new(map_tree(*x, f))),
        Algebra::Slice {
            input,
            offset,
            limit,
        } => Algebra::Slice {
            input: Box::new(map_tree(*input, f)),
            offset,
            limit,
        },
        leaf => leaf,
    };
    f(a)
}

// ---------------------------------------------------------------------------
// Well-designed-pattern analysis (doc 04 §4: unlocks simpler left-join
// strategies in the engine).
// ---------------------------------------------------------------------------

/// Pérez et al. well-designedness over the OPTIONAL structure: for every
/// `LeftJoin(A, B)`, any variable of `B` that also occurs OUTSIDE the
/// left join must occur in `A`. Filters and expressions count as
/// occurrences.
pub fn well_designed(root: &Algebra) -> bool {
    check(root, &HashSet::new())
}

fn check(a: &Algebra, outside: &HashSet<VarId>) -> bool {
    match a {
        Algebra::LeftJoin { left, right, expr } => {
            let a_vars: HashSet<VarId> = all_vars(left).into_iter().collect();
            let b_vars: HashSet<VarId> = {
                let mut v = all_vars(right);
                if let Some(e) = expr {
                    expr_vars(e, &mut v);
                }
                v.into_iter().collect()
            };
            for v in b_vars.intersection(outside) {
                if !a_vars.contains(v) {
                    return false;
                }
            }
            // Inside A: everything in B counts as outside-of-A's own
            // nested optionals, and vice versa.
            let mut outside_a = outside.clone();
            outside_a.extend(b_vars.iter().copied());
            let mut outside_b = outside.clone();
            outside_b.extend(a_vars.iter().copied());
            check(left, &outside_a) && check(right, &outside_b)
        }
        Algebra::Join(l, r) | Algebra::Union(l, r) | Algebra::Minus(l, r) => {
            let lv = all_vars(l);
            let rv = all_vars(r);
            let mut outside_l = outside.clone();
            outside_l.extend(rv.iter().copied());
            let mut outside_r = outside.clone();
            outside_r.extend(lv.iter().copied());
            check(l, &outside_l) && check(r, &outside_r)
        }
        Algebra::Filter { expr, input } => {
            let mut ov = outside.clone();
            let mut ev = Vec::new();
            expr_vars(expr, &mut ev);
            ov.extend(ev);
            check(input, &ov)
        }
        Algebra::Graph { input, .. }
        | Algebra::Service { input, .. }
        | Algebra::Extend { input, .. }
        | Algebra::OrderBy { input, .. }
        | Algebra::Project { input, .. }
        | Algebra::Slice { input, .. }
        | Algebra::Group { input, .. } => check(input, outside),
        Algebra::ToMultiSet(x) | Algebra::Distinct(x) | Algebra::Reduced(x) => check(x, outside),
        Algebra::Bgp(_) | Algebra::Path { .. } | Algebra::Table { .. } => true,
    }
}

/// Every variable occurring anywhere in a subtree (patterns AND
/// expressions — occurrence, not visibility).
fn all_vars(a: &Algebra) -> Vec<VarId> {
    let mut out = Vec::new();
    collect_vars(a, &mut out);
    out
}

fn collect_vars(a: &Algebra, out: &mut Vec<VarId>) {
    fn p(x: &P, out: &mut Vec<VarId>) {
        match x {
            P::Var(v) => out.push(*v),
            P::Term(_) => {}
            P::Triple(tp) => {
                p(&tp.s, out);
                p(&tp.p, out);
                p(&tp.o, out);
            }
        }
    }
    match a {
        Algebra::Bgp(ts) => {
            for t in ts {
                p(&t.s, out);
                p(&t.p, out);
                p(&t.o, out);
            }
        }
        Algebra::Path { s, o, .. } => {
            p(s, out);
            p(o, out);
        }
        Algebra::Join(l, r) | Algebra::Union(l, r) | Algebra::Minus(l, r) => {
            collect_vars(l, out);
            collect_vars(r, out);
        }
        Algebra::LeftJoin { left, right, expr } => {
            collect_vars(left, out);
            collect_vars(right, out);
            if let Some(e) = expr {
                expr_vars(e, out);
            }
        }
        Algebra::Filter { expr, input } => {
            expr_vars(expr, out);
            collect_vars(input, out);
        }
        Algebra::Graph { graph, input } => {
            p(graph, out);
            collect_vars(input, out);
        }
        Algebra::Service { target, input, .. } => {
            p(target, out);
            collect_vars(input, out);
        }
        Algebra::Extend { input, var, expr } => {
            collect_vars(input, out);
            out.push(*var);
            expr_vars(expr, out);
        }
        Algebra::Table { vars, .. } => out.extend(vars.iter().copied()),
        Algebra::ToMultiSet(x) | Algebra::Distinct(x) | Algebra::Reduced(x) => collect_vars(x, out),
        Algebra::Group {
            keys,
            aggregates,
            input,
        } => {
            for (v, e) in keys {
                out.push(*v);
                if let Some(e) = e {
                    expr_vars(e, out);
                }
            }
            for (v, agg) in aggregates {
                out.push(*v);
                if let Some(e) = &agg.expr {
                    expr_vars(e, out);
                }
            }
            collect_vars(input, out);
        }
        Algebra::OrderBy { input, conditions } => {
            for (e, _) in conditions {
                expr_vars(e, out);
            }
            collect_vars(input, out);
        }
        Algebra::Project { input, vars } => {
            out.extend(vars.iter().copied());
            collect_vars(input, out);
        }
        Algebra::Slice { input, .. } => collect_vars(input, out),
    }
}

fn expr_vars(e: &Expression, out: &mut Vec<VarId>) {
    use Expression as E;
    match e {
        E::Var(v) => out.push(*v),
        E::Term(_) => {}
        E::Or(a, b)
        | E::And(a, b)
        | E::Cmp(_, a, b)
        | E::Add(a, b)
        | E::Sub(a, b)
        | E::Mul(a, b)
        | E::Div(a, b) => {
            expr_vars(a, out);
            expr_vars(b, out);
        }
        E::In { expr, list, .. } => {
            expr_vars(expr, out);
            for x in list {
                expr_vars(x, out);
            }
        }
        E::Not(a) | E::UnaryMinus(a) | E::UnaryPlus(a) => expr_vars(a, out),
        E::Builtin(_, args) => {
            for a in args {
                expr_vars(a, out);
            }
        }
        E::Function { args, .. } => {
            for a in args {
                expr_vars(a, out);
            }
        }
        E::Exists { pattern, .. } => collect_vars(pattern, out),
        E::TripleTerm { s, p, o } => {
            expr_vars(s, out);
            expr_vars(p, out);
            expr_vars(o, out);
        }
    }
}
