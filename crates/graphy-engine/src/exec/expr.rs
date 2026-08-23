//! Compiled expression programs (doc 05 §4): filter/extend expressions
//! compile once per query into a small program with **inline-ID fast
//! paths** — comparisons whose operands land on inline-tagged `TermId`s
//! (numerics, booleans, dateTimes) evaluate by `partial_cmp_value`
//! without touching the dictionary. Everything else falls back to the
//! shared reference expression evaluator, keeping the semantic long
//! tail single-sourced.

use graphy_algebra::algebra::CmpOp;
use graphy_algebra::{Expression, VarId};
use graphy_core::id::{partial_cmp_value, Tag};

use crate::eval::{Evaluator, Row, B};

/// A compiled expression: a fast shape plus the original tree for the
/// general path.
#[derive(Debug, Clone)]
pub(crate) struct Prog {
    shape: Shape,
    expr: Expression,
    /// Interned constant operand (filled on first use).
    konst: Option<B>,
}

#[derive(Debug, Clone)]
enum Shape {
    /// `?a <op> ?b` — inline-ID compare when both bindings are inline.
    CmpVarVar { op: CmpOp, a: VarId, b: VarId },
    /// `?a <op> const` (either side; op pre-flipped to var-first).
    CmpVarConst { op: CmpOp, a: VarId, c: Vec<u8> },
    /// No fast path: shared reference evaluator.
    General,
}

impl Prog {
    pub fn compile(expr: &Expression) -> Prog {
        use Expression as E;
        let shape = match expr {
            E::Cmp(op, a, b) => match (&**a, &**b) {
                (E::Var(x), E::Var(y)) => Shape::CmpVarVar {
                    op: *op,
                    a: *x,
                    b: *y,
                },
                (E::Var(x), E::Term(c)) => Shape::CmpVarConst {
                    op: *op,
                    a: *x,
                    c: c.clone(),
                },
                (E::Term(c), E::Var(x)) => Shape::CmpVarConst {
                    op: flip(*op),
                    a: *x,
                    c: c.clone(),
                },
                _ => Shape::General,
            },
            _ => Shape::General,
        };
        Prog {
            shape,
            expr: expr.clone(),
            konst: None,
        }
    }

    /// Effective boolean value of the expression over `row` (unbound /
    /// error → `false`, per FILTER semantics).
    pub fn truthy(&mut self, ev: &mut Evaluator<'_>, row: &Row) -> bool {
        match &self.shape {
            Shape::CmpVarVar { op, a, b } => {
                if let (Some(B::Id(x)), Some(B::Id(y))) = (
                    row.get(a.0 as usize).copied().flatten(),
                    row.get(b.0 as usize).copied().flatten(),
                ) {
                    if x.tag() != Some(Tag::DateTime) && y.tag() != Some(Tag::DateTime) {
                        if let Some(o) = partial_cmp_value(x, y) {
                            return apply(*op, o);
                        }
                    }
                }
                ev.truthy(&self.expr, row)
            }
            Shape::CmpVarConst { op, a, .. } => {
                let c = match self.konst {
                    Some(c) => c,
                    None => {
                        let Shape::CmpVarConst { c: bytes, .. } = &self.shape else {
                            unreachable!()
                        };
                        let b = ev.intern(bytes.clone());
                        self.konst = Some(b);
                        b
                    }
                };
                if let (Some(B::Id(x)), B::Id(y)) = (row.get(a.0 as usize).copied().flatten(), c) {
                    if x.tag() != Some(Tag::DateTime) && y.tag() != Some(Tag::DateTime) {
                        if let Some(o) = partial_cmp_value(x, y) {
                            return apply(*op, o);
                        }
                    }
                }
                ev.truthy(&self.expr, row)
            }
            Shape::General => ev.truthy(&self.expr, row),
        }
    }

    /// Full evaluation (Extend / BIND): always the shared evaluator —
    /// the produced binding must intern exactly like the reference.
    pub fn eval(&self, ev: &mut Evaluator<'_>, row: &Row) -> Result<B, ()> {
        ev.eval_expr(&self.expr, row)
    }
}

fn flip(op: CmpOp) -> CmpOp {
    match op {
        CmpOp::Lt => CmpOp::Gt,
        CmpOp::Le => CmpOp::Ge,
        CmpOp::Gt => CmpOp::Lt,
        CmpOp::Ge => CmpOp::Le,
        other => other,
    }
}

fn apply(op: CmpOp, o: std::cmp::Ordering) -> bool {
    use std::cmp::Ordering::*;
    match op {
        CmpOp::Eq => o == Equal,
        CmpOp::Ne => o != Equal,
        CmpOp::Lt => o == Less,
        CmpOp::Le => o != Greater,
        CmpOp::Gt => o == Greater,
        CmpOp::Ge => o != Less,
    }
}
