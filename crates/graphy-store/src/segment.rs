//! Segment reader (doc 02 §6): heap and mmap open modes over the same
//! cursor-based parsing. Heap mode reads and checksums every component;
//! mmap mode maps them and takes zero-copy views (headers and structure
//! validated, payload digests left to `verify`). Provides id↔term
//! resolution, pattern scans with exact counts, and `verify`. The batched
//! scan seam over one segment is `scan.rs` (`SegmentScan`); the snapshot
//! level (base∪delta, the engine's `QuadScan`) lives in `store.rs`.

use std::path::{Path, PathBuf};

use graphy_core::{concise, InlineValue, TermId, TermRef};
use graphy_succinct::{BitVector, Bytes, Cursor, Pfc, WaveletMatrix};
use roaring::RoaringTreemap;

use crate::bt::Bt;
pub use crate::bt::Order;
use crate::foq::Foq;
use crate::format::{map_component, read_component, Kind, StoreError};
use crate::manifest::Manifest;
use crate::sidecar::HashSidecar;

/// How to load component payloads at open (doc 02 §6).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpenMode {
    /// Read every component fully into anonymous memory, verifying payload
    /// digests — the latency-predictable mode, and the one `verify` uses.
    Heap,
    /// Memory-map components and hold zero-copy views; pages fault in on
    /// demand. Headers and structure are validated; payload digests are not
    /// (that would fault everything in — run `verify` for integrity).
    Mmap,
}

/// A quad of column values: (subject dense id, predicate dense id, object
/// value, graph column value — 0 = default graph).
pub type QuadId = [u64; 4];

/// Term position, selecting the id space (doc 02 §7: predicate/graph spaces
/// are separate — the same IRI can hold different ids per position).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TermPos {
    Subject,
    Predicate,
    Object,
    Graph,
}

/// A scan pattern over column values (see [`Segment::resolve_pattern`]).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Pattern {
    pub s: Option<u64>,
    pub p: Option<u64>,
    pub o: Option<u64>,
    /// `Some(0)` = default graph only; `Some(k)` = named graph `k`.
    pub g: Option<u64>,
}

#[derive(Debug)]
pub(crate) struct Graphs {
    pub(crate) at: Vec<RoaringTreemap>,
    pub(crate) tg_starts: BitVector,
    pub(crate) tg: WaveletMatrix,
}

impl Graphs {
    /// Quad-list index range of SPO triple ordinal `ord` (its tg group).
    pub(crate) fn quad_range(&self, ord: u64) -> (u64, u64) {
        (quad_range_start(self, ord), quad_range_start(self, ord + 1))
    }
}

/// A dictionary section: PFC plus its optional term→ordinal hash sidecar
/// (docs/08 §4 — rebuildable, so a missing or malformed sidecar downgrades
/// to PFC binary search instead of failing the open).
#[derive(Debug)]
struct Section {
    pfc: Pfc,
    hash: Option<HashSidecar>,
}

impl Section {
    fn len(&self) -> usize {
        self.pfc.len()
    }

    fn pfc(&self) -> &Pfc {
        &self.pfc
    }

    fn get(&self, i: usize) -> Option<Vec<u8>> {
        self.pfc.get(i)
    }

    fn locate(&self, key: &[u8]) -> Option<usize> {
        match &self.hash {
            Some(h) => h.locate(key, &self.pfc),
            None => self.pfc.locate(key),
        }
    }
}

/// An open (heap-mode) base segment.
#[derive(Debug)]
pub struct Segment {
    pub manifest: Manifest,
    dir: PathBuf,
    shared: Section,
    subjects: Section,
    predicates: Section,
    objects: Section,
    graphs_dict: Section,
    tt_records: Vec<[u64; 3]>,
    orderings: Vec<(Order, Bt)>,
    graphs: Option<Graphs>,
    /// FoQ accessors (compact profile only, docs/08 §4).
    foq: Option<Foq>,
}

/// Load one component payload per the open mode (heap: read + digest check;
/// mmap: header-validated zero-copy view). Both paths yield 8-byte-aligned
/// payload bytes, so all downstream parsing is cursor/view based.
fn load_payload(dir: &Path, rel: &str, kind: Kind, mode: OpenMode) -> Result<Bytes, StoreError> {
    let path = dir.join(rel);
    match mode {
        OpenMode::Heap => Ok(Bytes::from_vec_aligned(read_component(&path, kind)?)),
        OpenMode::Mmap => map_component(&path, kind),
    }
}

impl Segment {
    /// Open in [`OpenMode::Heap`] (full digest verification).
    pub fn open(dir: &Path) -> Result<Segment, StoreError> {
        Segment::open_with(dir, OpenMode::Heap)
    }

    pub fn open_with(dir: &Path, mode: OpenMode) -> Result<Segment, StoreError> {
        let manifest = Manifest::load(dir)?;
        Segment::open_from(manifest, dir, &|rel, kind| {
            load_payload(dir, rel, kind, mode)
        })
    }

    /// Open from an embedded byte image `[(rel, bytes)]` (docs/11 M12a) —
    /// the heap-mode open path with reads served from memory, including the
    /// per-component header/digest verification.
    pub fn open_embedded(files: &[(&str, &[u8])]) -> Result<Segment, StoreError> {
        let find = |rel: &str| files.iter().find(|(r, _)| *r == rel).map(|(_, b)| *b);
        let manifest_bytes = find(crate::manifest::MANIFEST_NAME).ok_or_else(|| {
            StoreError::Manifest("embedded segment image lacks a manifest".into())
        })?;
        let manifest = Manifest::from_bytes(manifest_bytes)?;
        Segment::open_from(manifest, Path::new("<embedded>"), &|rel, kind| {
            let ctx = Path::new("<embedded>").join(rel);
            let bytes = find(rel).ok_or_else(|| {
                StoreError::format(&ctx, "missing from the embedded segment image")
            })?;
            Ok(Bytes::from_vec_aligned(crate::format::parse_component(
                bytes, kind, &ctx,
            )?))
        })
    }

    /// The shared open path: `load` serves component bytes by relative path
    /// (from a directory or an embedded image); `dir` only names things in
    /// errors and the segment's [`Segment::dir`].
    fn open_from(
        manifest: Manifest,
        dir: &Path,
        load: &dyn Fn(&str, Kind) -> Result<Bytes, StoreError>,
    ) -> Result<Segment, StoreError> {
        let dict = |name: &str| -> Result<Section, StoreError> {
            let rel = format!("dict/{name}.pfc");
            let payload = load(&rel, Kind::Dict)?;
            let mut c = Cursor::new(payload);
            let pfc = Pfc::deserialize_view(&mut c)
                .map_err(|e| StoreError::format(&dir.join(&rel), e.to_string()))?;
            if !c.is_empty() {
                return Err(StoreError::format(&dir.join(&rel), "trailing bytes"));
            }
            let hash = load(&format!("dict/{name}.hash"), Kind::HashSidecar)
                .ok()
                .and_then(|p| HashSidecar::deserialize(p, pfc.len()).ok());
            Ok(Section { pfc, hash })
        };
        let shared = dict("shared")?;
        let subjects = dict("subjects")?;
        let predicates = dict("predicates")?;
        let objects = dict("objects")?;
        let graphs_dict = dict("graphs")?;

        let tt_rel = "dict/triple_terms.bin";
        let tt_err = |e: String| StoreError::format(&dir.join(tt_rel), e);
        let tt_payload = load(tt_rel, Kind::TripleTerms)?;
        let tt_records = {
            let mut c = Cursor::new(tt_payload);
            let n = c.read_u64().map_err(|e| tt_err(e.to_string()))?;
            if n > c.remaining() as u64 / 24 {
                return Err(tt_err(format!("triple-term count {n} exceeds payload")));
            }
            let mut out = Vec::with_capacity(n as usize);
            for _ in 0..n {
                let mut rec = [0u64; 3];
                for slot in &mut rec {
                    *slot = c.read_u64().map_err(|e| tt_err(e.to_string()))?;
                }
                out.push(rec);
            }
            if !c.is_empty() {
                return Err(tt_err("trailing bytes".to_owned()));
            }
            out
        };

        let mut orderings = Vec::new();
        for name in &manifest.orderings {
            let order = Order::from_name(name)
                .ok_or_else(|| StoreError::Manifest(format!("unknown ordering {name}")))?;
            let rel = format!("idx/{name}.bt");
            let path = dir.join(&rel);
            let payload = load(&rel, Kind::BitmapTriples)?;
            let mut c = Cursor::new(payload);
            let bt =
                Bt::deserialize(&mut c).map_err(|e| StoreError::format(&path, e.to_string()))?;
            if !c.is_empty() {
                return Err(StoreError::format(&path, "trailing bytes"));
            }
            if bt.n_triples() != manifest.counts.triples {
                return Err(StoreError::Corrupt(format!(
                    "{name}: {} triples, manifest says {}",
                    bt.n_triples(),
                    manifest.counts.triples
                )));
            }
            // Pz iff non-SPO with graphs (docs/08 §4) — scan/count rely on it.
            let want_pz = order != Order::Spo && manifest.has_graphs;
            if bt.has_spo_payload() != want_pz {
                return Err(StoreError::Corrupt(format!(
                    "{name}: Pz payload {}",
                    if want_pz { "missing" } else { "unexpected" }
                )));
            }
            orderings.push((order, bt));
        }
        if !orderings.iter().any(|(o, _)| *o == Order::Spo) {
            return Err(StoreError::Manifest(
                "segment lacks the SPO ordering".into(),
            ));
        }

        let graphs = if manifest.has_graphs {
            let at_path = dir.join("graphs/at.roar");
            let at_err = |e: String| StoreError::format(&at_path, e);
            // Roaring's format needs real deserialization, so the graph
            // bitmaps live on the heap in every open mode.
            let payload = load("graphs/at.roar", Kind::GraphsAt)?;
            let mut c = Cursor::new(payload);
            let n = c.read_u64().map_err(|e| at_err(e.to_string()))?;
            if n > c.remaining() as u64 / 8 {
                return Err(at_err(format!("bitmap count {n} exceeds payload")));
            }
            let mut at = Vec::with_capacity(n as usize);
            for _ in 0..n {
                let len = c.read_u64().map_err(|e| at_err(e.to_string()))? as usize;
                let blob = c.take_bytes(len).map_err(|e| at_err(e.to_string()))?;
                let bm =
                    RoaringTreemap::deserialize_from(&*blob).map_err(|e| at_err(e.to_string()))?;
                at.push(bm);
                // Blobs are zero-padded to the 8-byte alignment rule.
                c.align8().map_err(|e| at_err(e.to_string()))?;
            }
            if !c.is_empty() {
                return Err(at_err("trailing bytes".to_owned()));
            }
            let tg_path = dir.join("graphs/tg.wm");
            let payload = load("graphs/tg.wm", Kind::GraphsTg)?;
            let mut c = Cursor::new(payload);
            let tg_starts = BitVector::deserialize_view(&mut c)
                .map_err(|e| StoreError::format(&tg_path, e.to_string()))?;
            let tg = WaveletMatrix::deserialize_view(&mut c)
                .map_err(|e| StoreError::format(&tg_path, e.to_string()))?;
            if !c.is_empty() {
                return Err(StoreError::format(&tg_path, "trailing bytes"));
            }
            if tg_starts.len() != tg.len() || tg_starts.count_ones() != manifest.counts.triples {
                return Err(StoreError::Corrupt("tg accessor shape mismatch".into()));
            }
            Some(Graphs { at, tg_starts, tg })
        } else {
            None
        };

        // FoQ accessors: required for the compact profile (they ARE its P-
        // and O-rooted access), absent otherwise.
        let foq = if manifest.profile == "compact" {
            let rel = "idx/foq.wm";
            let path = dir.join(rel);
            let payload = load(rel, Kind::Foq)?;
            let mut c = Cursor::new(payload);
            let foq =
                Foq::deserialize(&mut c).map_err(|e| StoreError::format(&path, e.to_string()))?;
            if !c.is_empty() {
                return Err(StoreError::format(&path, "trailing bytes"));
            }
            let spo = orderings
                .iter()
                .find(|(o, _)| *o == Order::Spo)
                .map(|(_, bt)| bt)
                .ok_or_else(|| StoreError::Manifest("segment lacks the SPO ordering".into()))?;
            if foq.wp.len() as u64 != spo.n_y() || foq.po.len() as u64 != spo.n_triples() {
                return Err(StoreError::Corrupt("foq shape does not match SPO".into()));
            }
            Some(foq)
        } else {
            None
        };

        let seg = Segment {
            manifest,
            dir: dir.to_owned(),
            shared,
            subjects,
            predicates,
            objects,
            graphs_dict,
            tt_records,
            orderings,
            graphs,
            foq,
        };
        seg.check_counts()?;
        Ok(seg)
    }

    fn check_counts(&self) -> Result<(), StoreError> {
        let c = &self.manifest.counts;
        let checks = [
            (self.shared.len() as u64, c.shared, "shared"),
            (self.subjects.len() as u64, c.subjects, "subjects"),
            (self.predicates.len() as u64, c.predicates, "predicates"),
            (self.objects.len() as u64, c.objects, "objects"),
            (self.graphs_dict.len() as u64, c.graphs, "graphs"),
            (self.tt_records.len() as u64, c.triple_terms, "triple terms"),
        ];
        for (got, want, what) in checks {
            if got != want {
                return Err(StoreError::Corrupt(format!(
                    "{what}: {got} entries, manifest says {want}"
                )));
            }
        }
        Ok(())
    }

    // ------------------------------------------------------- id ↔ term

    /// Concise bytes of a column value in the given position.
    pub fn decode_value(&self, v: u64, pos: TermPos) -> Result<Vec<u8>, StoreError> {
        let missing = |what: String| StoreError::Corrupt(what);
        let n_sh = self.manifest.counts.shared;
        match pos {
            TermPos::Subject => {
                if v < n_sh {
                    self.shared
                        .get(v as usize)
                        .ok_or_else(|| missing(format!("shared[{v}]")))
                } else {
                    self.subjects
                        .get((v - n_sh) as usize)
                        .ok_or_else(|| missing(format!("subjects[{v}]")))
                }
            }
            TermPos::Predicate => self
                .predicates
                .get(v as usize)
                .ok_or_else(|| missing(format!("predicates[{v}]"))),
            TermPos::Graph => self
                .graphs_dict
                .get(v as usize)
                .ok_or_else(|| missing(format!("graphs[{v}]"))),
            TermPos::Object => match v >> 60 {
                0x0 => {
                    if v < n_sh {
                        self.shared
                            .get(v as usize)
                            .ok_or_else(|| missing(format!("shared[{v}]")))
                    } else {
                        self.objects
                            .get((v - n_sh) as usize)
                            .ok_or_else(|| missing(format!("objects[{v}]")))
                    }
                }
                0x7 => {
                    let ord = (v & ((1 << 60) - 1)) as usize;
                    let rec = self
                        .tt_records
                        .get(ord)
                        .copied()
                        .ok_or_else(|| missing(format!("triple term [{ord}]")))?;
                    let s = self.decode_value(rec[0], TermPos::Subject)?;
                    let p = self.decode_value(rec[1], TermPos::Predicate)?;
                    let o = self.decode_value(rec[2], TermPos::Object)?;
                    let mut out = Vec::with_capacity(s.len() + p.len() + o.len() + 7);
                    concise::encode_triple_term(&mut out, &s, &p, &o);
                    Ok(out)
                }
                _ => {
                    let value = TermId::from_raw(v)
                        .decode()
                        .ok_or_else(|| missing(format!("inline id {v:#x}")))?;
                    Ok(inline_concise(&value))
                }
            },
        }
    }

    /// Column value of a concise term in the given position, if present.
    /// Sections resolve through their hash sidecars when loaded (O(1) +
    /// one PFC confirm) and fall back to PFC binary search otherwise.
    pub fn resolve_term(&self, bytes: &[u8], pos: TermPos) -> Option<u64> {
        let n_sh = self.manifest.counts.shared;
        let in_so = |bytes: &[u8], other: &Section| {
            self.shared
                .locate(bytes)
                .map(|i| i as u64)
                .or_else(|| other.locate(bytes).map(|i| n_sh + i as u64))
        };
        match pos {
            TermPos::Subject => in_so(bytes, &self.subjects),
            TermPos::Predicate => self.predicates.locate(bytes).map(|i| i as u64),
            TermPos::Graph => self.graphs_dict.locate(bytes).map(|i| i as u64),
            TermPos::Object => match concise::decode(bytes).ok()? {
                TermRef::Literal(l) => {
                    if l.lang().is_none() {
                        if let Some(id) = TermId::try_inline(l.lexical(), l.datatype()) {
                            return Some(id.raw());
                        }
                    }
                    in_so(bytes, &self.objects)
                }
                TermRef::TripleTerm(tt) => {
                    // Resolve components, then find the record (linear over
                    // the — typically tiny — triple-term section).
                    let mut enc = Vec::new();
                    let s = self.resolve_ref(tt.subject(), TermPos::Subject, &mut enc)?;
                    let p = self.resolve_ref(tt.predicate(), TermPos::Predicate, &mut enc)?;
                    let o = self.resolve_ref(tt.object(), TermPos::Object, &mut enc)?;
                    let rec = [s, p, o];
                    self.tt_records
                        .iter()
                        .position(|r| *r == rec)
                        .map(|i| (0x7 << 60) | i as u64)
                }
                _ => in_so(bytes, &self.objects),
            },
        }
    }

    fn resolve_ref(&self, t: TermRef<'_>, pos: TermPos, scratch: &mut Vec<u8>) -> Option<u64> {
        scratch.clear();
        encode_term_ref(&t, scratch);
        self.resolve_term(scratch, pos)
    }

    /// Build a [`Pattern`] from concise term bytes; `None` when a bound term
    /// does not occur in this segment (the pattern matches nothing).
    /// `g: Some(None)` = default graph; `None` = any graph.
    #[allow(clippy::type_complexity)]
    pub fn resolve_pattern(
        &self,
        s: Option<&[u8]>,
        p: Option<&[u8]>,
        o: Option<&[u8]>,
        g: Option<Option<&[u8]>>,
    ) -> Option<Pattern> {
        let mut pat = Pattern::default();
        if let Some(b) = s {
            pat.s = Some(self.resolve_term(b, TermPos::Subject)?);
        }
        if let Some(b) = p {
            pat.p = Some(self.resolve_term(b, TermPos::Predicate)?);
        }
        if let Some(b) = o {
            pat.o = Some(self.resolve_term(b, TermPos::Object)?);
        }
        match g {
            None => {}
            Some(None) => pat.g = Some(0),
            Some(Some(b)) => {
                pat.g = Some(self.resolve_term(b, TermPos::Graph)? + 1);
            }
        }
        Some(pat)
    }

    // ------------------------------------------------------------ scans

    /// The graph layer, when the segment has named graphs.
    pub(crate) fn graphs_layer(&self) -> Option<&Graphs> {
        self.graphs.as_ref()
    }

    /// The FoQ accessors (compact profile only).
    pub(crate) fn foq(&self) -> Option<&Foq> {
        self.foq.as_ref()
    }

    /// Orderings a [`crate::scan::SegmentScan`] can emit in: materialized ones,
    /// plus PSO/OSP served virtually by the FoQ accessors on compact.
    /// The five dictionary PFC sections in section order (shared,
    /// subjects, predicates, objects, graphs) — sorted-iteration input for
    /// the streaming dictionary merge (doc 07 §6.2).
    pub(crate) fn dict_pfcs(&self) -> [&graphy_succinct::Pfc; 5] {
        [
            self.shared.pfc(),
            self.subjects.pfc(),
            self.predicates.pfc(),
            self.objects.pfc(),
            self.graphs_dict.pfc(),
        ]
    }

    /// Triple-term records (`[s, p, o]` final ids, depth-ordered).
    pub(crate) fn tt_records(&self) -> &[[u64; 3]] {
        &self.tt_records
    }

    /// The segment's directory (minor merges write new components here).
    pub(crate) fn seg_dir(&self) -> &Path {
        &self.dir
    }

    pub fn scan_orders(&self) -> Vec<Order> {
        let mut orders: Vec<Order> = self.orderings.iter().map(|(o, _)| *o).collect();
        if self.foq.is_some() {
            for o in [Order::Pso, Order::Osp] {
                if !orders.contains(&o) {
                    orders.push(o);
                }
            }
        }
        orders
    }

    /// The materialized BitmapTriples for `order`, if this profile has it.
    pub(crate) fn ordering_bt(&self, order: Order) -> Option<&Bt> {
        self.orderings
            .iter()
            .find(|(o, _)| *o == order)
            .map(|(_, bt)| bt)
    }

    /// The scan order with the cheapest access for this pattern, FoQ
    /// virtual orderings included: materialized bound prefixes rank by
    /// depth; a bound object (OSP via the O-index) beats a bound predicate
    /// (PSO via Wp), and any materialized prefix beats both.
    pub(crate) fn best_scan_order(&self, pat: &Pattern) -> Order {
        let mut best = {
            let (order, _) = self.best_order(pat);
            (Self::prefix_len(pat, *order) * 4, *order)
        };
        if self.foq.is_some() {
            let foq_candidates = [
                (3, Order::Osp, pat.o.is_some()),
                (2, Order::Pso, pat.p.is_some()),
            ];
            for (score, order, applies) in foq_candidates {
                if applies && score > best.0 && self.ordering_bt(order).is_none() {
                    best = (score, order);
                }
            }
        }
        best.1
    }

    /// Length of `pat`'s bound prefix in `order`.
    fn prefix_len(pat: &Pattern, order: Order) -> usize {
        let bound = order.to_xyz(
            u64::from(pat.s.is_some()),
            u64::from(pat.p.is_some()),
            u64::from(pat.o.is_some()),
        );
        bound.iter().take_while(|&&b| b == 1).count()
    }

    /// Materialized ordering with the longest bound prefix for this pattern.
    pub(crate) fn best_order(&self, pat: &Pattern) -> &(Order, Bt) {
        let score = |order: Order| {
            let bound = order.to_xyz(
                u64::from(pat.s.is_some()),
                u64::from(pat.p.is_some()),
                u64::from(pat.o.is_some()),
            );
            let mut n = 0;
            for b in bound {
                if b == 1 {
                    n += 1;
                } else {
                    break;
                }
            }
            n
        };
        self.orderings
            .iter()
            .max_by_key(|(o, _)| score(*o))
            .expect("at least SPO present")
    }

    /// All matching quads (column values) in the best ordering's order.
    /// Row-materializing convenience over [`Segment::scan_order`] (the
    /// batched seam) — tests and the CLI exporter use this.
    pub fn scan(&self, pat: &Pattern) -> Result<Vec<QuadId>, StoreError> {
        let order = self.best_scan_order(pat);
        let mut scan = self.scan_order(pat, order)?;
        let mut batch = crate::scan::QuadBatch::new();
        let mut out = Vec::new();
        while scan.next_batch(&mut batch)? {
            for i in 0..batch.len() {
                out.push([batch.s[i], batch.p[i], batch.o[i], batch.g[i]]);
            }
        }
        Ok(out)
    }

    /// Exact count. Bound-prefix (+ optional graph) patterns avoid
    /// enumeration entirely; residual-filter patterns fall back to scan
    /// (which routes through the FoQ accessors on compact).
    pub fn count(&self, pat: &Pattern) -> Result<u64, StoreError> {
        // Compact-profile O(1) special case: a bound object's triple count
        // is its O-index run length.
        if let (Some(foq), None, None, Some(o), None, None) =
            (&self.foq, pat.s, pat.p, pat.o, pat.g, &self.graphs)
        {
            return Ok(match foq.locate_object(o) {
                None => 0,
                Some(j) => {
                    let (lo, hi) = foq.object_run(j);
                    hi - lo
                }
            });
        }
        let (order, bt) = self.best_order(pat);
        let bound = order.to_xyz(
            u64::from(pat.s.is_some()),
            u64::from(pat.p.is_some()),
            u64::from(pat.o.is_some()),
        );
        let n_bound = [pat.s, pat.p, pat.o].iter().flatten().count();
        let prefix_len = bound.iter().take_while(|&&b| b == 1).count();
        if prefix_len < n_bound {
            return Ok(self.scan(pat)?.len() as u64);
        }
        let want = order.to_xyz(
            pat.s.unwrap_or_default(),
            pat.p.unwrap_or_default(),
            pat.o.unwrap_or_default(),
        );
        let (lo, hi) = match prefix_len {
            0 => (0, bt.n_triples()),
            1 => match bt.x_group(want[0]) {
                None => (0, 0),
                Some(g) => bt.z_range_of_group(g),
            },
            2 => match bt.x_group(want[0]).and_then(|g| bt.find_y(g, want[1])) {
                None => (0, 0),
                Some(yi) => bt.z_range(yi),
            },
            _ => match bt
                .x_group(want[0])
                .and_then(|g| bt.find_y(g, want[1]))
                .and_then(|yi| bt.find_z(yi, want[2]))
            {
                None => (0, 0),
                Some(i) => (i, i + 1),
            },
        };
        match (pat.g, &self.graphs) {
            (None, None) | (Some(0), None) => Ok(hi - lo),
            (Some(_), None) => Ok(0),
            (g_filter, Some(g)) if *order == Order::Spo => Ok(match g_filter {
                // Total quads in range = sum of graph-set sizes.
                None => quad_range_start(g, hi) - quad_range_start(g, lo),
                Some(gv) => {
                    g.at.get(gv as usize)
                        .map_or(0, |bm| treemap_range_cardinality(bm, lo, hi))
                }
            }),
            (g_filter, Some(g)) => {
                // Secondary ordering: the range is not SPO-contiguous, so
                // walk it through Pz — per-ordinal bitmap probes, no SPO
                // lookups. (Pz presence is enforced at open; scan is the
                // defensive fallback.)
                if !bt.has_spo_payload() {
                    return Ok(self.scan(pat)?.len() as u64);
                }
                let mut n = 0;
                for i in lo..hi {
                    let ord = bt.spo_at(i).expect("Pz checked above");
                    n += match g_filter {
                        None => quad_range_start(g, ord + 1) - quad_range_start(g, ord),
                        Some(gv) => {
                            u64::from(g.at.get(gv as usize).is_some_and(|bm| bm.contains(ord)))
                        }
                    };
                }
                Ok(n)
            }
        }
    }

    /// Whether the base contains this exact quad (column-value key with the
    /// graph column convention). Out-of-range components (overlay ids) are
    /// definitively absent.
    pub(crate) fn contains_quad(&self, q: QuadId) -> bool {
        let c = &self.manifest.counts;
        if q[0] >= c.shared + c.subjects || q[1] >= c.predicates || q[3] > c.graphs {
            return false;
        }
        match q[2] >> 60 {
            0x0 if q[2] >= c.shared + c.objects => return false,
            0x7 if (q[2] & ((1 << 60) - 1)) >= c.triple_terms => return false,
            _ => {}
        }
        let spo = self.ordering_bt(Order::Spo).expect("SPO required at open");
        let Some(ord) = spo
            .x_group(q[0])
            .and_then(|g| spo.find_y(g, q[1]))
            .and_then(|yi| spo.find_z(yi, q[2]))
        else {
            return false;
        };
        match (&self.graphs, q[3]) {
            (None, 0) => true,
            (None, _) => false,
            (Some(g), gv) => g.at.get(gv as usize).is_some_and(|bm| bm.contains(ord)),
        }
    }

    /// SPO ordinal of a triple known to exist.
    pub(crate) fn spo_ordinal(&self, s: u64, p: u64, o: u64) -> Result<u64, StoreError> {
        let bt = &self
            .orderings
            .iter()
            .find(|(o, _)| *o == Order::Spo)
            .expect("SPO required at open")
            .1;
        bt.x_group(s)
            .and_then(|g| bt.find_y(g, p))
            .and_then(|yi| bt.find_z(yi, o))
            .ok_or_else(|| StoreError::Corrupt(format!("triple ({s},{p},{o}) missing from SPO")))
    }

    // ----------------------------------------------------------- verify

    /// Deep verification: manifest↔component digests (sidecars included —
    /// they are optional for `open`, but an on-disk sidecar that differs
    /// from the manifest is corruption), structural checks (already enforced
    /// at open), full order walks with Pz cross-checks, sidecar locate
    /// walks, graph-layer cross-checks. Returns the manifest on success.
    pub fn verify(dir: &Path) -> Result<Manifest, StoreError> {
        let seg = Segment::open(dir)?;
        // Manifest lists exactly the on-disk components with matching digests.
        for (rel, comp) in seg.manifest.components.iter().chain(&seg.manifest.sidecars) {
            let path = seg.dir.join(rel);
            let bytes = std::fs::read(&path).map_err(|e| StoreError::io(&path, e))?;
            if bytes.len() as u64 != comp.bytes + crate::format::HEADER_LEN as u64 {
                return Err(StoreError::Corrupt(format!("{rel}: size mismatch")));
            }
            let digest = xxhash_rust::xxh3::xxh3_64(&bytes[crate::format::HEADER_LEN..]);
            if format!("{digest:016x}") != comp.xxh3 {
                return Err(StoreError::Corrupt(format!(
                    "{rel}: digest differs from manifest"
                )));
            }
        }
        // Full order walk: strictly increasing triples in every ordering,
        // and every Pz entry is exactly the triple's SPO ordinal.
        for (order, bt) in &seg.orderings {
            let mut last: Option<[u64; 3]> = None;
            for i in 0..bt.n_triples() {
                let t = bt.triple_at(i);
                if let Some(l) = last {
                    if t <= l {
                        return Err(StoreError::Corrupt(format!(
                            "{}: ordinal {i} not increasing",
                            order.name()
                        )));
                    }
                }
                last = Some(t);
                if let Some(pz) = bt.spo_at(i) {
                    let [s, p, o] = order.to_spo(t[0], t[1], t[2]);
                    if pz != seg.spo_ordinal(s, p, o)? {
                        return Err(StoreError::Corrupt(format!(
                            "{}: ordinal {i} carries the wrong SPO ordinal",
                            order.name()
                        )));
                    }
                }
            }
        }
        // FoQ accessors (compact): Wp mirrors SPO's Sy symbol-for-symbol,
        // and the O-index is a permutation of the ordinals grouped by the
        // triples' actual object values.
        if let Some(foq) = &seg.foq {
            let spo = seg.ordering_bt(Order::Spo).expect("SPO required at open");
            for yi in 0..spo.n_y() {
                if foq.wp.access(yi as usize) != spo.y_at(yi) {
                    return Err(StoreError::Corrupt(format!(
                        "foq: Wp[{yi}] differs from SPO Sy"
                    )));
                }
            }
            let mut seen = vec![false; spo.n_triples() as usize];
            for j in 0..foq.n_objects() {
                let o = foq.xo.get(j as usize);
                let (lo, hi) = foq.object_run(j);
                let mut last = None;
                for r in lo..hi {
                    let ord = foq.po.get(r as usize);
                    if last.is_some_and(|l| ord <= l) {
                        return Err(StoreError::Corrupt(format!(
                            "foq: object run {j} not strictly increasing"
                        )));
                    }
                    last = Some(ord);
                    if std::mem::replace(&mut seen[ord as usize], true) {
                        return Err(StoreError::Corrupt(format!(
                            "foq: ordinal {ord} appears twice"
                        )));
                    }
                    if spo.triple_at(ord)[2] != o {
                        return Err(StoreError::Corrupt(format!(
                            "foq: ordinal {ord} filed under the wrong object"
                        )));
                    }
                }
            }
            if seen.iter().any(|&s| !s) {
                return Err(StoreError::Corrupt("foq: O-index misses ordinals".into()));
            }
        } else if seg.manifest.profile == "compact" {
            return Err(StoreError::Corrupt("compact segment lacks foq".into()));
        }
        // Sidecars listed in the manifest must have loaded (digests matched
        // above, so a deserialize failure is a builder bug, not bit rot) and
        // must locate every section term.
        for (name, section) in [
            ("shared", &seg.shared),
            ("subjects", &seg.subjects),
            ("predicates", &seg.predicates),
            ("objects", &seg.objects),
            ("graphs", &seg.graphs_dict),
        ] {
            let listed = seg
                .manifest
                .sidecars
                .contains_key(&format!("dict/{name}.hash"));
            let Some(hash) = &section.hash else {
                if listed {
                    return Err(StoreError::Corrupt(format!(
                        "dict/{name}.hash listed in manifest but unusable"
                    )));
                }
                continue;
            };
            for (i, term) in section.pfc.iter().enumerate() {
                if hash.locate(&term, &section.pfc) != Some(i) {
                    return Err(StoreError::Corrupt(format!(
                        "dict/{name}.hash cannot locate entry {i}"
                    )));
                }
            }
        }
        // Graph layer: bitmap cardinalities sum to the quad count.
        if let Some(g) = &seg.graphs {
            let total: u64 = g.at.iter().map(RoaringTreemap::len).sum();
            if total != seg.manifest.counts.quads {
                return Err(StoreError::Corrupt(format!(
                    "graph bitmaps hold {total} quads, manifest says {}",
                    seg.manifest.counts.quads
                )));
            }
        } else if seg.manifest.counts.quads != seg.manifest.counts.triples {
            return Err(StoreError::Corrupt(
                "triples-only segment with quads != triples".into(),
            ));
        }
        Ok(seg.manifest)
    }
}

/// Elements of `bm` in `[lo, hi)` via rank (`rank(v)` counts elements ≤ v).
fn treemap_range_cardinality(bm: &RoaringTreemap, lo: u64, hi: u64) -> u64 {
    if hi == 0 || lo >= hi {
        return 0;
    }
    let up_to_hi = bm.rank(hi - 1);
    let up_to_lo = if lo == 0 { 0 } else { bm.rank(lo - 1) };
    up_to_hi - up_to_lo
}

/// Number of quads before triple ordinal `lo` (via the tg group starts).
fn quad_range_start(g: &Graphs, ordinal: u64) -> u64 {
    if ordinal >= g.tg_starts.count_ones() {
        return g.tg_starts.len() as u64;
    }
    g.tg_starts.select1(ordinal).expect("in range") as u64
}

/// Concise bytes for an inline value (canonical datatyped literal, or the
/// simple form for inline xsd:string short strings — `^` spellings of
/// xsd:string are forbidden by the concise single-spelling invariant).
fn inline_concise(v: &InlineValue) -> Vec<u8> {
    let lexical = v.canonical_lexical();
    let dt = v.datatype_iri();
    let mut out = Vec::with_capacity(lexical.len() + dt.len() + 3);
    if dt == graphy_core::vocab::XSD_STRING {
        concise::encode_simple(&mut out, &lexical);
    } else {
        concise::encode_datatype(&mut out, &lexical, dt);
    }
    out
}

/// Re-encode a decoded term view into concise bytes.
fn encode_term_ref(t: &TermRef<'_>, out: &mut Vec<u8>) {
    match t {
        TermRef::Iri(i) => concise::encode_iri(out, i),
        TermRef::BlankNode(b) => concise::encode_blank(out, b),
        TermRef::Literal(l) => match l.lang() {
            Some((tag, dir)) => concise::encode_lang(out, l.lexical(), tag, dir),
            None => {
                let dt = l.datatype();
                if dt == graphy_core::vocab::XSD_STRING {
                    concise::encode_simple(out, l.lexical());
                } else {
                    concise::encode_datatype(out, l.lexical(), dt);
                }
            }
        },
        TermRef::TripleTerm(tt) => {
            let mut s = Vec::new();
            let mut p = Vec::new();
            let mut o = Vec::new();
            encode_term_ref(&tt.subject(), &mut s);
            encode_term_ref(&tt.predicate(), &mut p);
            encode_term_ref(&tt.object(), &mut o);
            concise::encode_triple_term(out, &s, &p, &o);
        }
    }
}

/// The embedded empty segment (docs/11 M12a): a builder-produced byte image
/// used as the base of [`Store::ephemeral`](crate::Store::ephemeral) stores.
/// Regenerate with `cargo run -p graphy-store --example gen_empty_segment`;
/// the `embedded_empty_segment_matches_builders` test guards drift.
pub const EMPTY_SEGMENT: &[(&str, &[u8])] = &[
    (
        "MANIFEST.json",
        include_bytes!("empty_segment/MANIFEST.json") as &[u8],
    ),
    (
        "dict/graphs.hash",
        include_bytes!("empty_segment/dict/graphs.hash") as &[u8],
    ),
    (
        "dict/graphs.pfc",
        include_bytes!("empty_segment/dict/graphs.pfc") as &[u8],
    ),
    (
        "dict/objects.hash",
        include_bytes!("empty_segment/dict/objects.hash") as &[u8],
    ),
    (
        "dict/objects.pfc",
        include_bytes!("empty_segment/dict/objects.pfc") as &[u8],
    ),
    (
        "dict/predicates.hash",
        include_bytes!("empty_segment/dict/predicates.hash") as &[u8],
    ),
    (
        "dict/predicates.pfc",
        include_bytes!("empty_segment/dict/predicates.pfc") as &[u8],
    ),
    (
        "dict/shared.hash",
        include_bytes!("empty_segment/dict/shared.hash") as &[u8],
    ),
    (
        "dict/shared.pfc",
        include_bytes!("empty_segment/dict/shared.pfc") as &[u8],
    ),
    (
        "dict/subjects.hash",
        include_bytes!("empty_segment/dict/subjects.hash") as &[u8],
    ),
    (
        "dict/subjects.pfc",
        include_bytes!("empty_segment/dict/subjects.pfc") as &[u8],
    ),
    (
        "dict/triple_terms.bin",
        include_bytes!("empty_segment/dict/triple_terms.bin") as &[u8],
    ),
    (
        "idx/osp.bt",
        include_bytes!("empty_segment/idx/osp.bt") as &[u8],
    ),
    (
        "idx/pos.bt",
        include_bytes!("empty_segment/idx/pos.bt") as &[u8],
    ),
    (
        "idx/spo.bt",
        include_bytes!("empty_segment/idx/spo.bt") as &[u8],
    ),
    (
        "stats/charsets.bin",
        include_bytes!("empty_segment/stats/charsets.bin") as &[u8],
    ),
    (
        "stats/pred.stats",
        include_bytes!("empty_segment/stats/pred.stats") as &[u8],
    ),
];
