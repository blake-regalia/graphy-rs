//! Segment builder (doc 07 §7): parse-time intern → provisional quad spill →
//! remap → external sort → streaming index construction. Bulk load is this
//! builder with an empty base; the M4+ merger reuses the same phases.
//!
//! Memory is bounded by the sort budget and the dictionary hash (the one
//! structure proportional to distinct terms — the parallel sharded-arena
//! upgrade comes with load parallelism).

use std::collections::HashMap;
use std::fs::File;
use std::io::{self, BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use graphy_core::{concise, TermRef};
use graphy_succinct::intvec::{bits_for, PackedIntsBuilder};
use graphy_succinct::serial::{write_u64, write_u64s};
use graphy_succinct::{BitVectorBuilder, ExtSorter, Pfc, PfcBuilder, WaveletMatrix};
use roaring::RoaringTreemap;

use crate::bt::{BtBuilder, BtSpillBuilder, Order};
use crate::dict::{
    BuildDict, Pos, Resolver, Sections, ShardedDict, SpillerSet, R_GRAPH, R_PRED, R_SUBJ,
};
use crate::format::{write_component, Kind, StoreError};
use crate::manifest::{Component, Counts, Manifest, FORMAT_VERSION};
use crate::sidecar::HashSidecar;

/// Index profile (doc 02 §5).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Profile {
    /// SPO only (FoQ wavelet accessors arrive with the read path work).
    Compact,
    /// SPO, POS, OSP (default).
    Balanced,
    /// All six orderings.
    Covering,
}

impl Profile {
    pub fn name(self) -> &'static str {
        match self {
            Profile::Compact => "compact",
            Profile::Balanced => "balanced",
            Profile::Covering => "covering",
        }
    }

    pub fn from_name(s: &str) -> Option<Profile> {
        Some(match s {
            "compact" => Profile::Compact,
            "balanced" => Profile::Balanced,
            "covering" => Profile::Covering,
            _ => return None,
        })
    }

    pub fn orderings(self) -> &'static [Order] {
        match self {
            Profile::Compact => &[Order::Spo],
            Profile::Balanced => &[Order::Spo, Order::Pos, Order::Osp],
            Profile::Covering => &[
                Order::Spo,
                Order::Sop,
                Order::Pos,
                Order::Pso,
                Order::Osp,
                Order::Ops,
            ],
        }
    }
}

#[derive(Debug, Clone)]
pub struct BuilderConfig {
    /// Segment output directory (created; must not contain a segment).
    pub dir: PathBuf,
    /// Scratch directory for spill runs (created; cleaned on success).
    pub scratch: PathBuf,
    pub profile: Profile,
    /// External-sort buffer budget in bytes.
    pub sort_budget: usize,
    /// Generation recorded in the manifest (0 for bulk loads; the merger
    /// stamps G+1).
    pub generation: u64,
    /// Cooperative duty-cycle cap on the build's hot loops (doc 07 §6.4):
    /// the merger paces its rebuild so foreground readers keep their
    /// latency; bulk loads leave it `None` (fastest build). Clamped to
    /// `(0, 1]`. Granularity is bounded by the external sort's spill/merge
    /// bursts (~`sort_budget`-sized), so a paced merge wants a smaller
    /// sort budget than a bulk load.
    pub pace_duty: Option<f64>,
    /// Materialized-ordering override (`None` = the profile's set). Major
    /// merges pass the base's CURRENT ordering list so lazily added
    /// orderings (minor merges, doc 07 §6.4) survive generation folds.
    /// Must contain [`Order::Spo`].
    pub orderings: Option<Vec<Order>>,
    /// Intern-dictionary memory budget in bytes (doc 07 §7): over it, the
    /// intern maps flush byte-sorted term runs to scratch and the
    /// finalize k-way merges them — output stays byte-identical to an
    /// unbudgeted build. `None` = keep every term resident (fastest).
    pub intern_budget: Option<usize>,
}

impl BuilderConfig {
    pub fn new(dir: impl Into<PathBuf>) -> BuilderConfig {
        let dir = dir.into();
        let scratch = dir.join(".scratch");
        BuilderConfig {
            dir,
            scratch,
            profile: Profile::Balanced,
            sort_budget: 256 << 20,
            generation: 0,
            pace_duty: None,
            orderings: None,
            intern_budget: None,
        }
    }
}

/// Streaming bulk-load builder: push concise-encoded quads, then `finish`.
/// For parallel ingestion (doc 07 §7), take [`SegmentBuilder::lanes`] and
/// feed each [`IngestLane`] from its own thread.
#[derive(Debug)]
pub struct SegmentBuilder {
    cfg: BuilderConfig,
    intern: Intern,
    spill: BufWriter<File>,
    spill_path: PathBuf,
    n_pushed: u64,
    /// Joined lane spills: (path, quads pushed through that lane).
    joined: Vec<(PathBuf, u64)>,
    next_lane: usize,
}

#[derive(Debug)]
enum Intern {
    Serial(Box<BuildDict>),
    Sharded(Arc<ShardedDict>),
    /// Two-pass bulk load (`intern_budget`): pass 1 spills raw byte quads
    /// and streams terms through budgeted spillers; no ids are minted.
    TwoPass(Arc<SpillerSet>),
}

/// Cooperative duty-cycle throttle for the build's hot loops (doc 07
/// §6.4): [`PaceGate::tick`] is called once per record and checks the
/// clock only every 4096 calls; after ~8 ms of work it sleeps long enough
/// that `work / (work + sleep) ≈ duty`. Each loop (including each Phase C
/// thread) carries its own gate, so the duty is per-thread.
pub(crate) struct PaceGate {
    duty: f64,
    since: std::time::Instant,
    n: u32,
}

impl PaceGate {
    pub fn new(duty: Option<f64>) -> Option<PaceGate> {
        let duty = duty?.clamp(f64::EPSILON, 1.0);
        (duty < 1.0).then(|| PaceGate {
            duty,
            since: std::time::Instant::now(),
            n: 0,
        })
    }

    #[inline]
    pub fn tick(&mut self) {
        self.n = self.n.wrapping_add(1);
        if self.n & 0xFFF != 0 {
            return;
        }
        let worked = self.since.elapsed();
        if worked >= std::time::Duration::from_millis(8) {
            std::thread::sleep(worked.mul_f64((1.0 - self.duty) / self.duty));
            self.since = std::time::Instant::now();
        }
    }
}

/// Tick an optional gate (hot-loop convenience).
#[inline]
pub(crate) fn pace(gate: &mut Option<PaceGate>) {
    if let Some(g) = gate.as_mut() {
        g.tick();
    }
}

/// Sentinel for the default graph in provisional spill records.
const DEFAULT_GRAPH: u64 = u64::MAX;

/// Validate a quad's term kinds (the builder is a public trust boundary).
pub(crate) fn validate_quad(s: &[u8], p: &[u8], g: Option<&[u8]>) -> Result<(), StoreError> {
    let bad = |m: String| StoreError::Corrupt(m);
    let node_kind = |bytes: &[u8], what: &str, allow_blank: bool| -> Result<(), StoreError> {
        match concise::decode(bytes).map_err(|e| bad(format!("{what}: {e}")))? {
            TermRef::Iri(_) => Ok(()),
            TermRef::BlankNode(_) if allow_blank => Ok(()),
            other => Err(bad(format!("{what}: invalid term kind {other:?}"))),
        }
    };
    node_kind(s, "subject", true)?;
    node_kind(p, "predicate", false)?;
    if let Some(g) = g {
        node_kind(g, "graph", true)?;
    }
    Ok(())
}

/// Append one provisional quad record to a spill writer.
fn write_spill_record(
    w: &mut BufWriter<File>,
    path: &Path,
    refs: [u64; 4],
) -> Result<(), StoreError> {
    let mut rec = [0u8; 32];
    for (i, v) in refs.into_iter().enumerate() {
        rec[i * 8..i * 8 + 8].copy_from_slice(&v.to_le_bytes());
    }
    w.write_all(&rec).map_err(|e| StoreError::io(path, e))
}

/// Two-pass spill record: four length-prefixed concise byte strings
/// (`[u32 len][bytes]` × s, p, o, g); a zero graph length means the
/// default graph (concise terms are never empty).
fn write_byte_quad(
    w: &mut BufWriter<File>,
    path: &Path,
    s: &[u8],
    p: &[u8],
    o: &[u8],
    g: Option<&[u8]>,
) -> Result<(), StoreError> {
    let err = |e: std::io::Error| StoreError::io(path, e);
    for t in [s, p, o] {
        w.write_all(&(t.len() as u32).to_le_bytes()).map_err(err)?;
        w.write_all(t).map_err(err)?;
    }
    match g {
        Some(g) => {
            w.write_all(&(g.len() as u32).to_le_bytes()).map_err(err)?;
            w.write_all(g).map_err(err)?;
        }
        None => w.write_all(&0u32.to_le_bytes()).map_err(err)?,
    }
    Ok(())
}

/// Read one [`write_byte_quad`] record into reusable buffers; `bufs[3]`
/// empty = default graph.
fn read_byte_quad(
    r: &mut BufReader<File>,
    path: &Path,
    bufs: &mut [Vec<u8>; 4],
) -> Result<(), StoreError> {
    let err = |e: std::io::Error| StoreError::io(path, e);
    for buf in bufs.iter_mut() {
        let mut len = [0u8; 4];
        r.read_exact(&mut len).map_err(err)?;
        buf.resize(u32::from_le_bytes(len) as usize, 0);
        r.read_exact(buf).map_err(err)?;
    }
    Ok(())
}

/// Create the segment output/scratch directory skeleton (idempotent).
pub(crate) fn prepare_dirs(cfg: &BuilderConfig) -> Result<(), StoreError> {
    for sub in ["", "dict", "idx", "graphs", "stats"] {
        let d = cfg.dir.join(sub);
        std::fs::create_dir_all(&d).map_err(|e| StoreError::io(&d, e))?;
    }
    std::fs::create_dir_all(&cfg.scratch).map_err(|e| StoreError::io(&cfg.scratch, e))
}

/// A push-style producer of final-id `[s, p, o, g]` quads: called once
/// with a sink, it must feed every quad of the dataset (any order;
/// Phase B sorts and dedups).
pub type QuadSource<'a> =
    dyn FnMut(&mut dyn FnMut([u64; 4]) -> Result<(), StoreError>) -> Result<(), StoreError> + 'a;

/// Phases B–D from a finalized dictionary and a stream of final-id quads
/// (doc 07 §6.2: shared by the bulk loader — which maps provisional spill
/// records through its freshly built dictionary — and the streaming
/// dictionary merge, which rewrites the freeze snapshot's ids through its
/// remap tables so no term bytes flow at all). Writes every component and
/// the manifest.
pub(crate) fn build_from_ids(
    cfg: &BuilderConfig,
    sections: &Sections,
    tt_records: &[[u64; 3]],
    counts_dict: [u64; 5],
    source: &mut QuadSource<'_>,
) -> Result<Manifest, StoreError> {
    {
        prepare_dirs(cfg)?;
        let mut components = std::collections::BTreeMap::new();
        let mut sidecars = std::collections::BTreeMap::new();
        let component = |lg: (u64, u64)| Component {
            bytes: lg.0,
            xxh3: format!("{:016x}", lg.1),
        };
        let mut track = |name: &str, lg: (u64, u64)| {
            components.insert(name.to_owned(), component(lg));
        };
        for (name, pfc) in [
            ("shared", &sections.shared),
            ("subjects", &sections.subjects),
            ("predicates", &sections.predicates),
            ("objects", &sections.objects),
            ("graphs", &sections.graphs),
        ] {
            let rel = format!("dict/{name}.pfc");
            let lg = write_component(&cfg.dir.join(&rel), Kind::Dict, |w| pfc.serialize_into(w))?;
            track(&rel, lg);
            // Rebuildable term→ordinal sidecar (doc 02 RQ3: built eagerly
            // while the section is at hand).
            let rel = format!("dict/{name}.hash");
            let sc = HashSidecar::build(pfc);
            let lg = write_component(&cfg.dir.join(&rel), Kind::HashSidecar, |w| {
                sc.serialize_into(w)
            })?;
            sidecars.insert(rel, component(lg));
        }
        {
            let rel = "dict/triple_terms.bin";
            let lg = write_component(&cfg.dir.join(rel), Kind::TripleTerms, |w| {
                write_u64(w, tt_records.len() as u64)?;
                for r in tt_records {
                    write_u64s(w, r)?;
                }
                Ok(())
            })?;
            track(rel, lg);
        }

        // ---- Phase B: sort + dedup + primary ordering + graphs + charsets,
        // fed final-id quads by `source` (bulk load: provisional spills
        // mapped through the fresh dictionary; streaming merge: the freeze
        // snapshot rewritten through the remap tables).
        let budget = cfg.sort_budget.max(32 * 1024);
        let mut sorter: ExtSorter<[u64; 4]> =
            ExtSorter::new(&cfg.scratch, budget).map_err(|e| StoreError::io(&cfg.scratch, e))?;
        let mut maxima = [0u64; 4];
        source(&mut |q: [u64; 4]| {
            maxima = [
                maxima[0].max(q[0]),
                maxima[1].max(q[1]),
                maxima[2].max(q[2]),
                maxima[3].max(q[3]),
            ];
            sorter.push(q).map_err(|e| StoreError::io(&cfg.scratch, e))
        })?;
        let widths = [
            bits_for(maxima[0]),
            bits_for(maxima[1]),
            bits_for(maxima[2]),
            bits_for(maxima[3]),
        ];

        let mut pred_counts = vec![0u64; counts_dict[2] as usize];
        let mut spo = BtBuilder::new(false, widths[1], widths[2]);
        let triples_path = cfg.scratch.join("triples.run");
        let mut triples_out = BufWriter::new(
            File::create(&triples_path).map_err(|e| StoreError::io(&triples_path, e))?,
        );
        let mut graph_bitmaps: Vec<RoaringTreemap> = Vec::new();
        let mut tg_starts = BitVectorBuilder::new();
        // Packed at the graph-ordinal width (inc B: at 10⁸ quads this is
        // tens of MB instead of an 800 MB Vec<u64>).
        let mut tg_graphs = PackedIntsBuilder::new(bits_for(maxima[3]).max(1));
        let mut charsets = CharSets::default();
        let mut n_quads = 0u64;
        let mut n_triples = 0u64;
        let mut has_named_graphs = false;
        {
            let mut last_quad: Option<[u64; 4]> = None;
            let mut last_triple: Option<[u64; 3]> = None;
            let mut gate = PaceGate::new(cfg.pace_duty);
            for rec in sorter
                .finish()
                .map_err(|e| StoreError::io(&cfg.scratch, e))?
            {
                pace(&mut gate);
                let q = rec.map_err(|e| StoreError::io(&cfg.scratch, e))?;
                if last_quad == Some(q) {
                    continue; // datasets are sets
                }
                last_quad = Some(q);
                n_quads += 1;
                let t = [q[0], q[1], q[2]];
                let new_triple = last_triple != Some(t);
                if new_triple {
                    last_triple = Some(t);
                    pred_counts[t[1] as usize] += 1;
                    spo.push(t[0], t[1], t[2], None)
                        .map_err(StoreError::Corrupt)?;
                    let mut buf = [0u8; 24];
                    for (i, v) in t.iter().enumerate() {
                        buf[i * 8..i * 8 + 8].copy_from_slice(&v.to_le_bytes());
                    }
                    triples_out
                        .write_all(&buf)
                        .map_err(|e| StoreError::io(&triples_path, e))?;
                    charsets.observe(t[0], t[1]);
                    n_triples += 1;
                }
                let ordinal = n_triples - 1;
                let g = q[3];
                if g > 0 {
                    has_named_graphs = true;
                }
                while graph_bitmaps.len() <= g as usize {
                    graph_bitmaps.push(RoaringTreemap::new());
                }
                graph_bitmaps[g as usize].insert(ordinal);
                tg_starts.push(new_triple);
                tg_graphs.push(g);
            }
        }
        triples_out
            .flush()
            .map_err(|e| StoreError::io(&triples_path, e))?;
        charsets.flush_subject();
        let spo = spo.finish();
        {
            let rel = "idx/spo.bt";
            let lg = write_component(&cfg.dir.join(rel), Kind::BitmapTriples, |w| {
                spo.serialize_into(w)
            })?;
            track(rel, lg);
        }

        // FoQ accessors give the compact profile its P- and O-rooted access
        // (docs/08 §4); other profiles materialize orderings instead.
        if cfg.profile == Profile::Compact {
            let mut sorter: ExtSorter<[u64; 2]> = ExtSorter::new(&cfg.scratch, cfg.sort_budget)
                .map_err(|e| StoreError::io(&cfg.scratch, e))?;
            let mut gate = PaceGate::new(cfg.pace_duty);
            for (ordinal, t) in read_triples(&triples_path, n_triples)?.enumerate() {
                pace(&mut gate);
                let [_, _, o] = t?;
                sorter
                    .push([o, ordinal as u64])
                    .map_err(|e| StoreError::io(&cfg.scratch, e))?;
            }
            // Stream the sorted pairs straight into the build (the Wp input
            // is the one Foq piece that still buffers — n_sy words).
            let mut io_err: Option<io::Error> = None;
            let sorted = sorter
                .finish()
                .map_err(|e| StoreError::io(&cfg.scratch, e))?
                .map_while(|rec| match rec {
                    Ok([o, ordinal]) => Some((o, ordinal)),
                    Err(e) => {
                        io_err = Some(e);
                        None
                    }
                });
            let foq = crate::foq::Foq::build(&spo, sorted, widths[1]);
            if let Some(e) = io_err {
                return Err(StoreError::io(&cfg.scratch, e));
            }
            let rel = "idx/foq.wm";
            let lg = write_component(&cfg.dir.join(rel), Kind::Foq, |w| foq.serialize_into(w))?;
            track(rel, lg);
        }
        // The SPO Bt is serialized and (for compact) consumed by FoQ — it
        // has no Phase C role, so release its ~n×width-bit sequences now.
        drop(spo);

        // Graph layer (skipped entirely for triples-only datasets).
        let has_graphs = has_named_graphs;
        if has_graphs {
            let rel = "graphs/at.roar";
            let lg = write_component(&cfg.dir.join(rel), Kind::GraphsAt, |w| {
                write_u64(w, graph_bitmaps.len() as u64)?;
                for bm in &graph_bitmaps {
                    write_u64(w, bm.serialized_size() as u64)?;
                    bm.serialize_into(&mut *w)
                        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
                    // v2 alignment rule: the next byte_len is a u64.
                    w.pad_to_8()?;
                }
                Ok(())
            })?;
            track(rel, lg);

            let rel = "graphs/tg.wm";
            let starts = tg_starts.build();
            let wm = WaveletMatrix::from_packed(&tg_graphs.build());
            let lg = write_component(&cfg.dir.join(rel), Kind::GraphsTg, |w| {
                starts.serialize_into(w)?;
                wm.serialize_into(w)
            })?;
            track(rel, lg);
        }

        // ---- Phase C: secondary orderings + predicate stats. The tasks are
        // independent read-only passes over the immutable canonical run, so
        // they execute concurrently (doc 07 §7: "orderings build in parallel
        // up to an io/cpu budget"), splitting the sort budget between them.
        let mut pred_stats: Vec<[u64; 3]> = pred_counts.iter().map(|&c| [c, 0, 0]).collect();
        let orderings: Vec<Order> = match &cfg.orderings {
            Some(list) => list.clone(),
            None => cfg.profile.orderings().to_vec(),
        };
        let secondary: Vec<Order> = orderings
            .iter()
            .copied()
            .filter(|&o| o != Order::Spo)
            .collect();
        let task_budget = (cfg.sort_budget / (secondary.len() + 1).max(1)).max(32 * 1024);
        let build_ordering = |order: Order| -> Result<(String, (u64, u64)), StoreError> {
            // Pz payload only where it earns its bytes: graph
            // structures are indexed by SPO ordinals (doc 02 §3.2).
            let bt = sort_ordering(
                cfg,
                &triples_path,
                n_triples,
                order,
                widths,
                has_graphs,
                task_budget,
            )?;
            let rel = format!("idx/{}.bt", order.name());
            let lg = write_component(&cfg.dir.join(&rel), Kind::BitmapTriples, |w| {
                bt.serialize_into(w)
            })?;
            Ok((rel, lg))
        };
        // wasm32 has no std threads: the same tasks run sequentially on
        // the calling thread (identical output — each task is an
        // independent pass over the immutable canonical run).
        let (ordering_results, distinct) = if cfg!(target_arch = "wasm32") {
            let results: Vec<_> = secondary
                .iter()
                .map(|&order| build_ordering(order))
                .collect();
            let distinct =
                pred_distinct(cfg, &triples_path, n_triples, pred_stats.len(), task_budget);
            (results, distinct)
        } else {
            std::thread::scope(|scope| {
                let build_ordering = &build_ordering;
                let handles: Vec<_> = secondary
                    .iter()
                    .map(|&order| scope.spawn(move || build_ordering(order)))
                    .collect();
                // Distinct S and O per predicate from dedicated dedup passes
                // (exact, profile-independent).
                let distinct =
                    pred_distinct(cfg, &triples_path, n_triples, pred_stats.len(), task_budget);
                let results: Vec<_> = handles
                    .into_iter()
                    .map(|h| h.join().expect("ordering task must not panic"))
                    .collect();
                (results, distinct)
            })
        };
        for r in ordering_results {
            let (rel, lg) = r?;
            track(&rel, lg);
        }
        for (i, [ds, dobj]) in distinct?.into_iter().enumerate() {
            pred_stats[i][1] = ds;
            pred_stats[i][2] = dobj;
        }
        {
            let rel = "stats/pred.stats";
            let lg = write_component(&cfg.dir.join(rel), Kind::PredStats, |w| {
                write_u64(w, pred_stats.len() as u64)?;
                for s in &pred_stats {
                    write_u64s(w, s)?;
                }
                Ok(())
            })?;
            track(rel, lg);
        }
        {
            let rel = "stats/charsets.bin";
            let lg = write_component(&cfg.dir.join(rel), Kind::CharSets, |w| {
                charsets.serialize_into(w)
            })?;
            track(rel, lg);
        }

        std::fs::remove_file(&triples_path).ok();
        std::fs::remove_dir(&cfg.scratch).ok();

        let manifest = Manifest {
            format_version: FORMAT_VERSION,
            generation: cfg.generation,
            profile: cfg.profile.name().to_owned(),
            orderings: orderings.iter().map(|o| o.name().to_owned()).collect(),
            has_graphs,
            counts: Counts {
                quads: n_quads,
                triples: n_triples,
                shared: counts_dict[0],
                subjects: counts_dict[1],
                predicates: counts_dict[2],
                objects: counts_dict[3],
                graphs: counts_dict[4],
                triple_terms: tt_records.len() as u64,
            },
            components,
            sidecars,
        };
        manifest.save(&cfg.dir)?;
        Ok(manifest)
    }
}

/// Build a segment from an externally prepared dictionary (doc 03: the
/// graphy-hdt fast import path — HDT files arrive with their terms
/// already partitioned into exactly our sections). Each section's terms
/// must be concise-encoded, **byte-sorted**, distinct, and role-consistent
/// with the quads (`shared` = terms used as both subject and object);
/// inline-encodable literals must have been extracted by the caller (they
/// never enter the dictionary — pass their inline `TermId` raw values in
/// the object column instead). `source` feeds final column values:
/// subject/object ids are `shared` positions then `subjects`/`objects`
/// positions (0-based), graphs use the column convention (0 = default).
/// Sortedness is validated (cheap, and this is a public trust boundary);
/// term validity is the caller's contract.
pub fn build_from_sorted_dict(
    cfg: &BuilderConfig,
    shared: &[Vec<u8>],
    subjects: &[Vec<u8>],
    predicates: &[Vec<u8>],
    objects: &[Vec<u8>],
    graphs: &[Vec<u8>],
    source: &mut QuadSource<'_>,
) -> Result<Manifest, StoreError> {
    let build_pfc = |name: &str, terms: &[Vec<u8>]| -> Result<Pfc, StoreError> {
        let mut b = PfcBuilder::new(32);
        let mut prev: Option<&[u8]> = None;
        for t in terms {
            if prev.is_some_and(|p| p >= t.as_slice()) {
                return Err(StoreError::Corrupt(format!(
                    "{name} section not strictly byte-sorted"
                )));
            }
            b.push(t);
            prev = Some(t);
        }
        Ok(b.build())
    };
    let sections = Sections {
        shared: build_pfc("shared", shared)?,
        subjects: build_pfc("subjects", subjects)?,
        predicates: build_pfc("predicates", predicates)?,
        objects: build_pfc("objects", objects)?,
        graphs: build_pfc("graphs", graphs)?,
    };
    let counts = [
        shared.len() as u64,
        subjects.len() as u64,
        predicates.len() as u64,
        objects.len() as u64,
        graphs.len() as u64,
    ];
    build_from_ids(cfg, &sections, &[], counts, source)
}

impl SegmentBuilder {
    pub fn new(cfg: BuilderConfig) -> Result<SegmentBuilder, StoreError> {
        prepare_dirs(&cfg)?;
        let intern = match cfg.intern_budget {
            Some(b) => Intern::TwoPass(Arc::new(SpillerSet::new(cfg.scratch.clone(), b))),
            None => Intern::Serial(Box::new(BuildDict::new())),
        };
        let spill_path = cfg.scratch.join("quads.prov");
        let spill =
            BufWriter::new(File::create(&spill_path).map_err(|e| StoreError::io(&spill_path, e))?);
        Ok(SegmentBuilder {
            cfg,
            intern,
            spill,
            spill_path,
            n_pushed: 0,
            joined: Vec::new(),
            next_lane: 0,
        })
    }

    /// Push one quad of concise-encoded terms (`g: None` = default graph).
    /// Terms are validated (this is a public trust boundary).
    pub fn push_quad(
        &mut self,
        s: &[u8],
        p: &[u8],
        o: &[u8],
        g: Option<&[u8]>,
    ) -> Result<(), StoreError> {
        validate_quad(s, p, g)?;
        let bad = |m: String| StoreError::Corrupt(m);
        let refs = match &mut self.intern {
            Intern::Serial(d) => [
                d.intern_node(s, R_SUBJ),
                d.intern_node(p, R_PRED),
                d.intern_object(o).map_err(bad)?,
                g.map_or(DEFAULT_GRAPH, |g| d.intern_node(g, R_GRAPH)),
            ],
            Intern::Sharded(d) => [
                d.intern_node(s, R_SUBJ),
                d.intern_node(p, R_PRED),
                d.intern_object(o).map_err(bad)?,
                g.map_or(DEFAULT_GRAPH, |g| d.intern_node(g, R_GRAPH)),
            ],
            Intern::TwoPass(d) => {
                d.add_node(s, R_SUBJ);
                d.add_node(p, R_PRED);
                d.add_object(o).map_err(bad)?;
                if let Some(g) = g {
                    d.add_node(g, R_GRAPH);
                }
                write_byte_quad(&mut self.spill, &self.spill_path, s, p, o, g)?;
                self.n_pushed += 1;
                return Ok(());
            }
        };
        write_spill_record(&mut self.spill, &self.spill_path, refs)?;
        self.n_pushed += 1;
        Ok(())
    }

    /// Switch to the sharded dictionary (doc 07 §7) and hand out `n`
    /// independent ingest lanes; each lane is `Send` and accepts
    /// [`IngestLane::push_quad`] from its own thread. [`Self::join`] every
    /// lane before [`Self::finish`]. Must be called before any serial
    /// `push_quad` (the two dictionaries cannot merge).
    pub fn lanes(&mut self, n: usize) -> Result<Vec<IngestLane>, StoreError> {
        let dict = match &self.intern {
            Intern::Serial(_) if self.n_pushed > 0 => {
                return Err(StoreError::Corrupt(
                    "lanes() must precede serial push_quad".into(),
                ));
            }
            Intern::Serial(_) => {
                let d = Arc::new(ShardedDict::new());
                self.intern = Intern::Sharded(Arc::clone(&d));
                LaneDict::Sharded(d)
            }
            Intern::Sharded(d) => LaneDict::Sharded(Arc::clone(d)),
            // The spiller set is already thread-safe; lanes share it.
            Intern::TwoPass(d) => LaneDict::TwoPass(Arc::clone(d)),
        };
        let mut lanes = Vec::with_capacity(n);
        for _ in 0..n {
            let path = self
                .cfg
                .scratch
                .join(format!("quads-{}.prov", self.next_lane));
            self.next_lane += 1;
            let spill = BufWriter::new(File::create(&path).map_err(|e| StoreError::io(&path, e))?);
            lanes.push(IngestLane {
                dict: dict.clone(),
                spill,
                path,
                n_pushed: 0,
            });
        }
        Ok(lanes)
    }

    /// Absorb a finished lane's spill into the build.
    pub fn join(&mut self, mut lane: IngestLane) -> Result<(), StoreError> {
        lane.spill
            .flush()
            .map_err(|e| StoreError::io(&lane.path, e))?;
        self.joined.push((lane.path, lane.n_pushed));
        Ok(())
    }

    /// Build every component and write the manifest. Returns the manifest.
    pub fn finish(mut self) -> Result<Manifest, StoreError> {
        self.spill
            .flush()
            .map_err(|e| StoreError::io(&self.spill_path, e))?;
        let cfg = self.cfg.clone();
        let spills: Vec<(PathBuf, u64)> = std::iter::once((self.spill_path.clone(), self.n_pushed))
            .chain(self.joined.drain(..))
            .collect();

        // ---- Phase A: dictionary.
        let intern =
            std::mem::replace(&mut self.intern, Intern::Serial(Box::new(BuildDict::new())));
        let dict = match intern {
            Intern::Serial(d) => d.finalize(),
            Intern::Sharded(d) => Arc::try_unwrap(d)
                .map_err(|_| "ingest lanes still alive at finish".to_owned())
                .and_then(ShardedDict::finalize),
            Intern::TwoPass(d) => return finish_two_pass(cfg, d, spills),
        }
        .map_err(StoreError::Corrupt)?;
        let counts_dict = [
            dict.sections.shared.len() as u64,
            dict.sections.subjects.len() as u64,
            dict.sections.predicates.len() as u64,
            dict.sections.objects.len() as u64,
            dict.sections.graphs.len() as u64,
        ];
        build_from_ids(
            &cfg,
            &dict.sections,
            &dict.tt_records,
            counts_dict,
            &mut |sink| {
                let mut gate = PaceGate::new(cfg.pace_duty);
                for (path, n) in &spills {
                    let f = File::open(path).map_err(|e| StoreError::io(path, e))?;
                    let mut r = BufReader::new(f);
                    let mut rec = [0u8; 32];
                    for _ in 0..*n {
                        pace(&mut gate);
                        r.read_exact(&mut rec)
                            .map_err(|e| StoreError::io(path, e))?;
                        let read = |i: usize| {
                            u64::from_le_bytes(rec[i * 8..i * 8 + 8].try_into().expect("8 bytes"))
                        };
                        let s = dict
                            .map(read(0), Pos::Subject)
                            .map_err(StoreError::Corrupt)?;
                        let p = dict
                            .map(read(1), Pos::Predicate)
                            .map_err(StoreError::Corrupt)?;
                        let o = dict
                            .map(read(2), Pos::Object)
                            .map_err(StoreError::Corrupt)?;
                        let g = match read(3) {
                            DEFAULT_GRAPH => 0,
                            prov => dict.map(prov, Pos::Graph).map_err(StoreError::Corrupt)? + 1,
                        };
                        sink([s, p, o, g])?;
                    }
                }
                for (path, _) in &spills {
                    std::fs::remove_file(path).ok();
                }
                Ok(())
            },
        )
    }
}

/// Materialize one extra ordering from a canonical triple run and write
/// its component (minor merge, doc 07 §6.4). Returns the component's
/// relative path and (bytes, xxh3) for the manifest update.
pub(crate) fn materialize_ordering(
    cfg: &BuilderConfig,
    triples_path: &Path,
    n_triples: u64,
    order: Order,
    widths: [u32; 4],
    with_pz: bool,
) -> Result<(String, (u64, u64)), StoreError> {
    let bt = sort_ordering(
        cfg,
        triples_path,
        n_triples,
        order,
        widths,
        with_pz,
        cfg.sort_budget,
    )?;
    let rel = format!("idx/{}.bt", order.name());
    let lg = write_component(&cfg.dir.join(&rel), Kind::BitmapTriples, |w| {
        bt.serialize_into(w)
    })?;
    Ok((rel, lg))
}

/// External-sort the canonical triple run into `order` and build its
/// BitmapTriples. `with_pz` carries each triple's SPO ordinal (= its
/// position in the canonical run) into the `Pz` payload.
fn sort_ordering(
    cfg: &BuilderConfig,
    triples_path: &Path,
    n_triples: u64,
    order: Order,
    widths: [u32; 4],
    with_pz: bool,
    budget: usize,
) -> Result<crate::bt::SpilledBt, StoreError> {
    {
        let mut sorter: ExtSorter<[u64; 4]> =
            ExtSorter::new(&cfg.scratch, budget).map_err(|e| StoreError::io(&cfg.scratch, e))?;
        let mut gate = PaceGate::new(cfg.pace_duty);
        for (ordinal, t) in read_triples(triples_path, n_triples)?.enumerate() {
            pace(&mut gate);
            let [s, p, o] = t?;
            let [x, y, z] = order.to_xyz(s, p, o);
            // Triples are distinct, so the ordinal never influences the sort.
            sorter
                .push([x, y, z, ordinal as u64])
                .map_err(|e| StoreError::io(&cfg.scratch, e))?;
        }
        // Column widths permuted to this ordering (index 3 unused).
        let [ws, wp, wo] = [widths[0], widths[1], widths[2]];
        let [_, wy, wz] = match order {
            Order::Spo => [0, wp, wo],
            Order::Sop => [0, wo, wp],
            Order::Pos => [0, wo, ws],
            Order::Pso => [0, ws, wo],
            Order::Osp => [0, ws, wp],
            Order::Ops => [0, wp, ws],
        };
        // Serialize-only build (inc C): the sequences spill filled words
        // to scratch, so a 10⁸-triple ordering holds ~8 MiB per sequence
        // instead of n × width bits.
        let scratch_err = |e| StoreError::io(&cfg.scratch, e);
        let mut bt =
            BtSpillBuilder::new_spilling(&cfg.scratch, order.name(), order.explicit_x(), wy, wz)
                .map_err(scratch_err)?;
        if with_pz {
            bt = bt
                .with_spo_payload(
                    &cfg.scratch,
                    order.name(),
                    bits_for(n_triples.saturating_sub(1)),
                )
                .map_err(scratch_err)?;
        }
        for rec in sorter
            .finish()
            .map_err(|e| StoreError::io(&cfg.scratch, e))?
        {
            pace(&mut gate);
            let t = rec.map_err(|e| StoreError::io(&cfg.scratch, e))?;
            bt.push(t[0], t[1], t[2], with_pz.then_some(t[3]))
                .map_err(StoreError::Corrupt)?;
        }
        bt.finish().map_err(scratch_err)
    }
}

/// Exact distinct-subject / distinct-object counts per predicate via
/// dedup passes over (p, s) and (p, o).
fn pred_distinct(
    cfg: &BuilderConfig,
    triples_path: &Path,
    n_triples: u64,
    n_preds: usize,
    budget: usize,
) -> Result<Vec<[u64; 2]>, StoreError> {
    {
        let mut out = vec![[0u64; 2]; n_preds];
        for (col, slot) in [(0usize, 0usize), (2, 1)] {
            let mut sorter: ExtSorter<[u64; 2]> = ExtSorter::new(&cfg.scratch, budget)
                .map_err(|e| StoreError::io(&cfg.scratch, e))?;
            let mut gate = PaceGate::new(cfg.pace_duty);
            for t in read_triples(triples_path, n_triples)? {
                pace(&mut gate);
                let t = t?;
                sorter
                    .push([t[1], t[col]])
                    .map_err(|e| StoreError::io(&cfg.scratch, e))?;
            }
            let mut last = None;
            for rec in sorter
                .finish()
                .map_err(|e| StoreError::io(&cfg.scratch, e))?
            {
                pace(&mut gate);
                let pair = rec.map_err(|e| StoreError::io(&cfg.scratch, e))?;
                if last != Some(pair) {
                    last = Some(pair);
                    out[pair[0] as usize][slot] += 1;
                }
            }
        }
        Ok(out)
    }
}

/// The two-pass `finish` (doc 07 §7 memory ceiling): merge the spilled
/// term runs into sections, resolve the triple-term table, then stream the
/// byte-quad spills through a sidecar-backed [`Resolver`] into
/// [`build_from_ids`]. Peak memory is the spiller windows (pass 1) then
/// the sections + sidecars (pass 2) — never an id-space-sized table.
fn finish_two_pass(
    cfg: BuilderConfig,
    set: Arc<SpillerSet>,
    spills: Vec<(PathBuf, u64)>,
) -> Result<Manifest, StoreError> {
    let set = Arc::try_unwrap(set)
        .map_err(|_| StoreError::Corrupt("ingest lanes still alive at finish".into()))?;
    let (sections, tts) = set.into_sections().map_err(StoreError::Corrupt)?;
    let counts = [
        sections.shared.len() as u64,
        sections.subjects.len() as u64,
        sections.predicates.len() as u64,
        sections.objects.len() as u64,
        sections.graphs.len() as u64,
    ];
    let mut resolver = Resolver::new(&sections);
    let tt_records = resolver.resolve_tts(tts).map_err(StoreError::Corrupt)?;
    build_from_ids(&cfg, &sections, &tt_records, counts, &mut |sink| {
        let mut gate = PaceGate::new(cfg.pace_duty);
        let mut bufs: [Vec<u8>; 4] = Default::default();
        for (path, n) in &spills {
            let f = File::open(path).map_err(|e| StoreError::io(path, e))?;
            let mut r = BufReader::new(f);
            for _ in 0..*n {
                pace(&mut gate);
                read_byte_quad(&mut r, path, &mut bufs)?;
                let s = resolver.subject(&bufs[0]).map_err(StoreError::Corrupt)?;
                let p = resolver.predicate(&bufs[1]).map_err(StoreError::Corrupt)?;
                let o = resolver.object(&bufs[2]).map_err(StoreError::Corrupt)?;
                let g = if bufs[3].is_empty() {
                    0
                } else {
                    resolver.graph(&bufs[3]).map_err(StoreError::Corrupt)? + 1
                };
                sink([s, p, o, g])?;
            }
        }
        for (path, _) in &spills {
            std::fs::remove_file(path).ok();
        }
        Ok(())
    })
}

/// One parallel ingestion lane (doc 07 §7): interns into the builder's
/// shared dictionary (or spiller set) and spills quads to its own file.
/// `Send` — drive each lane from its own thread, then hand it back via
/// [`SegmentBuilder::join`].
#[derive(Debug)]
pub struct IngestLane {
    dict: LaneDict,
    spill: BufWriter<File>,
    path: PathBuf,
    n_pushed: u64,
}

#[derive(Debug, Clone)]
enum LaneDict {
    Sharded(Arc<ShardedDict>),
    TwoPass(Arc<SpillerSet>),
}

impl IngestLane {
    /// Thread-safe counterpart of [`SegmentBuilder::push_quad`] (same
    /// validation trust boundary).
    pub fn push_quad(
        &mut self,
        s: &[u8],
        p: &[u8],
        o: &[u8],
        g: Option<&[u8]>,
    ) -> Result<(), StoreError> {
        validate_quad(s, p, g)?;
        let bad = StoreError::Corrupt;
        match &self.dict {
            LaneDict::Sharded(d) => {
                let refs = [
                    d.intern_node(s, R_SUBJ),
                    d.intern_node(p, R_PRED),
                    d.intern_object(o).map_err(bad)?,
                    g.map_or(DEFAULT_GRAPH, |g| d.intern_node(g, R_GRAPH)),
                ];
                write_spill_record(&mut self.spill, &self.path, refs)?;
            }
            LaneDict::TwoPass(d) => {
                d.add_node(s, R_SUBJ);
                d.add_node(p, R_PRED);
                d.add_object(o).map_err(bad)?;
                if let Some(g) = g {
                    d.add_node(g, R_GRAPH);
                }
                write_byte_quad(&mut self.spill, &self.path, s, p, o, g)?;
            }
        }
        self.n_pushed += 1;
        Ok(())
    }

    /// Quads pushed through this lane so far.
    pub fn pushed(&self) -> u64 {
        self.n_pushed
    }
}

/// Stream (s, p, o) records back from the canonical run.
fn read_triples(
    path: &Path,
    n: u64,
) -> Result<impl Iterator<Item = Result<[u64; 3], StoreError>>, StoreError> {
    let f = File::open(path).map_err(|e| StoreError::io(path, e))?;
    let mut r = BufReader::new(f);
    let path = path.to_owned();
    Ok((0..n).map(move |_| {
        let mut rec = [0u8; 24];
        r.read_exact(&mut rec)
            .map_err(|e| StoreError::io(&path, e))?;
        Ok([
            u64::from_le_bytes(rec[0..8].try_into().expect("8 bytes")),
            u64::from_le_bytes(rec[8..16].try_into().expect("8 bytes")),
            u64::from_le_bytes(rec[16..24].try_into().expect("8 bytes")),
        ])
    }))
}

/// Characteristic sets (docs 05 §6, 07 §6 Phase D, 08 §4): per-subject
/// predicate-set signatures with a capped table; overflow subjects aggregate
/// into per-predicate **tail marginals** (format v2) so cardinality
/// estimation keeps a usable signal past the cap instead of a single scalar.
#[derive(Debug, Default)]
struct CharSets {
    cur_subject: Option<u64>,
    cur_preds: Vec<u64>,
    table: HashMap<Vec<u64>, u64>,
    tail_subjects: u64,
    tail_preds: HashMap<u64, u64>,
}

const CHARSET_CAP: usize = 65_536;

impl CharSets {
    /// Observe (subject, predicate) pairs in SPO order.
    fn observe(&mut self, s: u64, p: u64) {
        if self.cur_subject != Some(s) {
            self.flush_subject();
            self.cur_subject = Some(s);
        }
        if self.cur_preds.last() != Some(&p) {
            self.cur_preds.push(p);
        }
    }

    fn flush_subject(&mut self) {
        if self.cur_subject.take().is_none() {
            return;
        }
        let preds = std::mem::take(&mut self.cur_preds);
        if let Some(n) = self.table.get_mut(&preds) {
            *n += 1;
        } else if self.table.len() < CHARSET_CAP {
            self.table.insert(preds, 1);
        } else {
            self.tail_subjects += 1;
            for p in preds {
                *self.tail_preds.entry(p).or_insert(0) += 1;
            }
        }
    }

    /// `[n u64][tail_subjects u64][n_tail_preds u64] n × ([count][n_preds]
    /// [preds…]) n_tail_preds × ([pred][count])`, table most frequent first
    /// (ties by predicate list) for cheap top-k reads, tail marginals sorted
    /// by predicate id.
    fn serialize_into<W: Write>(&self, w: &mut W) -> io::Result<()> {
        let mut entries: Vec<(&Vec<u64>, u64)> = self.table.iter().map(|(k, &v)| (k, v)).collect();
        entries.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(b.0)));
        let mut tail: Vec<(u64, u64)> = self.tail_preds.iter().map(|(&p, &c)| (p, c)).collect();
        tail.sort_unstable();
        write_u64(w, entries.len() as u64)?;
        write_u64(w, self.tail_subjects)?;
        write_u64(w, tail.len() as u64)?;
        for (preds, count) in entries {
            write_u64(w, count)?;
            write_u64(w, preds.len() as u64)?;
            write_u64s(w, preds)?;
        }
        for (pred, count) in tail {
            write_u64(w, pred)?;
            write_u64(w, count)?;
        }
        Ok(())
    }
}
