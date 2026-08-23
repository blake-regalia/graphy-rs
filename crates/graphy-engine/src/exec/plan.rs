//! Physical planning (doc 05 §5): algebra → physical operator tree.
//! Leaf costing is **exact** (snapshot pattern counts); BGP join order
//! is greedy min-count with a connectivity bonus (DP over connected
//! subgraphs arrives with EXPLAIN); joins pick bind (IndexNestedLoop)
//! against base patterns and hash elsewhere.

use std::collections::HashSet;

use graphy_algebra::{AggregateExpr, Algebra, Expression, PathExpr, TriplePat, VarId, P};

use crate::eval::{pattern_col, Evaluator, Scope};
use crate::exec::expr::Prog;
use crate::EngineError;
use graphy_store::TermPos;

/// Planning failure: a construct the vectorized engine cannot run
/// (the caller falls back to the reference evaluator, which produces
/// the canonical error message where the construct is unsupported
/// everywhere).
#[derive(Debug)]
pub(crate) enum PlanError {
    Unsupported(&'static str),
    Engine(EngineError),
}

impl From<EngineError> for PlanError {
    fn from(e: EngineError) -> PlanError {
        PlanError::Engine(e)
    }
}

impl From<graphy_store::StoreError> for PlanError {
    fn from(e: graphy_store::StoreError) -> PlanError {
        PlanError::Engine(EngineError(e.to_string()))
    }
}

/// Plan-time graph scope: fixed (constants resolved) or the enclosing
/// `GRAPH ?var` enumeration (concrete scope arrives at runtime).
#[derive(Debug, Clone, Copy)]
pub(crate) enum PScope {
    Fixed(Scope),
    Var,
}

/// The physical plan tree. (`est` fields feed EXPLAIN, arriving with
/// the planner polish increment.)
#[allow(dead_code)]
#[derive(Debug, Clone)]
pub(crate) enum Phys {
    /// One empty row (the unit table).
    Unit,
    /// Provably empty (absent constant, invisible graph, …).
    Empty,
    /// Streaming driving scan of one triple pattern.
    Scan {
        pat: TriplePat,
        est: u64,
    },
    /// IndexNestedLoop (bind) join: probe `pat` per input row.
    BindJoin {
        input: Box<Phys>,
        pat: TriplePat,
        est: u64,
    },
    /// Property-path step per input row.
    BindPath {
        input: Box<Phys>,
        s: P,
        path: PathExpr,
        o: P,
    },
    /// Hash join on statically-certain shared vars (loose rows — with
    /// UNDEF keys — fall back to compatible-merge scanning).
    HashJoin {
        left: Box<Phys>,
        right: Box<Phys>,
        keys: Vec<VarId>,
    },
    LeftJoin {
        left: Box<Phys>,
        right: Box<Phys>,
        expr: Option<Expression>,
        keys: Vec<VarId>,
    },
    Minus {
        left: Box<Phys>,
        right: Box<Phys>,
    },
    Filter {
        input: Box<Phys>,
        prog: Prog,
    },
    Extend {
        input: Box<Phys>,
        var: VarId,
        prog: Prog,
    },
    Union(Box<Phys>, Box<Phys>),
    /// GRAPH <g> with the named column resolved at plan time.
    GraphConst {
        col: u64,
        input: Box<Phys>,
    },
    /// GRAPH ?g — enumerate named graphs, join the binding afterwards
    /// (§18.3).
    GraphVar {
        var: VarId,
        input: Box<Phys>,
    },
    Table {
        vars: Vec<VarId>,
        rows: Vec<Vec<Option<Vec<u8>>>>,
    },
    Group {
        keys: Vec<(VarId, Option<Expression>)>,
        aggregates: Vec<(VarId, AggregateExpr)>,
        input: Box<Phys>,
    },
    Sort {
        input: Box<Phys>,
        conditions: Vec<(Expression, bool)>,
    },
    Project {
        input: Box<Phys>,
        vars: Vec<VarId>,
    },
    Distinct {
        input: Box<Phys>,
    },
    Slice {
        input: Box<Phys>,
        offset: u64,
        limit: Option<u64>,
    },
}

/// Plan an algebra tree under a plan-time scope.
pub(crate) fn plan(ev: &Evaluator<'_>, a: &Algebra, scope: PScope) -> Result<Phys, PlanError> {
    Ok(match a {
        Algebra::Bgp(patterns) => plan_bgp(ev, Phys::Unit, patterns, scope, &HashSet::new())?,
        Algebra::Path { s, path, o } => Phys::BindPath {
            input: Box::new(Phys::Unit),
            s: s.clone(),
            path: path.clone(),
            o: o.clone(),
        },
        Algebra::Join(l, r) => {
            let left = plan(ev, l, scope)?;
            // Fuse a BGP / path right side into the left pipeline as
            // bind joins — the workhorse shape.
            match &**r {
                Algebra::Bgp(patterns) => {
                    let bound = certain(&left);
                    plan_bgp(ev, left, patterns, scope, &bound)?
                }
                Algebra::Path { s, path, o } => Phys::BindPath {
                    input: Box::new(left),
                    s: s.clone(),
                    path: path.clone(),
                    o: o.clone(),
                },
                _ => {
                    let right = plan(ev, r, scope)?;
                    let keys = key_vars(&left, &right);
                    Phys::HashJoin {
                        left: Box::new(left),
                        right: Box::new(right),
                        keys,
                    }
                }
            }
        }
        Algebra::LeftJoin { left, right, expr } => {
            let l = plan(ev, left, scope)?;
            let r = plan(ev, right, scope)?;
            let keys = key_vars(&l, &r);
            Phys::LeftJoin {
                left: Box::new(l),
                right: Box::new(r),
                expr: expr.clone(),
                keys,
            }
        }
        Algebra::Minus(l, r) => Phys::Minus {
            left: Box::new(plan(ev, l, scope)?),
            right: Box::new(plan(ev, r, scope)?),
        },
        Algebra::Filter { expr, input } => Phys::Filter {
            input: Box::new(plan(ev, input, scope)?),
            prog: Prog::compile(expr),
        },
        Algebra::Extend { input, var, expr } => Phys::Extend {
            input: Box::new(plan(ev, input, scope)?),
            var: *var,
            prog: Prog::compile(expr),
        },
        Algebra::Union(l, r) => {
            Phys::Union(Box::new(plan(ev, l, scope)?), Box::new(plan(ev, r, scope)?))
        }
        Algebra::Graph { graph, input } => match graph {
            P::Term(bytes) => {
                match pattern_col(ev.snap, bytes, TermPos::Graph).filter(|&c| c > 0) {
                    Some(col) if ev.named_visible(col) => Phys::GraphConst {
                        col,
                        input: Box::new(plan(ev, input, PScope::Fixed(Scope::Named(col)))?),
                    },
                    _ => Phys::Empty,
                }
            }
            P::Var(v) => Phys::GraphVar {
                var: *v,
                input: Box::new(plan(ev, input, PScope::Var)?),
            },
            P::Triple(_) => return Err(PlanError::Unsupported("triple term as graph name")),
        },
        Algebra::Service { .. } => return Err(PlanError::Unsupported("SERVICE")),
        Algebra::Table { vars, rows } => Phys::Table {
            vars: vars.clone(),
            rows: rows.clone(),
        },
        Algebra::ToMultiSet(x) => plan(ev, x, scope)?,
        Algebra::Group {
            keys,
            aggregates,
            input,
        } => Phys::Group {
            keys: keys.clone(),
            aggregates: aggregates.clone(),
            input: Box::new(plan(ev, input, scope)?),
        },
        Algebra::OrderBy { input, conditions } => Phys::Sort {
            input: Box::new(plan(ev, input, scope)?),
            conditions: conditions.clone(),
        },
        Algebra::Project { input, vars } => Phys::Project {
            input: Box::new(plan(ev, input, scope)?),
            vars: vars.clone(),
        },
        Algebra::Distinct(x) => Phys::Distinct {
            input: Box::new(plan(ev, x, scope)?),
        },
        Algebra::Reduced(x) => plan(ev, x, scope)?,
        Algebra::Slice {
            input,
            offset,
            limit,
        } => Phys::Slice {
            input: Box::new(plan(ev, input, scope)?),
            offset: *offset,
            limit: *limit,
        },
    })
}

/// Order a BGP's patterns and chain them onto `input` as Scan +
/// BindJoins. ≤ 10 patterns: exhaustive DP over subsets (exact leaf
/// counts, shared-variable damping, connectivity preferred); beyond:
/// greedy min-count with a connectivity bonus.
fn plan_bgp(
    ev: &Evaluator<'_>,
    input: Phys,
    patterns: &[TriplePat],
    scope: PScope,
    outer_bound: &HashSet<VarId>,
) -> Result<Phys, PlanError> {
    if patterns.is_empty() {
        return Ok(input);
    }
    let counts: Vec<u64> = patterns
        .iter()
        .map(|t| leaf_count(ev, t, scope))
        .collect::<Result<_, _>>()?;

    let order = if patterns.len() <= 10 {
        order_dp(patterns, &counts, outer_bound)
    } else {
        order_greedy(patterns, &counts, outer_bound)
    };

    let unit_input = matches!(input, Phys::Unit);
    let mut phys = input;
    let mut first = unit_input;
    for i in order {
        let t = patterns[i].clone();
        phys = if first {
            first = false;
            Phys::Scan {
                pat: t,
                est: counts[i],
            }
        } else {
            Phys::BindJoin {
                input: Box::new(phys),
                pat: t,
                est: counts[i],
            }
        };
    }
    Ok(phys)
}

/// Greedy: prefer patterns connected to the bound set, then min count.
fn order_greedy(
    patterns: &[TriplePat],
    counts: &[u64],
    outer_bound: &HashSet<VarId>,
) -> Vec<usize> {
    let mut remaining: Vec<usize> = (0..patterns.len()).collect();
    let mut bound = outer_bound.clone();
    let mut order = Vec::with_capacity(patterns.len());
    while !remaining.is_empty() {
        let pick = remaining
            .iter()
            .enumerate()
            .min_by_key(|(_, &i)| {
                let connected =
                    !bound.is_empty() && pat_vars(&patterns[i]).iter().any(|v| bound.contains(v));
                (u8::from(!connected), counts[i])
            })
            .map(|(k, _)| k)
            .unwrap();
        let i = remaining.remove(pick);
        bound.extend(pat_vars(&patterns[i]));
        order.push(i);
    }
    order
}

/// DP over pattern subsets (doc 05 §5.2, DPccp-style on the variable
/// connection graph): minimize the summed intermediate-cardinality
/// estimate. Estimates: exact leaf counts, one damping decade per
/// shared (already-bound) variable — bounded below by 1 row.
fn order_dp(patterns: &[TriplePat], counts: &[u64], outer_bound: &HashSet<VarId>) -> Vec<usize> {
    let n = patterns.len();
    // Local variable bit sets.
    let mut var_bits: std::collections::HashMap<VarId, u32> = std::collections::HashMap::new();
    let mut masks = vec![0u64; n];
    for (i, t) in patterns.iter().enumerate() {
        for v in pat_vars(t) {
            let next = var_bits.len() as u32;
            let bit = *var_bits.entry(v).or_insert(next);
            masks[i] |= 1u64 << bit;
        }
    }
    let outer_mask: u64 = outer_bound
        .iter()
        .filter_map(|v| var_bits.get(v))
        .fold(0, |m, &b| m | (1u64 << b));

    #[derive(Clone)]
    struct State {
        cost: f64,
        est: f64,
        vars: u64,
        order: Vec<usize>,
    }
    let full = (1usize << n) - 1;
    let mut dp: Vec<Option<State>> = vec![None; full + 1];
    for i in 0..n {
        let shared = ((masks[i] & outer_mask).count_ones()) as i32;
        let est = (counts[i] as f64 * 0.1f64.powi(shared)).max(1.0);
        dp[1 << i] = Some(State {
            cost: est,
            est,
            vars: masks[i] | outer_mask,
            order: vec![i],
        });
    }
    for set in 1..=full {
        let Some(cur) = dp[set].clone() else { continue };
        for j in 0..n {
            if set & (1 << j) != 0 {
                continue;
            }
            let shared = ((masks[j] & cur.vars).count_ones()) as i32;
            let est = if shared > 0 {
                (cur.est * counts[j] as f64 * 0.1f64.powi(shared + 1)).max(1.0)
            } else {
                // Cartesian: allowed, but its cost speaks for itself.
                (cur.est * counts[j] as f64).max(1.0)
            };
            let cost = cur.cost + est;
            let next = set | (1 << j);
            if dp[next].as_ref().is_none_or(|s| cost < s.cost) {
                let mut order = cur.order.clone();
                order.push(j);
                dp[next] = Some(State {
                    cost,
                    est,
                    vars: cur.vars | masks[j],
                    order,
                });
            }
        }
    }
    dp[full].take().map(|s| s.order).unwrap_or_default()
}

/// Exact leaf count of one pattern's constants under a plan scope
/// (`Var` scope counts across all graphs — an upper bound used only
/// for ordering).
fn leaf_count(ev: &Evaluator<'_>, t: &TriplePat, scope: PScope) -> Result<u64, PlanError> {
    match scope {
        PScope::Fixed(s) => Ok(ev.pattern_count(t, &s)?),
        PScope::Var => {
            let Some(mut pat) = ev.pattern_of(t, &Scope::Default, None)? else {
                return Ok(0);
            };
            pat.g = None;
            Ok(ev.snap.count_estimate(&pat)?)
        }
    }
}

pub(crate) fn pat_vars(t: &TriplePat) -> Vec<VarId> {
    let mut out = Vec::new();
    for p in [&t.s, &t.p, &t.o] {
        if let P::Var(v) = p {
            out.push(*v);
        }
    }
    out
}

/// Hash keys: variables certainly bound on both sides.
fn key_vars(l: &Phys, r: &Phys) -> Vec<VarId> {
    let a = certain(l);
    let b = certain(r);
    let mut out: Vec<VarId> = a.intersection(&b).copied().collect();
    out.sort();
    out
}

/// Variables certainly bound in every row a plan node emits.
pub(crate) fn certain(p: &Phys) -> HashSet<VarId> {
    match p {
        Phys::Unit | Phys::Empty => HashSet::new(),
        Phys::Scan { pat, .. } => pat_vars(pat).into_iter().collect(),
        Phys::BindJoin { input, pat, .. } => {
            let mut s = certain(input);
            s.extend(pat_vars(pat));
            s
        }
        Phys::BindPath { input, s, o, .. } => {
            let mut set = certain(input);
            for p in [s, o] {
                if let P::Var(v) = p {
                    set.insert(*v);
                }
            }
            set
        }
        Phys::HashJoin { left, right, .. } => {
            let mut s = certain(left);
            s.extend(certain(right));
            s
        }
        Phys::LeftJoin { left, .. } | Phys::Minus { left, .. } => certain(left),
        Phys::Filter { input, .. } => certain(input),
        // BIND may error → the variable stays unbound in that row.
        Phys::Extend { input, .. } => certain(input),
        Phys::Union(l, r) => certain(l).intersection(&certain(r)).copied().collect(),
        Phys::GraphConst { input, .. } => certain(input),
        Phys::GraphVar { var, input } => {
            let mut s = certain(input);
            s.insert(*var);
            s
        }
        Phys::Table { vars, rows } => vars
            .iter()
            .enumerate()
            .filter(|(k, _)| rows.iter().all(|r| r[*k].is_some()))
            .map(|(_, v)| *v)
            .collect(),
        // Aggregates may be absent (error) — only plain bound keys are
        // certain.
        Phys::Group { keys, input, .. } => {
            let inner = certain(input);
            keys.iter()
                .filter(|(v, e)| e.is_none() && inner.contains(v))
                .map(|(v, _)| *v)
                .collect()
        }
        Phys::Sort { input, .. } | Phys::Distinct { input } | Phys::Slice { input, .. } => {
            certain(input)
        }
        Phys::Project { input, vars } => {
            let inner = certain(input);
            vars.iter().filter(|v| inner.contains(v)).copied().collect()
        }
    }
}
