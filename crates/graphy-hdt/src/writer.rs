//! HDT file writer: collect a triple stream (concise terms), then emit a
//! standard FourSection + BitmapTriples(SPO) file. In-memory build —
//! O(dataset) — which matches HDT's own positioning as an exchange format
//! for datasets that fit a build machine; a streaming writer over
//! pre-sorted runs is future work if export at segment scale demands it.

use std::collections::{BTreeMap, BTreeSet};
use std::io::Write;
use std::path::Path;

use crate::codec::Out;
use crate::quads::GraphAnnex;
use crate::reader::{
    FMT_DICT_FOUR, FMT_DICT_FOUR_QUAD, FMT_TRIPLES_BITMAP, FMT_TRIPLES_BITMAP_QUAD, ORDER_SPO,
};
use crate::section::{Bitmap, LogSeq, PfcSection};
use crate::term::concise_to_hdt;
use crate::HdtError;

/// Accumulates triples, then [`HdtWriter::write_to`] emits the file.
#[derive(Debug, Default)]
pub struct HdtWriter {
    subjects: BTreeSet<Vec<u8>>,
    predicates: BTreeSet<Vec<u8>>,
    objects: BTreeSet<Vec<u8>>,
    /// HDTQ graph terms (HDT string form; empty = default graph).
    graphs: BTreeSet<Vec<u8>>,
    /// (s, p, o, graph) — graph is the HDT string, empty for default.
    triples: Vec<[Vec<u8>; 4]>,
    /// Any named-graph quad seen → emit HDTQ.
    quads: bool,
}

impl HdtWriter {
    pub fn new() -> HdtWriter {
        HdtWriter::default()
    }

    /// Add one triple of concise-encoded terms (default graph).
    /// Duplicates collapse at write time (HDT is a set).
    pub fn add_triple(&mut self, s: &[u8], p: &[u8], o: &[u8]) -> Result<(), HdtError> {
        self.add_quad(s, p, o, None)
    }

    /// Add one quad. Any named graph switches the output to HDTQ
    /// (qEndpoint dialect: a fifth graphs dictionary section + a
    /// per-graph triple-annotation annex; the default graph is the
    /// empty-string graph entry).
    pub fn add_quad(
        &mut self,
        s: &[u8],
        p: &[u8],
        o: &[u8],
        g: Option<&[u8]>,
    ) -> Result<(), HdtError> {
        let s = concise_to_hdt(s)?.into_bytes();
        let p = concise_to_hdt(p)?.into_bytes();
        let o = concise_to_hdt(o)?.into_bytes();
        let g = match g {
            None => Vec::new(),
            Some(g) => {
                self.quads = true;
                concise_to_hdt(g)?.into_bytes()
            }
        };
        self.subjects.insert(s.clone());
        self.predicates.insert(p.clone());
        self.objects.insert(o.clone());
        self.triples.push([s, p, o, g]);
        Ok(())
    }

    pub fn write_to_path(self, path: &Path) -> Result<(), HdtError> {
        let mut f = std::io::BufWriter::new(std::fs::File::create(path)?);
        self.write_to(&mut f)
    }

    pub fn write_to(mut self, w: &mut impl Write) -> Result<(), HdtError> {
        // ---- Dictionary: shared = subjects ∩ objects; ids per mapping=1.
        let shared: BTreeSet<Vec<u8>> =
            self.subjects.intersection(&self.objects).cloned().collect();
        let subj_only: Vec<&[u8]> = self
            .subjects
            .iter()
            .filter(|t| !shared.contains(*t))
            .map(|v| v.as_slice())
            .collect();
        let obj_only: Vec<&[u8]> = self
            .objects
            .iter()
            .filter(|t| !shared.contains(*t))
            .map(|v| v.as_slice())
            .collect();
        let n_sh = shared.len() as u64;
        let mut subj_id: BTreeMap<&[u8], u64> = BTreeMap::new();
        let mut obj_id: BTreeMap<&[u8], u64> = BTreeMap::new();
        let mut pred_id: BTreeMap<&[u8], u64> = BTreeMap::new();
        for (i, t) in shared.iter().enumerate() {
            subj_id.insert(t, i as u64 + 1);
            obj_id.insert(t, i as u64 + 1);
        }
        for (i, t) in subj_only.iter().enumerate() {
            subj_id.insert(t, n_sh + i as u64 + 1);
        }
        for (i, t) in obj_only.iter().enumerate() {
            obj_id.insert(t, n_sh + i as u64 + 1);
        }
        for (i, t) in self.predicates.iter().enumerate() {
            pred_id.insert(t, i as u64 + 1);
        }

        // ---- Graphs dictionary (HDTQ): named graphs plus the
        // empty-string default-graph entry when default-graph statements
        // exist alongside named ones.
        if self.quads {
            for [_, _, _, g] in &self.triples {
                self.graphs.insert(g.clone());
            }
        }
        let graph_layer: BTreeMap<&[u8], u64> = self
            .graphs
            .iter()
            .enumerate()
            .map(|(i, g)| (g.as_slice(), i as u64))
            .collect();

        // ---- Quads: ids, (s, p, o, g-layer) sort, dedup; then the
        // distinct-triple sequence + per-graph annotation layers.
        let mut ids: Vec<[u64; 4]> = self
            .triples
            .drain(..)
            .map(|[s, p, o, g]| {
                [
                    subj_id[s.as_slice()],
                    pred_id[p.as_slice()],
                    obj_id[o.as_slice()],
                    graph_layer.get(g.as_slice()).copied().unwrap_or(0),
                ]
            })
            .collect();
        ids.sort_unstable();
        ids.dedup();
        let mut layers: Vec<Vec<u64>> = vec![Vec::new(); self.graphs.len()];
        let mut triples_ids: Vec<[u64; 3]> = Vec::with_capacity(ids.len());
        for q in &ids {
            let t = [q[0], q[1], q[2]];
            if triples_ids.last() != Some(&t) {
                triples_ids.push(t);
            }
            if self.quads {
                layers[q[3] as usize].push(triples_ids.len() as u64 - 1);
            }
        }
        let ids = triples_ids;

        // ---- BitmapTriples arrays.
        let mut sy: Vec<u64> = Vec::new();
        let mut sz: Vec<u64> = Vec::new();
        let mut by: Vec<bool> = Vec::new();
        let mut bz: Vec<bool> = Vec::new();
        for (k, t) in ids.iter().enumerate() {
            let next = ids.get(k + 1);
            let new_pair = next.is_none_or(|n| n[0] != t[0] || n[1] != t[1]);
            let new_subject = next.is_none_or(|n| n[0] != t[0]);
            if k == 0 || {
                let prev = ids[k - 1];
                prev[0] != t[0] || prev[1] != t[1]
            } {
                sy.push(t[1]);
            }
            sz.push(t[2]);
            bz.push(new_pair);
            if new_pair {
                by.push(new_subject);
            }
        }
        debug_assert_eq!(by.len(), sy.len());

        // ---- Emit.
        let mut out = Out::new();
        out.control_info(1, "<http://purl.org/HDT/hdt#HDTv1>", "");

        // Header: minimal well-formed N-Triples metadata payload.
        let header = format!(
            "<file://exported> <http://www.w3.org/1999/02/22-rdf-syntax-ns#type> \
             <http://purl.org/HDT/hdt#Dataset> .\n\
             <file://exported> <http://rdfs.org/ns/void#triples> \
             \"{}\" .\n",
            ids.len()
        );
        out.control_info(2, "ntriples", &format!("length={};", header.len()));
        out.buf.extend_from_slice(header.as_bytes());

        // Dictionary.
        let size_strings: usize = shared.iter().map(|t| t.len()).sum::<usize>()
            + subj_only.iter().map(|t| t.len()).sum::<usize>()
            + obj_only.iter().map(|t| t.len()).sum::<usize>()
            + self.predicates.iter().map(|t| t.len()).sum::<usize>();
        out.control_info(
            3,
            if self.quads {
                FMT_DICT_FOUR_QUAD
            } else {
                FMT_DICT_FOUR
            },
            &format!("mapping=1;sizeStrings={size_strings};"),
        );
        let owned = |v: &[&[u8]]| -> Vec<Vec<u8>> { v.iter().map(|s| s.to_vec()).collect() };
        PfcSection::write(&mut out, &shared.iter().cloned().collect::<Vec<_>>());
        PfcSection::write(&mut out, &owned(&subj_only));
        PfcSection::write(
            &mut out,
            &self.predicates.iter().cloned().collect::<Vec<_>>(),
        );
        PfcSection::write(&mut out, &owned(&obj_only));
        if self.quads {
            PfcSection::write(&mut out, &self.graphs.iter().cloned().collect::<Vec<_>>());
        }

        // Triples (+ the HDTQ graph annex).
        out.control_info(
            4,
            if self.quads {
                FMT_TRIPLES_BITMAP_QUAD
            } else {
                FMT_TRIPLES_BITMAP
            },
            &format!("order={ORDER_SPO};"),
        );
        Bitmap::write(&mut out, &by);
        Bitmap::write(&mut out, &bz);
        LogSeq::write(&mut out, &sy);
        LogSeq::write(&mut out, &sz);
        if self.quads {
            GraphAnnex::write(&mut out, &layers, ids.len() as u64);
        }

        w.write_all(&out.buf)?;
        Ok(())
    }
}
