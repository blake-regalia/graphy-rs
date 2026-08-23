//! Streaming dictionary merge (doc 07 §6.2 Phase A, M5): build generation
//! G+1's dictionary from G's **already-sorted** PFC sections and the
//! (budget-bounded) overlay — no re-interning, no hash map over every
//! distinct term. Memory is the new sections' compressed PFC buffers, one
//! `u64` remap slot per *old* id, and usage bitmaps; the raw term bytes
//! never accumulate.
//!
//! Three passes:
//! 1. **Usage pre-pass** ([`usage_prepass`]): one id-level scan of the
//!    freeze snapshot marks which old ids the merged dataset references
//!    per position (garbage detection — a term whose quads were all
//!    tombstoned simply never gets marked — and the raw material for role
//!    migration), plus a closure over referenced triple terms so their
//!    component terms count as used. Overlay object terms that are quoted
//!    triples are split here (recursively) into component byte-terms —
//!    those may be brand-new terms that exist nowhere else.
//! 2. **Section merge** ([`merge_dictionaries`]): one global k-way byte
//!    merge over the five base PFC iterators, the four sorted overlay term
//!    lists, and the split-out triple-term component lists. Equal bytes
//!    across sources meet at the heap head, so each distinct term's global
//!    role set (union over all its old ids' usage) is known exactly when
//!    it is emitted — section membership (shared / subjects / objects /
//!    predicates / graphs) is decided per the same rules as
//!    `dict::finalize_parts`, the new PFC builders receive the term in
//!    sorted order, and every contributing old id gets its remap entry.
//!    Unreferenced terms are dropped here.
//! 3. **Triple-term canonicalization**: referenced base records (remapped
//!    component-wise) and overlay records (resolved through the merge)
//!    are grouped by depth, sorted on their resolved `[s, p, o]`, and
//!    deduplicated — exactly `finalize_parts`' canonical order, so the
//!    output stays byte-identical to an offline rebuild.
//!
//! The output ([`MergedDict`]) feeds `builder::build_from_ids`: Phase B
//! streams the snapshot a second time, rewriting each column through the
//! remap tables ([`MergedDict::map_*`]) — ordinals in, ordinals out.

use std::cmp::Reverse;
use std::collections::{BTreeMap, BinaryHeap, HashMap};

use graphy_core::TermId;
use graphy_succinct::{Pfc, PfcBuilder};

use crate::bt::Order;
use crate::dict::{split_triple_term, Sections, TT_TAG};
use crate::format::StoreError;
use crate::scan::QuadBatch;
use crate::segment::{Pattern, TermPos};
use crate::store::Snapshot;

const NONE: u64 = u64::MAX;
/// Remap tags for section values whose final column value needs the shared
/// count (unknown until the merge pass ends): bit 63 marks "+ n_shared".
const FIX: u64 = 1 << 63;

/// One bit per old id and position: does the merged dataset reference it?
#[derive(Debug)]
pub(crate) struct Usage {
    subj: Bits,
    pred: Bits,
    obj: Bits,
    graph: Bits,
    tt: Bits,
}

#[derive(Debug)]
struct Bits(Vec<u64>);

impl Bits {
    fn new(n: u64) -> Bits {
        Bits(vec![0; (n as usize).div_ceil(64)])
    }

    #[inline]
    fn set(&mut self, i: u64) {
        self.0[(i / 64) as usize] |= 1 << (i % 64);
    }

    #[inline]
    fn get(&self, i: u64) -> bool {
        self.0
            .get((i / 64) as usize)
            .is_some_and(|w| w & (1 << (i % 64)) != 0)
    }
}

/// Id-level usage scan of the freeze snapshot (pass 1). Bitmaps cover the
/// base spaces plus the overlay extensions.
pub(crate) fn usage_prepass(snap: &Snapshot) -> Result<Usage, StoreError> {
    let seg = snap.segment();
    let c = &seg.manifest.counts;
    let ov = |pos| snap.overlay_len(pos);
    let mut u = Usage {
        subj: Bits::new(c.shared + c.subjects + ov(TermPos::Subject)),
        pred: Bits::new(c.predicates + ov(TermPos::Predicate)),
        obj: Bits::new(c.shared + c.objects + ov(TermPos::Object)),
        graph: Bits::new(c.graphs + ov(TermPos::Graph)),
        tt: Bits::new(c.triple_terms),
    };

    let mut scan = snap.scan(&Pattern::default(), Order::Spo)?;
    let mut batch = QuadBatch::new();
    while scan.next_batch(&mut batch)? {
        for i in 0..batch.len() {
            u.subj.set(batch.s[i]);
            u.pred.set(batch.p[i]);
            match batch.o[i] >> 60 {
                0x0 => u.obj.set(batch.o[i]),
                0x7 => u.tt.set(batch.o[i] & !TT_TAG),
                _ => {} // inline values live outside the dictionary
            }
            if batch.g[i] > 0 {
                u.graph.set(batch.g[i] - 1);
            }
        }
    }

    // Closure: components of referenced triple terms are referenced.
    // Records are depth-ordered (components precede referents), so a
    // descending walk marks inner records before reaching them.
    let records = seg.tt_records();
    for i in (0..records.len()).rev() {
        if !u.tt.get(i as u64) {
            continue;
        }
        let [s, p, o] = records[i];
        u.subj.set(s);
        u.pred.set(p);
        match o >> 60 {
            0x0 => u.obj.set(o),
            0x7 => u.tt.set(o & !TT_TAG),
            _ => {}
        }
    }
    Ok(u)
}

/// An overlay triple term split to its parts: components are either final
/// references into the merged dictionary (resolved after the section
/// merge), inline ids, or inner overlay-tt indices.
#[derive(Debug, Clone, Copy)]
enum TtComp {
    /// Index into the extra component list for the given role.
    Extra(usize),
    /// An inline `TermId` (passes through every generation unchanged).
    Inline(u64),
    /// Inner overlay triple term (index into `ov_tts`).
    Inner(usize),
}

/// The merged dictionary: new sections + everything Phase B needs to
/// rewrite old ids into the new id space.
pub(crate) struct MergedDict {
    pub sections: Sections,
    pub tt_records: Vec<[u64; 3]>,
    pub counts_dict: [u64; 5],
    /// Old subject-space id → new subject-space value ([`NONE`] = unused).
    r_subj: Vec<u64>,
    r_pred: Vec<u64>,
    r_obj: Vec<u64>,
    /// Old graph SECTION ordinal → new section ordinal.
    r_graph: Vec<u64>,
    /// Old base tt ordinal → new final id (tagged).
    r_tt: Vec<u64>,
}

impl MergedDict {
    #[inline]
    fn get(table: &[u64], v: u64, what: &str) -> Result<u64, StoreError> {
        match table.get(v as usize).copied() {
            Some(x) if x != NONE => Ok(x),
            _ => Err(StoreError::Corrupt(format!(
                "streaming merge: unmapped {what} id {v}"
            ))),
        }
    }

    pub fn map_subj(&self, v: u64) -> Result<u64, StoreError> {
        Self::get(&self.r_subj, v, "subject")
    }

    pub fn map_pred(&self, v: u64) -> Result<u64, StoreError> {
        Self::get(&self.r_pred, v, "predicate")
    }

    pub fn map_obj(&self, v: u64) -> Result<u64, StoreError> {
        match v >> 60 {
            0x0 => Self::get(&self.r_obj, v, "object"),
            0x7 => Self::get(&self.r_tt, v & !TT_TAG, "triple term"),
            _ => Ok(v), // inline
        }
    }

    /// Graph COLUMN value (0 = default graph) → new column value.
    pub fn map_graph(&self, v: u64) -> Result<u64, StoreError> {
        if v == 0 {
            return Ok(0);
        }
        Ok(Self::get(&self.r_graph, v - 1, "graph")? + 1)
    }
}

/// One source for the k-way merge: a sorted stream of (bytes, old id).
struct Source<'a> {
    it: Box<dyn Iterator<Item = (Vec<u8>, u64)> + 'a>,
    head: Option<(Vec<u8>, u64)>,
}

impl<'a> Source<'a> {
    fn new(mut it: Box<dyn Iterator<Item = (Vec<u8>, u64)> + 'a>) -> Source<'a> {
        let head = it.next();
        Source { it, head }
    }

    fn advance(&mut self) -> (Vec<u8>, u64) {
        let out = self.head.take().expect("advance past end");
        self.head = self.it.next();
        out
    }
}

/// Source indices (fixed layout; the merge dispatches on them).
const SRC_SHARED: usize = 0;
const SRC_SUBJECTS: usize = 1;
const SRC_PREDICATES: usize = 2;
const SRC_OBJECTS: usize = 3;
const SRC_GRAPHS: usize = 4;
const SRC_OV_SUBJ: usize = 5;
const SRC_OV_PRED: usize = 6;
const SRC_OV_OBJ: usize = 7;
const SRC_OV_GRAPH: usize = 8;
const SRC_X_SUBJ: usize = 9;
const SRC_X_PRED: usize = 10;
const SRC_X_OBJ: usize = 11;
const N_SRC: usize = 12;

/// Pass 2 + 3: the global section merge and triple-term canonicalization.
pub(crate) fn merge_dictionaries(snap: &Snapshot, usage: &Usage) -> Result<MergedDict, StoreError> {
    let seg = snap.segment();
    let c = &seg.manifest.counts;
    let n_sh_old = c.shared;
    let corrupt = |m: String| StoreError::Corrupt(m);

    // ---- Overlay terms, sorted; object-position triple terms split out.
    let sorted_overlay = |pos| {
        let mut v = snap.overlay_terms_at(pos);
        v.sort_unstable();
        v
    };
    let ov_subj = sorted_overlay(TermPos::Subject);
    let ov_pred = sorted_overlay(TermPos::Predicate);
    let ov_graph = sorted_overlay(TermPos::Graph);
    let ov_obj_all = sorted_overlay(TermPos::Object);

    // Split used overlay quoted-triple objects into component byte-terms
    // (per role) and structural keys; plain overlay objects stay put.
    let mut ov_obj: Vec<(Box<[u8]>, u64)> = Vec::new();
    let mut extra: [BTreeMap<Box<[u8]>, usize>; 3] = Default::default(); // subj, pred, obj
    let mut ov_tts: Vec<[TtComp; 3]> = Vec::new();
    let mut ov_tt_ids: HashMap<Box<[u8]>, usize> = HashMap::new();
    // Overlay object column value → its overlay tt index (for remapping).
    let mut ov_obj_tt: Vec<(u64, usize)> = Vec::new();

    fn add_extra(extra: &mut [BTreeMap<Box<[u8]>, usize>; 3], role: usize, bytes: &[u8]) -> usize {
        let n = extra[role].len();
        *extra[role].entry(bytes.into()).or_insert(n)
    }

    fn split_tt(
        bytes: &[u8],
        extra: &mut [BTreeMap<Box<[u8]>, usize>; 3],
        ov_tts: &mut Vec<[TtComp; 3]>,
        ov_tt_ids: &mut HashMap<Box<[u8]>, usize>,
    ) -> Result<usize, String> {
        if let Some(&i) = ov_tt_ids.get(bytes) {
            return Ok(i);
        }
        let (s, p, o) = split_triple_term(bytes)?;
        let cs = TtComp::Extra(add_extra(extra, 0, s));
        let cp = TtComp::Extra(add_extra(extra, 1, p));
        let co = if o.first() == Some(&0x09) {
            TtComp::Inner(split_tt(o, extra, ov_tts, ov_tt_ids)?)
        } else if let Some(id) = try_inline_object(o)? {
            TtComp::Inline(id)
        } else {
            TtComp::Extra(add_extra(extra, 2, o))
        };
        let i = ov_tts.len();
        ov_tts.push([cs, cp, co]);
        ov_tt_ids.insert(bytes.into(), i);
        Ok(i)
    }

    for (bytes, v) in ov_obj_all {
        if !usage.obj.get(v) {
            continue; // dead overlay term (added then deleted): garbage
        }
        if bytes.first() == Some(&0x09) {
            let i = split_tt(&bytes, &mut extra, &mut ov_tts, &mut ov_tt_ids).map_err(&corrupt)?;
            ov_obj_tt.push((v, i));
        } else {
            ov_obj.push((bytes, v));
        }
    }
    let filter_used = |v: Vec<(Box<[u8]>, u64)>, bits: &Bits| -> Vec<(Box<[u8]>, u64)> {
        v.into_iter().filter(|&(_, id)| bits.get(id)).collect()
    };
    let ov_subj = filter_used(ov_subj, &usage.subj);
    let ov_pred = filter_used(ov_pred, &usage.pred);
    let ov_graph_used: Vec<(Box<[u8]>, u64)> = ov_graph
        .into_iter()
        .filter(|&(_, id)| usage.graph.get(id - 1))
        .collect();

    // ---- Sources for the global merge. Base PFC iterators stream the
    // sections without materializing them; overlay/extra lists are
    // budget-bounded vectors.
    let [pfc_shared, pfc_subjects, pfc_predicates, pfc_objects, pfc_graphs] = seg.dict_pfcs();
    fn stream_pfc(pfc: &Pfc) -> Box<dyn Iterator<Item = (Vec<u8>, u64)> + '_> {
        Box::new(pfc.iter().zip(0u64..))
    }
    fn boxed(v: Vec<(Box<[u8]>, u64)>) -> Box<dyn Iterator<Item = (Vec<u8>, u64)> + 'static> {
        Box::new(v.into_iter().map(|(b, i)| (b.into_vec(), i)))
    }
    fn extra_src(
        m: &BTreeMap<Box<[u8]>, usize>,
    ) -> Box<dyn Iterator<Item = (Vec<u8>, u64)> + 'static> {
        let v: Vec<(Vec<u8>, u64)> = m
            .iter()
            .map(|(b, &i)| (b.clone().into_vec(), i as u64))
            .collect();
        Box::new(v.into_iter())
    }

    let mut sources: Vec<Source<'_>> = Vec::with_capacity(N_SRC);
    sources.push(Source::new(stream_pfc(pfc_shared)));
    sources.push(Source::new(stream_pfc(pfc_subjects)));
    sources.push(Source::new(stream_pfc(pfc_predicates)));
    sources.push(Source::new(stream_pfc(pfc_objects)));
    sources.push(Source::new(stream_pfc(pfc_graphs)));
    sources.push(Source::new(boxed(ov_subj)));
    sources.push(Source::new(boxed(ov_pred)));
    sources.push(Source::new(boxed(ov_obj)));
    sources.push(Source::new(boxed(ov_graph_used)));
    sources.push(Source::new(extra_src(&extra[0])));
    sources.push(Source::new(extra_src(&extra[1])));
    sources.push(Source::new(extra_src(&extra[2])));

    // ---- Remap tables over the old id spaces.
    let ov = |pos| snap.overlay_len(pos);
    let mut r_subj = vec![NONE; (c.shared + c.subjects + ov(TermPos::Subject)) as usize];
    let mut r_pred = vec![NONE; (c.predicates + ov(TermPos::Predicate)) as usize];
    let mut r_obj = vec![NONE; (c.shared + c.objects + ov(TermPos::Object)) as usize];
    let mut r_graph = vec![NONE; (c.graphs + ov(TermPos::Graph)) as usize];
    let mut x_subj = vec![NONE; extra[0].len()];
    let mut x_pred = vec![NONE; extra[1].len()];
    let mut x_obj = vec![NONE; extra[2].len()];

    // ---- The global merge.
    let mut b_shared = PfcBuilder::new(32);
    let mut b_subjects = PfcBuilder::new(32);
    let mut b_predicates = PfcBuilder::new(32);
    let mut b_objects = PfcBuilder::new(32);
    let mut b_graphs = PfcBuilder::new(32);
    let (mut n_shared, mut n_subjects, mut n_predicates, mut n_objects, mut n_graphs) =
        (0u64, 0u64, 0u64, 0u64, 0u64);

    let mut heap: BinaryHeap<Reverse<(Vec<u8>, usize)>> = BinaryHeap::new();
    for (i, s) in sources.iter().enumerate() {
        if let Some((b, _)) = &s.head {
            heap.push(Reverse((b.clone(), i)));
        }
    }
    // Per-group scratch: (source, old id) pairs of the current term.
    let mut group: Vec<(usize, u64)> = Vec::new();
    while let Some(Reverse((bytes, first_src))) = heap.pop() {
        group.clear();
        let (_, id) = sources[first_src].advance();
        group.push((first_src, id));
        if let Some((b, _)) = &sources[first_src].head {
            heap.push(Reverse((b.clone(), first_src)));
        }
        // Pull every source whose head equals `bytes`.
        while let Some(Reverse((b, src))) = heap.peek() {
            if *b != bytes {
                break;
            }
            let src = *src;
            heap.pop();
            let (_, id) = sources[src].advance();
            group.push((src, id));
            if let Some((b, _)) = &sources[src].head {
                heap.push(Reverse((b.clone(), src)));
            }
        }

        // Union this term's roles across all of its old ids.
        let (mut s_used, mut o_used, mut p_used, mut g_used) = (false, false, false, false);
        for &(src, id) in &group {
            match src {
                SRC_SHARED => {
                    s_used |= usage.subj.get(id);
                    o_used |= usage.obj.get(id);
                }
                SRC_SUBJECTS => s_used |= usage.subj.get(n_sh_old + id),
                SRC_PREDICATES => p_used |= usage.pred.get(id),
                SRC_OBJECTS => o_used |= usage.obj.get(n_sh_old + id),
                SRC_GRAPHS => g_used |= usage.graph.get(id),
                SRC_OV_SUBJ => s_used = true, // pre-filtered to used
                SRC_OV_PRED => p_used = true,
                SRC_OV_OBJ => o_used = true,
                SRC_OV_GRAPH => g_used = true,
                SRC_X_SUBJ => s_used = true, // component of a used tt
                SRC_X_PRED => p_used = true,
                SRC_X_OBJ => o_used = true,
                _ => unreachable!(),
            }
        }

        // Section membership (same rules as dict::finalize_parts).
        let (new_s, new_o) = match (s_used, o_used) {
            (true, true) => {
                b_shared.push(&bytes);
                let i = n_shared;
                n_shared += 1;
                (Some(i), Some(i))
            }
            (true, false) => {
                b_subjects.push(&bytes);
                let i = FIX | n_subjects;
                n_subjects += 1;
                (Some(i), None)
            }
            (false, true) => {
                b_objects.push(&bytes);
                let i = FIX | n_objects;
                n_objects += 1;
                (None, Some(i))
            }
            (false, false) => (None, None),
        };
        let new_p = p_used.then(|| {
            b_predicates.push(&bytes);
            let i = n_predicates;
            n_predicates += 1;
            i
        });
        let new_g = g_used.then(|| {
            b_graphs.push(&bytes);
            let i = n_graphs;
            n_graphs += 1;
            i
        });

        for &(src, id) in &group {
            match src {
                SRC_SHARED => {
                    if let Some(v) = new_s {
                        r_subj[id as usize] = v;
                    }
                    if let Some(v) = new_o {
                        r_obj[id as usize] = v;
                    }
                }
                SRC_SUBJECTS => {
                    if let Some(v) = new_s {
                        r_subj[(n_sh_old + id) as usize] = v;
                    }
                }
                SRC_OBJECTS => {
                    if let Some(v) = new_o {
                        r_obj[(n_sh_old + id) as usize] = v;
                    }
                }
                SRC_PREDICATES => {
                    if let Some(v) = new_p {
                        r_pred[id as usize] = v;
                    }
                }
                SRC_GRAPHS => {
                    if let Some(v) = new_g {
                        r_graph[id as usize] = v;
                    }
                }
                SRC_OV_SUBJ => r_subj[id as usize] = new_s.expect("used overlay subject"),
                SRC_OV_PRED => r_pred[id as usize] = new_p.expect("used overlay predicate"),
                SRC_OV_OBJ => r_obj[id as usize] = new_o.expect("used overlay object"),
                SRC_OV_GRAPH => r_graph[(id - 1) as usize] = new_g.expect("used overlay graph"),
                SRC_X_SUBJ => x_subj[id as usize] = new_s.expect("tt component subject"),
                SRC_X_PRED => x_pred[id as usize] = new_p.expect("tt component predicate"),
                SRC_X_OBJ => x_obj[id as usize] = new_o.expect("tt component object"),
                _ => unreachable!(),
            }
        }
    }

    // ---- Fix up +n_shared tags now the shared count is known.
    let fixup = |t: &mut [u64]| {
        for v in t.iter_mut() {
            if *v != NONE && *v & FIX != 0 {
                *v = n_shared + (*v & !FIX);
            }
        }
    };
    fixup(&mut r_subj);
    fixup(&mut r_obj);
    fixup(&mut x_subj);
    fixup(&mut x_obj);

    // ---- Triple terms: remap referenced base records + resolve overlay
    // records, then canonicalize exactly like finalize_parts (depth
    // groups, sorted resolved records) with cross-source dedup.
    let base_records = seg.tt_records();
    let mut base_depth = vec![0u32; base_records.len()];
    for (i, r) in base_records.iter().enumerate() {
        base_depth[i] = match r[2] >> 60 {
            0x7 => base_depth[(r[2] & !TT_TAG) as usize] + 1,
            _ => 1,
        };
    }
    let mut ov_depth = vec![0u32; ov_tts.len()];
    for (i, r) in ov_tts.iter().enumerate() {
        ov_depth[i] = match r[2] {
            TtComp::Inner(j) => ov_depth[j] + 1,
            _ => 1,
        };
    }
    let max_depth = base_depth
        .iter()
        .chain(ov_depth.iter())
        .copied()
        .max()
        .unwrap_or(0);

    let mut r_tt = vec![NONE; base_records.len()];
    let mut ov_tt_final = vec![NONE; ov_tts.len()];
    let mut records: Vec<[u64; 3]> = Vec::new();
    for depth in 1..=max_depth {
        // (resolved record, source): base first — identical records dedup
        // to one ordinal regardless of origin.
        let mut level: Vec<([u64; 3], bool, usize)> = Vec::new();
        for (i, r) in base_records.iter().enumerate() {
            if base_depth[i] != depth || !usage.tt.get(i as u64) {
                continue;
            }
            let s = MergedDict::get(&r_subj, r[0], "tt subject")?;
            let p = MergedDict::get(&r_pred, r[1], "tt predicate")?;
            let o = match r[2] >> 60 {
                0x0 => MergedDict::get(&r_obj, r[2], "tt object")?,
                0x7 => {
                    let inner = r_tt[(r[2] & !TT_TAG) as usize];
                    debug_assert_ne!(inner, NONE, "lower depth resolved first");
                    inner
                }
                _ => r[2],
            };
            level.push(([s, p, o], false, i));
        }
        for (i, r) in ov_tts.iter().enumerate() {
            if ov_depth[i] != depth {
                continue;
            }
            let comp = |c: &TtComp, table: &[u64], what: &str| -> Result<u64, StoreError> {
                match c {
                    TtComp::Extra(j) => MergedDict::get(table, *j as u64, what),
                    TtComp::Inline(id) => Ok(*id),
                    TtComp::Inner(j) => {
                        let v = ov_tt_final[*j];
                        debug_assert_ne!(v, NONE, "lower depth resolved first");
                        Ok(v)
                    }
                }
            };
            let s = comp(&r[0], &x_subj, "tt component subject")?;
            let p = comp(&r[1], &x_pred, "tt component predicate")?;
            let o = comp(&r[2], &x_obj, "tt component object")?;
            level.push(([s, p, o], true, i));
        }
        level.sort_unstable();
        let mut last: Option<[u64; 3]> = None;
        for (record, is_ov, i) in level {
            if last != Some(record) {
                last = Some(record);
                records.push(record);
            }
            let final_id = TT_TAG | (records.len() as u64 - 1);
            if is_ov {
                ov_tt_final[i] = final_id;
            } else {
                r_tt[i] = final_id;
            }
        }
    }
    // Overlay object ids whose bytes were quoted triples remap to their
    // tt final ids through the object table.
    for (v, i) in ov_obj_tt {
        r_obj[v as usize] = ov_tt_final[i];
    }

    let sections = Sections {
        shared: b_shared.build(),
        subjects: b_subjects.build(),
        predicates: b_predicates.build(),
        objects: b_objects.build(),
        graphs: b_graphs.build(),
    };
    Ok(MergedDict {
        counts_dict: [n_shared, n_subjects, n_predicates, n_objects, n_graphs],
        sections,
        tt_records: records,
        r_subj,
        r_pred,
        r_obj,
        r_graph,
        r_tt,
    })
}

/// Inline-encode a component object literal exactly like
/// `BuildDict::intern_object` does (canonical-form-only gate).
fn try_inline_object(bytes: &[u8]) -> Result<Option<u64>, String> {
    match graphy_core::concise::decode(bytes).map_err(|e| e.to_string())? {
        graphy_core::TermRef::Literal(l) if l.lang().is_none() => {
            Ok(TermId::try_inline(l.lexical(), l.datatype()).map(|id| id.raw()))
        }
        _ => Ok(None),
    }
}
