//! The in-memory delta layer (doc 07 §3, M4): adds and tombstones over an
//! immutable base segment, snapshot-consistent via epoch stamps.
//!
//! - **Indexes**: one ordered map per scan order the snapshot serves
//!   (materialized + FoQ-virtual), keyed by the (x, y, z, g)-permuted quad.
//!   Values are append-only `(epoch, kind)` event lists; a reader at epoch e
//!   sees the latest event with `epoch ≤ e` (so a delete of a delta add
//!   elides it for later epochs while older snapshots still see the add).
//! - **Overlay dictionary**: terms absent from the base get column values
//!   *above* the base ranges, per position, arrival-ordered. Overlay ids are
//!   snapshot-meaningful and remapped at merge (M5) like all others.
//! - **Locking baseline** (doc 07 §3 deferred the structure choice to a
//!   benchmark; DECIDED 2026-07-15, BENCHMARKS.md M4: baseline KEPT — write
//!   cost is flat in resident delta size and the writer is WAL/fsync-bound
//!   long before structure cost matters): one `RwLock` around the whole
//!   state. Readers never hold it across batches — scans *collect* their
//!   (bounded) matching range up front under a short read lock, so snapshot
//!   reads stay consistent without blocking the single writer for long.

use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::RwLock;

use graphy_core::TermId;

use crate::bt::Order;
use crate::segment::{Pattern, TermPos};

/// Process-unique identity for one overlay/id-space incarnation. The
/// engine's process-wide plan cache includes this value because compaction
/// can replace a delta and reissue overlay columns without advancing the
/// store's write epoch.
static NEXT_IDENTITY: AtomicU64 = AtomicU64::new(1);

/// Canonical (s, p, o, g) column-value quad key.
pub(crate) type QuadKey = [u64; 4];

/// Epoch-stamped event list for one quad key. The overwhelmingly common
/// case — a key touched once — stores its event inline; only real
/// delete/re-add histories pay for a heap vector. Six index copies of
/// every entry make this per-entry economy a first-order term of the
/// delta's resident size (docs/11 wasm32 budget).
#[derive(Debug, Clone, PartialEq)]
enum Events {
    One([(u64, Kind); 1]),
    Many(Vec<(u64, Kind)>),
}

impl Events {
    fn one(e: (u64, Kind)) -> Events {
        Events::One([e])
    }

    fn push(&mut self, e: (u64, Kind)) {
        match self {
            Events::One([a]) => *self = Events::Many(vec![*a, e]),
            Events::Many(v) => v.push(e),
        }
    }

    fn as_slice(&self) -> &[(u64, Kind)] {
        match self {
            Events::One(a) => a,
            Events::Many(v) => v,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Kind {
    Add,
    Tombstone,
}

/// Permute a canonical key into `order`'s (x, y, z, g) space.
/// Index-key permutation: **graph-first**, then the order's xyz. The
/// engine binds the graph on every scan (`Scope` always names one),
/// so leading with it keeps graph-scoped patterns range-clipped even
/// when s/p/o are all variables — the shape every `graph <g> { ?s ?p
/// ?o }` read takes. Within one bound graph the iteration order
/// equals ascending `[x, y, z]`, which is exactly the zipper's
/// permuted comparison order; unbound-graph collections re-sort.
fn permute(order: Order, q: QuadKey) -> [u64; 4] {
    let [x, y, z] = order.to_xyz(q[0], q[1], q[2]);
    [q[3], x, y, z]
}

/// Inverse of [`permute`].
fn unpermute(order: Order, k: [u64; 4]) -> QuadKey {
    let [s, p, o] = order.to_spo(k[1], k[2], k[3]);
    [s, p, o, k[0]]
}

/// One position's overlay id space: terms above the base range,
/// arrival-ordered (doc 07 §3 — order patches arrive with the merge work).
#[derive(Debug)]
struct OverlaySpace {
    /// First overlay column value (= one past the base range).
    base: u64,
    ids: HashMap<Box<[u8]>, u64>,
    terms: Vec<Box<[u8]>>,
    /// Per-entry canonical [`TermId`] alias: `Some` when the term already
    /// holds an identity in the *other* subject/object space (base or
    /// overlay), preserving the base invariant that one term resolves to
    /// one public id across both positions (the shared-section guarantee).
    /// `None` = the entry's positional overlay id IS its identity.
    alias: Vec<Option<TermId>>,
}

impl OverlaySpace {
    fn new(base: u64) -> OverlaySpace {
        OverlaySpace {
            base,
            ids: HashMap::new(),
            terms: Vec::new(),
            alias: Vec::new(),
        }
    }

    fn resolve(&self, bytes: &[u8]) -> Option<u64> {
        self.ids.get(bytes).copied()
    }

    fn intern(&mut self, bytes: &[u8], alias: Option<TermId>) -> u64 {
        if let Some(&v) = self.ids.get(bytes) {
            return v;
        }
        let v = self.base + self.terms.len() as u64;
        self.ids.insert(bytes.into(), v);
        self.terms.push(bytes.into());
        self.alias.push(alias);
        v
    }

    fn decode(&self, v: u64) -> Option<&[u8]> {
        self.terms
            .get(v.checked_sub(self.base)? as usize)
            .map(AsRef::as_ref)
    }
}

/// Everything the writer mutates, behind the delta's lock.
#[derive(Debug)]
struct State {
    /// Per scan order: permuted key → epoch-stamped events.
    indexes: Vec<(Order, BTreeMap<[u64; 4], Events>)>,
    subj: OverlaySpace,
    pred: OverlaySpace,
    obj: OverlaySpace,
    graph: OverlaySpace,
    /// Canonical id → positional overlay column value, for aliased entries
    /// (the inverse of [`OverlaySpace::alias`]): lets a public id whose
    /// section names the *other* position translate into this position's
    /// column space for scans and bound-pattern pushdown.
    alias_cols: HashMap<(TermId, TermPos), u64>,
    /// Live event count (rough size signal for the merge scheduler, M5).
    events: u64,
    /// Live tombstone count (read-amplification signal: every tombstone is
    /// zipper work on scans until a merge folds it away).
    tombstones: u64,
}

/// The shared delta layer handle (one per store generation).
#[derive(Debug)]
pub(crate) struct Delta {
    identity: u64,
    state: RwLock<State>,
}

/// Base column-range watermarks the overlay sits above.
#[derive(Debug, Clone, Copy)]
pub(crate) struct BaseRanges {
    /// Subject space size (`n_shared + n_subjects`).
    pub subjects: u64,
    pub predicates: u64,
    /// Object dictionary space size (`n_shared + n_objects`; tag-0 range).
    pub objects: u64,
    /// One past the last named-graph column value (`n_graphs + 1`).
    pub graphs: u64,
}

impl Delta {
    pub fn new(orders: &[Order], ranges: BaseRanges) -> Delta {
        Delta {
            identity: NEXT_IDENTITY.fetch_add(1, Ordering::Relaxed),
            state: RwLock::new(State {
                indexes: orders.iter().map(|&o| (o, BTreeMap::new())).collect(),
                subj: OverlaySpace::new(ranges.subjects),
                pred: OverlaySpace::new(ranges.predicates),
                obj: OverlaySpace::new(ranges.objects),
                graph: OverlaySpace::new(ranges.graphs),
                alias_cols: HashMap::new(),
                events: 0,
                tombstones: 0,
            }),
        }
    }

    /// Process-unique identity of this overlay/id-space incarnation.
    pub fn identity(&self) -> u64 {
        self.identity
    }

    /// Column value of an overlay term in `pos`, if interned.
    pub fn resolve(&self, bytes: &[u8], pos: TermPos) -> Option<u64> {
        let s = self.state.read().expect("delta lock");
        match pos {
            TermPos::Subject => s.subj.resolve(bytes),
            TermPos::Predicate => s.pred.resolve(bytes),
            TermPos::Object => s.obj.resolve(bytes),
            TermPos::Graph => s.graph.resolve(bytes),
        }
    }

    /// Concise bytes of an overlay column value (owned; the arena lives
    /// behind the lock).
    pub fn decode(&self, v: u64, pos: TermPos) -> Option<Vec<u8>> {
        let s = self.state.read().expect("delta lock");
        let space = match pos {
            TermPos::Subject => &s.subj,
            TermPos::Predicate => &s.pred,
            TermPos::Object => &s.obj,
            TermPos::Graph => &s.graph,
        };
        space.decode(v).map(<[u8]>::to_vec)
    }

    /// Writer-side: intern a term for `pos` (base misses only). `alias` is
    /// the term's canonical [`TermId`] when it already holds one in the
    /// other subject/object space (see [`OverlaySpace::alias`]); callers
    /// pass `None` for predicate/graph positions.
    pub fn intern(&self, bytes: &[u8], pos: TermPos, alias: Option<TermId>) -> u64 {
        let mut s = self.state.write().expect("delta lock");
        let v = match pos {
            TermPos::Subject => s.subj.intern(bytes, alias),
            TermPos::Predicate => s.pred.intern(bytes, None),
            TermPos::Object => s.obj.intern(bytes, alias),
            TermPos::Graph => s.graph.intern(bytes, None),
        };
        if let Some(id) = alias.filter(|_| matches!(pos, TermPos::Subject | TermPos::Object)) {
            s.alias_cols.insert((id, pos), v);
        }
        v
    }

    /// Canonical [`TermId`] alias of an overlay column value, when the
    /// entry's identity lives in the other subject/object space.
    pub fn canon(&self, v: u64, pos: TermPos) -> Option<TermId> {
        let s = self.state.read().expect("delta lock");
        let space = match pos {
            TermPos::Subject => &s.subj,
            TermPos::Object => &s.obj,
            _ => return None,
        };
        *space.alias.get(v.checked_sub(space.base)? as usize)?
    }

    /// Positional overlay column value of a canonical [`TermId`] whose own
    /// section names the other position (inverse of [`Delta::canon`]).
    pub fn alias_col(&self, id: TermId, pos: TermPos) -> Option<u64> {
        let s = self.state.read().expect("delta lock");
        s.alias_cols.get(&(id, pos)).copied()
    }

    /// Writer-side: record events (canonical keys) at `epoch` in every index.
    pub fn record(&self, entries: &[(QuadKey, Kind)], epoch: u64) {
        if entries.is_empty() {
            return;
        }
        let mut s = self.state.write().expect("delta lock");
        s.events += entries.len() as u64;
        s.tombstones += entries
            .iter()
            .filter(|(_, k)| *k == Kind::Tombstone)
            .count() as u64;
        for (order, map) in &mut s.indexes {
            for &(q, kind) in entries {
                match map.entry(permute(*order, q)) {
                    std::collections::btree_map::Entry::Vacant(e) => {
                        e.insert(Events::one((epoch, kind)));
                    }
                    std::collections::btree_map::Entry::Occupied(mut e) => {
                        e.get_mut().push((epoch, kind));
                    }
                }
            }
        }
    }

    /// Live event count (size signal).
    pub fn events(&self) -> u64 {
        self.state.read().expect("delta lock").events
    }

    /// Live tombstone count (read-amplification signal for the scheduler).
    pub fn tombstones(&self) -> u64 {
        self.state.read().expect("delta lock").tombstones
    }

    /// Effective kind of one exact quad at `epoch`, if the delta has events
    /// for it.
    pub fn probe(&self, key: QuadKey, epoch: u64) -> Option<Kind> {
        let s = self.state.read().expect("delta lock");
        let (order, map) = &s.indexes.first().expect("at least one index");
        map.get(&permute(*order, key))?
            .as_slice()
            .iter()
            .rev()
            .find(|(e, _)| *e <= epoch)
            .map(|&(_, k)| k)
    }

    /// Every overlay term interned for `pos`, as (concise bytes, column
    /// value) — the streaming dictionary merge's overlay input (bounded by
    /// the delta budget; the caller sorts).
    pub fn overlay_terms(&self, pos: TermPos) -> Vec<(Box<[u8]>, u64)> {
        let s = self.state.read().expect("delta lock");
        let space = match pos {
            TermPos::Subject => &s.subj,
            TermPos::Predicate => &s.pred,
            TermPos::Object => &s.obj,
            TermPos::Graph => &s.graph,
        };
        space
            .terms
            .iter()
            .enumerate()
            .map(|(i, t)| (t.clone(), space.base + i as u64))
            .collect()
    }

    /// Number of overlay terms interned for `pos` (extends the public
    /// TermId ordinal range past the base counts).
    pub fn space_len(&self, pos: TermPos) -> u64 {
        let s = self.state.read().expect("delta lock");
        let space = match pos {
            TermPos::Subject => &s.subj,
            TermPos::Predicate => &s.pred,
            TermPos::Object => &s.obj,
            TermPos::Graph => &s.graph,
        };
        space.terms.len() as u64
    }

    /// Add a scan-order index (minor merge, doc 07 §6.4: a lazily added
    /// ordering needs delta coverage too). Backfilled from index 0 with
    /// every event at its original epoch, so readers at ANY epoch see the
    /// same dataset through the new order. Idempotent.
    pub fn add_order(&self, order: Order) {
        let mut s = self.state.write().expect("delta lock");
        if s.indexes.iter().any(|(o, _)| *o == order) {
            return;
        }
        let (order0, src) = {
            let (o0, map) = &s.indexes[0];
            let src: Vec<([u64; 4], Events)> = map.iter().map(|(k, v)| (*k, v.clone())).collect();
            (*o0, src)
        };
        let mut map = BTreeMap::new();
        for (k, events) in src {
            map.insert(permute(order, unpermute(order0, k)), events);
        }
        s.indexes.push((order, map));
    }

    /// Epoch GC (doc 07 §2): compact event lists to what readers at epochs
    /// `≥ min_epoch` can still observe. For each key, events strictly before
    /// the effective event at `min_epoch` are unreachable and drop; the
    /// effective event itself drops when it merely restates base membership
    /// (`Add` of a base quad after a delete/re-add chain, or `Tombstone` of a
    /// quad the base never had) — every live reader then falls through to
    /// the base with an identical result. Empty lists remove the key.
    /// Returns the number of events reclaimed. Overlay *terms* are never
    /// freed here (their column values live in handed-out keys); the merge
    /// (M5) remaps them.
    pub fn gc(&self, min_epoch: u64, in_base: impl Fn(QuadKey) -> bool) -> u64 {
        let mut s = self.state.write().expect("delta lock");
        let s = &mut *s;
        let before = s.events;
        for (order, map) in &mut s.indexes {
            map.retain(|k, events| {
                // Event lists are epoch-sorted (single writer, monotone).
                let s = events.as_slice();
                let cut = s.partition_point(|(e, _)| *e <= min_epoch);
                if cut > 0 {
                    let (_, kind) = s[cut - 1];
                    let base_has = in_base(unpermute(*order, *k));
                    let restates_base = match kind {
                        Kind::Add => base_has,
                        Kind::Tombstone => !base_has,
                    };
                    let keep = &s[if restates_base { cut } else { cut - 1 }..];
                    *events = match keep {
                        [] => return false,
                        &[e] => Events::one(e),
                        many => Events::Many(many.to_vec()),
                    };
                }
                true
            });
        }
        // The counters count each entry once; index 0 holds one copy each.
        s.events = s.indexes[0]
            .1
            .values()
            .map(|v| v.as_slice().len() as u64)
            .sum();
        s.tombstones = s.indexes[0]
            .1
            .values()
            .flat_map(Events::as_slice)
            .filter(|(_, k)| *k == Kind::Tombstone)
            .count() as u64;
        before - s.events
    }

    /// Every event with `after < epoch ≤ upto` (a slice of the merge swap's
    /// active suffix, doc 07 §6.3): canonical keys with their kind and
    /// epoch, grouped per key in epoch order. The merger remaps these into
    /// the new generation's id space in *shadow passes* — disjoint ascending
    /// epoch ranges — so most of the suffix is processed before the swap
    /// goes exclusive; events at or below the freeze epoch are folded into
    /// the new base and die with this delta.
    pub fn collect_suffix(&self, after: u64, upto: u64) -> Vec<(QuadKey, Kind, u64)> {
        let s = self.state.read().expect("delta lock");
        let (order, map) = s.indexes.first().expect("at least one index");
        let mut out = Vec::new();
        for (&k, events) in map {
            for &(e, kind) in events
                .as_slice()
                .iter()
                .skip_while(|(e, _)| *e <= after)
                .take_while(|(e, _)| *e <= upto)
            {
                out.push((unpermute(*order, k), kind, e));
            }
        }
        out
    }

    /// One `(key, effective kind)` per key with any event at `epoch` or
    /// below — the *net* state a reader at `epoch` observes, with history
    /// elided. The ephemeral compaction's input (docs/11): unlike
    /// [`Delta::collect_suffix`] the output is bounded by resident *keys*,
    /// not events, so a heavily churned delta folds without materializing
    /// its history.
    pub fn collect_effective(&self, epoch: u64) -> Vec<(QuadKey, Kind)> {
        let s = self.state.read().expect("delta lock");
        let (order, map) = s.indexes.first().expect("at least one index");
        let mut out = Vec::with_capacity(map.len());
        for (&k, events) in map {
            if let Some(&(_, kind)) = events.as_slice().iter().rev().find(|(e, _)| *e <= epoch) {
                out.push((unpermute(*order, k), kind));
            }
        }
        out
    }

    /// Matching delta entries for `pat` in `order`, as seen at `epoch`:
    /// canonical keys with their effective kind, ascending in the permuted
    /// key order (zipper-ready against a base scan in the same order).
    /// Collected under a short read lock — bounded by the delta budget.
    pub fn collect_range(&self, order: Order, pat: &Pattern, epoch: u64) -> Vec<(QuadKey, Kind)> {
        let s = self.state.read().expect("delta lock");
        let map = &s
            .indexes
            .iter()
            .find(|(o, _)| *o == order)
            .expect("delta indexes cover every scan order")
            .1;

        // Bound prefix in permuted space → BTreeMap range; the rest filter.
        let (want, bound, lo, hi, clipped) = range_plan(order, pat);

        let range: Box<dyn Iterator<Item = (&[u64; 4], &Events)>> = if clipped {
            Box::new(map.range(lo..hi))
        } else {
            Box::new(map.range(lo..))
        };
        let mut out = Vec::new();
        for (&k, events) in range {
            // Residual filters (non-prefix bounds).
            if (bound[0] && k[0] != want[0])
                || (bound[1] && k[1] != want[1])
                || (bound[2] && k[2] != want[2])
                || (bound[3] && k[3] != want[3])
            {
                continue;
            }
            // Effective kind at the snapshot's epoch.
            let Some(&(_, kind)) = events.as_slice().iter().rev().find(|(e, _)| *e <= epoch) else {
                continue;
            };
            out.push((unpermute(order, k), kind));
        }
        // Graph-first iteration equals the zipper's [x, y, z, g]
        // comparison order only within one graph; an unbound-graph
        // collection spans graphs and re-sorts (it walked the whole
        // map anyway).
        if pat.g.is_none() {
            out.sort_unstable_by_key(|(q, _)| {
                let [x, y, z] = order.to_xyz(q[0], q[1], q[2]);
                [x, y, z, q[3]]
            });
        }
        out
    }

    /// Upper-bound size of the matching delta range for `pat` in
    /// `order` — the planning/ordering heuristic's delta term. When
    /// the pattern's bound components give an index prefix this is
    /// the prefix range's entry count (residual-filtered); when they
    /// give none — a graph-only or otherwise non-prefix pattern —
    /// the answer is the live event count, O(1), instead of the
    /// whole-map walk an exact count would need (the 2026-08-07
    /// commit-path degradation: every metadata read re-counted a
    /// delta-resident store per planned pattern).
    pub fn estimate_range(&self, order: Order, pat: &Pattern) -> u64 {
        let s = self.state.read().expect("delta lock");
        let map = &s
            .indexes
            .iter()
            .find(|(o, _)| *o == order)
            .expect("delta indexes cover every scan order")
            .1;
        let (want, bound, lo, hi, clipped) = range_plan(order, pat);
        if !clipped {
            return s.events;
        }
        map.range(lo..hi)
            .filter(|(k, _)| {
                !((bound[0] && k[0] != want[0])
                    || (bound[1] && k[1] != want[1])
                    || (bound[2] && k[2] != want[2])
                    || (bound[3] && k[3] != want[3]))
            })
            .count() as u64
    }
}

/// The permuted-space range plan shared by [`Delta::collect_range`]
/// and [`Delta::estimate_range`]: target key, per-component bound
/// flags, BTreeMap range bounds, and whether the bound components
/// yielded a usable (clipped) prefix.
#[allow(clippy::type_complexity)]
fn range_plan(order: Order, pat: &Pattern) -> ([u64; 4], [bool; 4], [u64; 4], [u64; 4], bool) {
    let want = permute(
        order,
        [
            pat.s.unwrap_or_default(),
            pat.p.unwrap_or_default(),
            pat.o.unwrap_or_default(),
            pat.g.unwrap_or_default(),
        ],
    );
    let bound = {
        let [bx, by, bz] = order.to_xyz(
            u64::from(pat.s.is_some()),
            u64::from(pat.p.is_some()),
            u64::from(pat.o.is_some()),
        );
        [pat.g.is_some(), bx == 1, by == 1, bz == 1]
    };
    let prefix = bound.iter().take_while(|&&b| b).count();
    let mut lo = [0u64; 4];
    let mut hi = [u64::MAX; 4];
    let mut clipped = true;
    lo[..prefix].copy_from_slice(&want[..prefix]);
    hi[..prefix].copy_from_slice(&want[..prefix]);
    if prefix > 0 {
        // hi = prefix with last component + 1 (exclusive via saturation).
        if hi[prefix - 1] == u64::MAX {
            clipped = false; // prefix already at the top; range to end
        } else {
            hi[prefix - 1] += 1;
            for h in hi.iter_mut().skip(prefix) {
                *h = 0;
            }
        }
    } else {
        clipped = false;
    }
    (want, bound, lo, hi, clipped)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ranges() -> BaseRanges {
        BaseRanges {
            subjects: 100,
            predicates: 10,
            objects: 200,
            graphs: 5,
        }
    }

    #[test]
    fn overlay_spaces_are_position_local() {
        let d = Delta::new(&[Order::Spo], ranges());
        let s = d.intern(b">http://x/new", TermPos::Subject, None);
        let o = d.intern(b">http://x/new", TermPos::Object, None);
        assert_eq!(s, 100);
        assert_eq!(o, 200);
        assert_eq!(d.intern(b">http://x/new", TermPos::Subject, None), s);
        assert_eq!(d.resolve(b">http://x/new", TermPos::Subject), Some(100));
        assert_eq!(d.resolve(b">http://x/other", TermPos::Subject), None);
        assert_eq!(
            d.decode(100, TermPos::Subject).as_deref(),
            Some(&b">http://x/new"[..])
        );
        assert_eq!(d.decode(99, TermPos::Subject), None);
    }

    #[test]
    fn cross_position_aliases_round_trip() {
        use graphy_core::Section;
        let d = Delta::new(&[Order::Spo], ranges());
        // Term already canonical elsewhere (e.g. base subject) entering the
        // object space: the overlay entry carries the alias both ways.
        let id = TermId::dict(Section::Subjects, 7);
        let o = d.intern(b">http://x/known", TermPos::Object, Some(id));
        assert_eq!(d.canon(o, TermPos::Object), Some(id));
        assert_eq!(d.alias_col(id, TermPos::Object), Some(o));
        assert_eq!(d.alias_col(id, TermPos::Subject), None);
        // Un-aliased entries expose no canon.
        let s = d.intern(b">http://x/fresh", TermPos::Subject, None);
        assert_eq!(d.canon(s, TermPos::Subject), None);
    }

    #[test]
    fn epochs_gate_visibility() {
        let d = Delta::new(&[Order::Spo, Order::Pos], ranges());
        let q: QuadKey = [1, 2, 3, 0];
        d.record(&[(q, Kind::Add)], 1);
        d.record(&[(q, Kind::Tombstone)], 3);

        let pat = Pattern::default();
        // Before the add: nothing.
        assert!(d.collect_range(Order::Spo, &pat, 0).is_empty());
        // Between add and delete: effective Add.
        assert_eq!(d.collect_range(Order::Spo, &pat, 1), vec![(q, Kind::Add)]);
        assert_eq!(d.collect_range(Order::Spo, &pat, 2), vec![(q, Kind::Add)]);
        // At/after the delete: effective Tombstone (older snapshots intact).
        assert_eq!(
            d.collect_range(Order::Pos, &pat, 3),
            vec![(q, Kind::Tombstone)]
        );
        assert_eq!(d.collect_range(Order::Spo, &pat, 1), vec![(q, Kind::Add)]);
    }

    #[test]
    fn gc_compacts_to_the_min_live_epoch() {
        let d = Delta::new(&[Order::Spo, Order::Pos], ranges());
        let base_quad: QuadKey = [1, 2, 3, 0]; // in_base = true
        let over_quad: QuadKey = [100, 2, 3, 0]; // overlay-only
                                                 // base_quad: delete(1), re-add(2), delete(5).
        d.record(&[(base_quad, Kind::Tombstone)], 1);
        d.record(&[(base_quad, Kind::Add)], 2);
        // over_quad: add(3), delete(4).
        d.record(&[(over_quad, Kind::Add)], 3);
        d.record(&[(over_quad, Kind::Tombstone)], 4);
        d.record(&[(base_quad, Kind::Tombstone)], 5);
        assert_eq!(d.events(), 5);

        // Floor at 3: base_quad's effective Add(2) restates base membership
        // (delete/re-add chain) → the Tombstone(1)+Add(2) prefix drops, the
        // future Tombstone(5) stays. over_quad's effective Add(3) is real
        // content → stays, along with its future Tombstone(4).
        let reclaimed = d.gc(3, |q| q == base_quad);
        assert_eq!(reclaimed, 2);
        assert_eq!(d.events(), 3);
        // A reader at 3 now falls through to the base for base_quad …
        assert_eq!(d.probe(base_quad, 3), None);
        // … and later epochs still see the delete.
        assert_eq!(d.probe(base_quad, 5), Some(Kind::Tombstone));
        assert_eq!(d.probe(over_quad, 3), Some(Kind::Add));

        // Floor at 5 (only the newest reader): over_quad's Tombstone
        // restates base absence → entry vanishes in every index; the
        // base_quad tombstone is load-bearing and survives.
        let reclaimed = d.gc(5, |q| q == base_quad);
        assert_eq!(reclaimed, 2);
        assert_eq!(d.events(), 1);
        assert_eq!(d.probe(over_quad, 5), None);
        assert_eq!(d.probe(base_quad, 5), Some(Kind::Tombstone));
        let pat = Pattern::default();
        assert_eq!(
            d.collect_range(Order::Pos, &pat, 5),
            vec![(base_quad, Kind::Tombstone)]
        );
    }

    #[test]
    fn collect_range_orders_and_filters() {
        let d = Delta::new(&[Order::Spo, Order::Pos], ranges());
        let quads: Vec<QuadKey> = vec![[5, 1, 9, 0], [5, 2, 7, 1], [6, 1, 7, 0], [6, 1, 8, 2]];
        let entries: Vec<(QuadKey, Kind)> = quads.iter().map(|&q| (q, Kind::Add)).collect();
        d.record(&entries, 1);

        // Bound-s range in SPO order.
        let pat = Pattern {
            s: Some(6),
            ..Pattern::default()
        };
        let got = d.collect_range(Order::Spo, &pat, 1);
        assert_eq!(
            got,
            vec![([6, 1, 7, 0], Kind::Add), ([6, 1, 8, 2], Kind::Add)]
        );

        // Bound-p in POS order: ascending (p, o, s, g).
        let pat = Pattern {
            p: Some(1),
            ..Pattern::default()
        };
        let got = d.collect_range(Order::Pos, &pat, 1);
        assert_eq!(
            got,
            vec![
                ([6, 1, 7, 0], Kind::Add),
                ([6, 1, 8, 2], Kind::Add),
                ([5, 1, 9, 0], Kind::Add),
            ]
        );

        // Residual graph filter.
        let pat = Pattern {
            g: Some(0),
            ..Pattern::default()
        };
        let got = d.collect_range(Order::Spo, &pat, 1);
        assert_eq!(
            got,
            vec![([5, 1, 9, 0], Kind::Add), ([6, 1, 7, 0], Kind::Add)]
        );
    }

    #[test]
    fn graph_scoped_collect_stays_zipper_ordered() {
        // Graph-first index keys must still hand the zipper ascending
        // permuted (x, y, z, g) sequences — bound-graph runs are
        // contiguous, unbound-graph collections re-sort.
        let d = Delta::new(&[Order::Spo], ranges());
        let quads: Vec<QuadKey> = vec![[9, 1, 1, 2], [5, 1, 1, 2], [7, 1, 1, 1], [6, 1, 1, 2]];
        let entries: Vec<(QuadKey, Kind)> = quads.iter().map(|&q| (q, Kind::Add)).collect();
        d.record(&entries, 1);
        let pat = Pattern {
            g: Some(2),
            ..Pattern::default()
        };
        assert_eq!(
            d.collect_range(Order::Spo, &pat, 1),
            vec![
                ([5, 1, 1, 2], Kind::Add),
                ([6, 1, 1, 2], Kind::Add),
                ([9, 1, 1, 2], Kind::Add),
            ]
        );
        // Unbound graph: ascending (s, p, o, g) across graphs.
        assert_eq!(
            d.collect_range(Order::Spo, &Pattern::default(), 1),
            vec![
                ([5, 1, 1, 2], Kind::Add),
                ([6, 1, 1, 2], Kind::Add),
                ([7, 1, 1, 1], Kind::Add),
                ([9, 1, 1, 2], Kind::Add),
            ]
        );
    }

    #[test]
    fn estimate_range_clips_prefixes_and_bounds_the_rest() {
        let d = Delta::new(&[Order::Spo], ranges());
        let quads: Vec<QuadKey> = vec![[5, 1, 9, 0], [5, 2, 7, 1], [6, 1, 7, 1], [6, 2, 8, 1]];
        let entries: Vec<(QuadKey, Kind)> = quads.iter().map(|&q| (q, Kind::Add)).collect();
        d.record(&entries, 1);
        // Graph-bound (the engine's every scan): clipped to the graph run.
        let g1 = Pattern {
            g: Some(1),
            ..Pattern::default()
        };
        assert_eq!(d.estimate_range(Order::Spo, &g1), 3);
        // Graph+subject: a deeper prefix.
        let g1s6 = Pattern {
            s: Some(6),
            g: Some(1),
            ..Pattern::default()
        };
        assert_eq!(d.estimate_range(Order::Spo, &g1s6), 2);
        // Non-prefix bounds (s without g): the O(1) upper bound — every
        // live event — instead of a whole-map walk.
        let s5 = Pattern {
            s: Some(5),
            ..Pattern::default()
        };
        assert_eq!(d.estimate_range(Order::Spo, &s5), d.events());
    }
}
