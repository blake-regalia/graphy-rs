//! `SegmentScan` — batched pattern scans over ONE base segment. The
//! storage↔engine seam (doc 02 §8, doc 05 §2) is the snapshot-level
//! `QuadScan` in `store.rs`, which zips this with the delta layer.
//!
//! A scan runs over ONE caller-requested ordering (the engine relies on the
//! output order for merge joins) and yields **columnar batches** of column
//! values, filling a caller-owned [`QuadBatch`] — no allocation per batch,
//! and the caller sizes the buffer to its vector width (doc 05 default 1024).
//!
//! Matching quads are produced in `(x, y, z, graph)` order of the requested
//! ordering: the bound prefix resolves to a contiguous ordinal range,
//! non-prefix bounds apply as residual filters, a bound graph becomes a
//! per-triple bitmap probe (SPO ordinals via `Pz` on secondary orderings),
//! and an unbound graph fans each triple out across its tg group —
//! resumable mid-group, so fan-out never overflows a batch.
//!
//! Base∪delta merging and tombstone elision happen one level up: the
//! snapshot-level `QuadScan` (store.rs) zips this scan with the delta.

use crate::bt::{Bt, BtSeqIter, Order};
use crate::foq::Foq;
use crate::format::StoreError;
use crate::segment::{Pattern, Segment};

/// Default batch capacity, matching the engine's vector width (doc 05 §2).
pub const BATCH_CAPACITY: usize = 1024;

/// A columnar batch of matching quads: parallel `s`/`p`/`o`/`g` column-value
/// vectors. The graph column is `0` for the default graph, else the named
/// graph's column value.
#[derive(Debug, Clone)]
pub struct QuadBatch {
    pub s: Vec<u64>,
    pub p: Vec<u64>,
    pub o: Vec<u64>,
    pub g: Vec<u64>,
    capacity: usize,
}

impl QuadBatch {
    pub fn new() -> QuadBatch {
        QuadBatch::with_capacity(BATCH_CAPACITY)
    }

    /// A batch holding at most `capacity` quads (`>= 1`).
    pub fn with_capacity(capacity: usize) -> QuadBatch {
        assert!(capacity >= 1, "batch capacity must be at least 1");
        QuadBatch {
            s: Vec::with_capacity(capacity),
            p: Vec::with_capacity(capacity),
            o: Vec::with_capacity(capacity),
            g: Vec::with_capacity(capacity),
            capacity,
        }
    }

    pub fn len(&self) -> usize {
        self.s.len()
    }

    pub fn is_empty(&self) -> bool {
        self.s.is_empty()
    }

    pub fn capacity(&self) -> usize {
        self.capacity
    }

    pub fn clear(&mut self) {
        self.s.clear();
        self.p.clear();
        self.o.clear();
        self.g.clear();
    }

    fn is_full(&self) -> bool {
        self.len() == self.capacity
    }

    /// Push a canonical (s, p, o, g) key (snapshot-level zipper).
    pub(crate) fn push_key(&mut self, q: [u64; 4]) {
        self.push(q[0], q[1], q[2], q[3]);
    }

    fn push(&mut self, s: u64, p: u64, o: u64, g: u64) {
        debug_assert!(!self.is_full());
        self.s.push(s);
        self.p.push(p);
        self.o.push(o);
        self.g.push(g);
    }
}

impl Default for QuadBatch {
    fn default() -> QuadBatch {
        QuadBatch::new()
    }
}

/// Graph fan-out state carried across a batch boundary: the current triple's
/// remaining quad-list indices.
#[derive(Debug, Clone, Copy)]
struct FanOut {
    s: u64,
    p: u64,
    o: u64,
    qi: u64,
    q_end: u64,
}

/// Where a scan's triples come from (see module docs): a contiguous range
/// over a materialized ordering, or one of the compact profile's FoQ
/// accessors serving PSO/OSP virtually.
#[derive(Debug)]
enum Source<'a> {
    /// A contiguous `Sz` ordinal range of a materialized ordering, walked
    /// run-to-run (amortized O(1) per triple).
    Range { bt: &'a Bt, iter: BtSeqIter<'a> },
    /// FoQ P-accessor: predicates `p..p_end`, each walked through its `Wp`
    /// occurrences `k..k_end`, each expanded through its `Sz` run
    /// `zi..z_end` under subject `s` — PSO-order emission.
    FoqP {
        spo: &'a Bt,
        foq: &'a Foq,
        p: u64,
        p_end: u64,
        k: u64,
        k_end: u64,
        zi: u64,
        z_end: u64,
        s: u64,
    },
    /// FoQ O-accessor: object indexes `j..j_end`, each walked through its
    /// `Po` run `r..r_end` — OSP-order emission.
    FoqO {
        spo: &'a Bt,
        foq: &'a Foq,
        j: u64,
        j_end: u64,
        o: u64,
        r: u64,
        r_end: u64,
    },
}

impl Source<'_> {
    /// The next triple in emission order, with its SPO ordinal when the
    /// source knows it (FoQ sources always do; ranges know it for SPO or
    /// via `Pz`).
    fn next(&mut self, order: Order) -> Option<([u64; 3], Option<u64>)> {
        match self {
            Source::Range { bt, iter } => {
                let (i, [x, y, z]) = iter.next()?;
                let hint = if order == Order::Spo {
                    Some(i)
                } else {
                    bt.spo_at(i)
                };
                Some((order.to_spo(x, y, z), hint))
            }
            Source::FoqP {
                spo,
                foq,
                p,
                p_end,
                k,
                k_end,
                zi,
                z_end,
                s,
            } => loop {
                if zi < z_end {
                    let ord = *zi;
                    *zi += 1;
                    return Some(([*s, *p, spo.z_at(ord)], Some(ord)));
                }
                if k < k_end {
                    let yi = foq.wp.select(*p, *k).expect("k below rank") as u64;
                    *k += 1;
                    *s = spo.x_value(spo.group_of_y(yi));
                    (*zi, *z_end) = spo.z_range(yi);
                    continue;
                }
                *p += 1;
                if p >= p_end {
                    return None;
                }
                *k = 0;
                *k_end = foq.wp.rank(*p, foq.wp.len());
            },
            Source::FoqO {
                spo,
                foq,
                j,
                j_end,
                o,
                r,
                r_end,
            } => loop {
                if r < r_end {
                    let ord = foq.po.get(*r as usize);
                    *r += 1;
                    let [s, p, _] = spo.triple_at(ord);
                    return Some(([s, p, *o], Some(ord)));
                }
                *j += 1;
                if j >= j_end {
                    return None;
                }
                *o = foq.xo.get(*j as usize);
                (*r, *r_end) = foq.object_run(*j);
            },
        }
    }
}

/// A running pattern scan over one ordering (see module docs).
#[derive(Debug)]
pub struct SegmentScan<'a> {
    seg: &'a Segment,
    order: Order,
    source: Source<'a>,
    /// Residual equality filters in (s, p, o) space — checked for every
    /// emitted triple, uniformly across sources (re-checking components a
    /// source already guarantees is a cheap always-true compare).
    filter: [Option<u64>; 3],
    g_filter: Option<u64>,
    fan_out: Option<FanOut>,
    /// (last SPO ordinal, its tg-group end): consecutive ordinals extend the
    /// cursor with a forward bit scan instead of two selects per triple.
    tg_seq: Option<(u64, u64)>,
}

impl Segment {
    /// Scan `pat` in the requested `order`, yielding batches via
    /// [`SegmentScan::next_batch`]. The order must be materialized in this
    /// profile — or PSO/OSP on compact, which the FoQ accessors serve
    /// virtually.
    pub fn scan_order(&self, pat: &Pattern, order: Order) -> Result<SegmentScan<'_>, StoreError> {
        let source = if let Some(bt) = self.ordering_bt(order) {
            let (lo, hi) = prefix_range(bt, pat, order);
            Source::Range {
                bt,
                iter: bt.seq_range(lo, hi),
            }
        } else if let (Some(foq), Order::Pso) = (self.foq(), order) {
            let spo = self.ordering_bt(Order::Spo).expect("SPO required at open");
            let n_preds = self.manifest.counts.predicates;
            let (p, p_end) = match pat.p {
                Some(p) if p < n_preds => (p, p + 1),
                Some(_) => (0, 0),
                None => (0, n_preds),
            };
            let k_end = if p < p_end {
                foq.wp.rank(p, foq.wp.len())
            } else {
                0
            };
            Source::FoqP {
                spo,
                foq,
                p,
                p_end,
                k: 0,
                k_end,
                zi: 0,
                z_end: 0,
                s: 0,
            }
        } else if let (Some(foq), Order::Osp) = (self.foq(), order) {
            let spo = self.ordering_bt(Order::Spo).expect("SPO required at open");
            let (j, j_end) = match pat.o {
                Some(o) => match foq.locate_object(o) {
                    Some(j) => (j, j + 1),
                    None => (0, 0),
                },
                None => (0, foq.n_objects()),
            };
            let (o, r, r_end) = if j < j_end {
                let (r, r_end) = foq.object_run(j);
                (foq.xo.get(j as usize), r, r_end)
            } else {
                (0, 0, 0)
            };
            Source::FoqO {
                spo,
                foq,
                j,
                j_end,
                o,
                r,
                r_end,
            }
        } else {
            return Err(StoreError::Manifest(format!(
                "ordering {} not available in this profile",
                order.name()
            )));
        };
        // A named-graph bound on a triples-only segment matches nothing.
        let mut source = source;
        if matches!(pat.g, Some(gv) if gv > 0) && self.graphs_layer().is_none() {
            exhaust(&mut source);
        }
        Ok(SegmentScan {
            seg: self,
            order,
            source,
            filter: [pat.s, pat.p, pat.o],
            g_filter: pat.g,
            fan_out: None,
            tg_seq: None,
        })
    }
}

/// Contiguous `Sz` ordinal range of `pat`'s bound prefix in `order`.
fn prefix_range(bt: &Bt, pat: &Pattern, order: Order) -> (u64, u64) {
    let want = order.to_xyz(
        pat.s.unwrap_or_default(),
        pat.p.unwrap_or_default(),
        pat.o.unwrap_or_default(),
    );
    let bound_flags = order.to_xyz(
        u64::from(pat.s.is_some()),
        u64::from(pat.p.is_some()),
        u64::from(pat.o.is_some()),
    );
    let bound = [
        bound_flags[0] == 1,
        bound_flags[1] == 1,
        bound_flags[2] == 1,
    ];
    if !bound[0] {
        return (0, bt.n_triples());
    }
    match bt.x_group(want[0]) {
        None => (0, 0),
        Some(g) => {
            if bound[1] {
                match bt.find_y(g, want[1]) {
                    None => (0, 0),
                    Some(yi) => {
                        if bound[2] {
                            match bt.find_z(yi, want[2]) {
                                None => (0, 0),
                                Some(i) => (i, i + 1),
                            }
                        } else {
                            bt.z_range(yi)
                        }
                    }
                }
            } else {
                bt.z_range_of_group(g)
            }
        }
    }
}

/// Force a source to its exhausted state.
fn exhaust(src: &mut Source<'_>) {
    match src {
        Source::Range { iter, .. } => iter.stop(),
        Source::FoqP {
            p,
            p_end,
            k,
            k_end,
            zi,
            z_end,
            ..
        } => {
            *p = *p_end;
            *k = *k_end;
            *zi = *z_end;
        }
        Source::FoqO {
            j, j_end, r, r_end, ..
        } => {
            *j = *j_end;
            *r = *r_end;
        }
    }
}

impl SegmentScan<'_> {
    /// The ordering this scan emits in.
    pub fn order(&self) -> Order {
        self.order
    }

    /// Clear `out` and fill it with the next quads, up to its capacity.
    /// Returns `false` (with `out` empty) when the scan is exhausted.
    pub fn next_batch(&mut self, out: &mut QuadBatch) -> Result<bool, StoreError> {
        out.clear();
        // Drain a fan-out suspended at the previous batch boundary.
        if let Some(f) = self.fan_out.take() {
            self.emit_fan_out(f, out);
        }
        while !out.is_full() {
            let Some(([s, p, o], hint)) = self.source.next(self.order) else {
                break;
            };
            // Residual filters.
            if self.filter[0].is_some_and(|v| v != s)
                || self.filter[1].is_some_and(|v| v != p)
                || self.filter[2].is_some_and(|v| v != o)
            {
                continue;
            }
            let Some(graphs) = self.seg.graphs_layer() else {
                // Triples-only: one default-graph quad (named-graph bounds
                // were short-circuited at construction).
                out.push(s, p, o, 0);
                continue;
            };
            // FoQ sources and SPO/Pz ranges hand the SPO ordinal over; the
            // lookup is the defensive fallback.
            let spo_ord = match hint {
                Some(ord) => ord,
                None => self.seg.spo_ordinal(s, p, o)?,
            };
            match self.g_filter {
                // Bound graph: one bitmap probe, at most one quad.
                Some(gv) => {
                    let member = graphs
                        .at
                        .get(gv as usize)
                        .is_some_and(|bm| bm.contains(spo_ord));
                    if member {
                        out.push(s, p, o, gv);
                    }
                }
                // Unbound graph: fan out across the triple's tg group.
                None => {
                    let (qi, q_end) = match self.tg_seq {
                        Some((last, last_end)) if spo_ord == last + 1 => {
                            let end = graphs
                                .tg_starts
                                .next_one(last_end as usize + 1)
                                .map_or(graphs.tg_starts.len() as u64, |p| p as u64);
                            (last_end, end)
                        }
                        _ => graphs.quad_range(spo_ord),
                    };
                    self.tg_seq = Some((spo_ord, q_end));
                    self.emit_fan_out(FanOut { s, p, o, qi, q_end }, out);
                }
            }
        }
        Ok(!out.is_empty())
    }

    /// Emit a triple's graph fan-out into `out`, suspending into
    /// `self.fan_out` if the batch fills mid-group.
    fn emit_fan_out(&mut self, mut f: FanOut, out: &mut QuadBatch) {
        let graphs = self.seg.graphs_layer().expect("fan-out requires graphs");
        while f.qi < f.q_end {
            if out.is_full() {
                self.fan_out = Some(f);
                return;
            }
            let gv = graphs.tg.access(f.qi as usize);
            out.push(f.s, f.p, f.o, gv);
            f.qi += 1;
        }
    }
}
