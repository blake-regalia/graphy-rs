//! Fast HDT import (doc 03): feed a segment build directly from an HDT
//! file's structure — no re-interning, no text parsing. HDT terms arrive
//! already partitioned into exactly our dictionary sections (shared =
//! subjects ∩ objects, plus subject-only, predicate, object-only), so the
//! import only has to (a) convert each section to concise terms, (b)
//! re-sort from HDT string order into concise byte order, (c) extract the
//! inline-encodable typed literals HDT keeps in its dictionary but our
//! format inlines into `TermId`s, and (d) rewrite the id triples through
//! the resulting permutations into `graphy_store::build_from_sorted_dict`.
//!
//! Output is **byte-identical** to loading the same triples through the
//! parser/intern path — the builders are deterministic and the section
//! partition matches what `BuildDict` would compute (tested).

use graphy_core::{concise, TermId, TermRef};
use graphy_store::{BuilderConfig, Manifest, StoreError};

use crate::reader::HdtReader;
use crate::HdtError;

/// Object-column mapping for one HDT object id.
#[derive(Debug, Clone, Copy)]
enum ObjMap {
    /// Dictionary column value (shared or objects position).
    Col(u64),
    /// Inline `TermId` raw value (typed literal extracted from the dict).
    Inline(u64),
}

/// Build a segment at `cfg.dir` from `reader`'s dataset. Returns the
/// manifest (same contract as `SegmentBuilder::finish`).
pub fn import_segment(reader: &HdtReader, cfg: &BuilderConfig) -> Result<Manifest, HdtError> {
    let (shared, subjects, predicates, objects) = reader.dictionary_concise()?;
    let n_sh = shared.len() as u64;

    // HDTQ graphs: the empty entry is our default-graph spelling — it
    // never enters the dictionary (the default graph has no term). Named
    // entries re-sort into concise order like every section; the remap is
    // indexed by the reader's column values (HDT graph index + 1).
    let hdt_graphs = reader.graphs_concise()?;
    let mut graph_terms: Vec<(Vec<u8>, usize)> = hdt_graphs
        .iter()
        .enumerate()
        .filter(|(_, t)| !t.is_empty())
        .map(|(i, t)| (t.clone(), i))
        .collect();
    graph_terms.sort_unstable();
    let mut g_remap = vec![0u64; hdt_graphs.len() + 1]; // col 0 stays 0
    let graphs_sorted: Vec<Vec<u8>> = graph_terms
        .iter()
        .enumerate()
        .map(|(pos, (t, hdt_i))| {
            g_remap[hdt_i + 1] = pos as u64 + 1;
            t.clone()
        })
        .collect();

    // Concise-order permutation of a section: sorted terms + old→position.
    let sort_section = |terms: Vec<Vec<u8>>| -> (Vec<Vec<u8>>, Vec<u64>) {
        let mut order: Vec<u32> = (0..terms.len() as u32).collect();
        order.sort_unstable_by(|&a, &b| terms[a as usize].cmp(&terms[b as usize]));
        let mut remap = vec![0u64; terms.len()];
        for (pos, &old) in order.iter().enumerate() {
            remap[old as usize] = pos as u64;
        }
        let mut sorted: Vec<Vec<u8>> = Vec::with_capacity(terms.len());
        let mut terms = terms.into_iter().map(Some).collect::<Vec<_>>();
        for &old in &order {
            sorted.push(terms[old as usize].take().expect("each old index once"));
        }
        (sorted, remap)
    };

    // Objects: extract inline-encodable typed literals first (they never
    // enter the dictionary; their ids map to inline TermIds).
    let mut obj_dict: Vec<Vec<u8>> = Vec::with_capacity(objects.len());
    let mut obj_map: Vec<ObjMap> = Vec::with_capacity(objects.len());
    let mut dict_positions: Vec<usize> = Vec::new(); // index into obj_map
    for term in objects {
        match inline_id(&term)? {
            Some(raw) => obj_map.push(ObjMap::Inline(raw)),
            None => {
                dict_positions.push(obj_map.len());
                obj_map.push(ObjMap::Col(0)); // filled below
                obj_dict.push(term);
            }
        }
    }

    let (shared_sorted, shared_remap) = sort_section(shared);
    let (subj_sorted, subj_remap) = sort_section(subjects);
    let (pred_sorted, pred_remap) = sort_section(predicates);
    let (obj_sorted, obj_remap) = sort_section(obj_dict);
    for (dense, &at) in dict_positions.iter().enumerate() {
        obj_map[at] = ObjMap::Col(n_sh + obj_remap[dense]);
    }

    let map_s = |id: u64| -> u64 {
        if id <= n_sh {
            shared_remap[(id - 1) as usize]
        } else {
            n_sh + subj_remap[(id - n_sh - 1) as usize]
        }
    };
    let map_o = |id: u64| -> Result<u64, StoreError> {
        if id <= n_sh {
            return Ok(shared_remap[(id - 1) as usize]);
        }
        match obj_map.get((id - n_sh - 1) as usize) {
            Some(ObjMap::Col(v)) => Ok(*v),
            Some(ObjMap::Inline(raw)) => Ok(*raw),
            None => Err(StoreError::Corrupt(format!("object id {id} out of range"))),
        }
    };

    let mut io: Option<HdtError> = None;
    let manifest = graphy_store::build_from_sorted_dict(
        cfg,
        &shared_sorted,
        &subj_sorted,
        &pred_sorted,
        &obj_sorted,
        &graphs_sorted,
        &mut |sink| {
            let r = reader.each_quad_ids(|s, p, o, g| {
                let q = [
                    map_s(s),
                    pred_remap[(p - 1) as usize],
                    map_o(o).map_err(|e| HdtError::Format(e.to_string()))?,
                    g_remap[g as usize],
                ];
                sink(q).map_err(|e| HdtError::Format(e.to_string()))
            });
            match r {
                Ok(()) => Ok(()),
                Err(e) => {
                    let msg = e.to_string();
                    io = Some(e);
                    Err(StoreError::Corrupt(msg))
                }
            }
        },
    )
    .map_err(|e| match io.take() {
        Some(orig) => orig,
        None => HdtError::Format(e.to_string()),
    })?;
    Ok(manifest)
}

/// The inline gate, identical to `BuildDict::intern_object`'s: plain
/// (no-language) literals whose canonical form inlines become `TermId`s.
fn inline_id(term: &[u8]) -> Result<Option<u64>, HdtError> {
    match concise::decode(term).map_err(|e| HdtError::Format(e.to_string()))? {
        TermRef::Literal(l) if l.lang().is_none() => {
            Ok(TermId::try_inline(l.lexical(), l.datatype()).map(|id| id.raw()))
        }
        _ => Ok(None),
    }
}
