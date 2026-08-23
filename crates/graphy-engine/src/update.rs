//! SPARQL Update execution (M7 inc.3, doc 05 + doc 07 §5): translated
//! operations run through the M4 write pipeline — one atomic
//! [`Store::apply`] commit per operation, deletes before inserts (§3.1.3),
//! later operations seeing earlier effects.
//!
//! Graph-existence semantics: this store is a quad set — a named graph
//! exists iff it holds at least one quad (explicitly permitted by SPARQL
//! Update §2.2.3). `CLEAR`/`DROP`/`ADD`/`MOVE`/`COPY` on a nonexistent
//! (empty) source error unless `SILENT`; `CREATE` is always a no-op.
//! `LOAD` uses an injected document loader so the engine remains
//! transport-independent.
//!
//! **Memory discipline** (the 2026-08-07 field problem: a ~300k-triple
//! model commit peaked ~17 KB/triple, past the wasm32 4 GiB ceiling):
//! instantiation works in shared-term space. WHERE rows stay in binding
//! space; each distinct binding decodes to concise bytes **once**
//! ([`Decoder`]), template constants pre-wrap once ([`Pc`]), and
//! instantiated quads land in an online dedup set — so a template whose
//! quads are row-constant (layer1's diff metadata: ~18 quads × 300k rows)
//! costs its *distinct* quads, not rows × quads owned copies. `INSERT
//! DATA`/`DELETE DATA` without blank nodes borrow the translated quads
//! outright — zero per-term copies on the bulk-ingest path.

use std::collections::{BTreeSet, HashMap};
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};

use graphy_algebra::{GraphTargetT, QuadPat, TranslatedUpdate, UpdateOpT, VarTable, P};
use graphy_store::{Pattern, QuadBatch, QuadTerms, Snapshot, Store, TermPos};

use crate::eval::{DatasetView, EngineError, Evaluator, Row, B};
use crate::exec::evaluate_rows_vec as evaluate_rows;
use crate::fresh::session;

/// Shared concise term bytes: template constants, memoized binding
/// decodes, and scan-collected terms all alias one allocation.
type Term = Rc<[u8]>;

/// One quad in shared-term shape (`Store::apply` reads borrowed slices).
type SharedQuad = (Term, Term, Term, Option<Term>);

/// One owned quad (blank-node freshening / `LOAD`).
type OwnedQuad = (Vec<u8>, Vec<u8>, Vec<u8>, Option<Vec<u8>>);

/// One RDF graph triple returned by a `LOAD` document retriever.
pub type LoadedTriple = (Vec<u8>, Vec<u8>, Vec<u8>);

/// Process-wide ordinal for blank labels minted by updates. The random
/// session nonce keeps ordinals distinct from labels persisted by earlier
/// processes; this counter preserves uniqueness within the process.
static FRESH: AtomicU64 = AtomicU64::new(0);

fn fresh_label() -> Vec<u8> {
    let n = FRESH.fetch_add(1, Ordering::Relaxed);
    format!("_u{:032x}c{n}", session()).into_bytes()
}

/// Raise the fresh-label ordinal floor. Kept for hosts that use monotonic
/// ordinals for diagnostics or deterministic tests; durable correctness no
/// longer depends on calling this because every process has a random nonce.
pub fn decorrelate_fresh_labels(floor: u64) {
    FRESH.fetch_max(floor, Ordering::Relaxed);
}

/// Execute a translated update request against `store`, one atomic commit
/// per operation.
pub fn execute_update(store: &Store, u: &TranslatedUpdate) -> Result<(), EngineError> {
    execute_update_with_loader(store, u, &mut |source| {
        Err(EngineError(format!(
            "LOAD {} requires a document loader",
            String::from_utf8_lossy(source)
        )))
    })
}

/// Execute an update with a transport-supplied RDF graph loader.
pub fn execute_update_with_loader(
    store: &Store,
    u: &TranslatedUpdate,
    loader: &mut impl FnMut(&[u8]) -> Result<Vec<LoadedTriple>, EngineError>,
) -> Result<(), EngineError> {
    for op in &u.ops {
        execute_op(store, op, loader)?;
    }
    Ok(())
}

fn commit(
    store: &Store,
    dels: &[QuadTerms<'_>],
    adds: &[QuadTerms<'_>],
) -> Result<(), EngineError> {
    store
        .apply(dels, adds)
        .map(|_| ())
        .map_err(EngineError::from)
}

fn shared_refs(v: &[SharedQuad]) -> Vec<QuadTerms<'_>> {
    v.iter()
        .map(|q| (&*q.0, &*q.1, &*q.2, q.3.as_deref()))
        .collect()
}

fn apply_shared(
    store: &Store,
    dels: &[SharedQuad],
    adds: &[SharedQuad],
) -> Result<(), EngineError> {
    commit(store, &shared_refs(dels), &shared_refs(adds))
}

fn apply_owned(store: &Store, dels: &[OwnedQuad], adds: &[OwnedQuad]) -> Result<(), EngineError> {
    fn refs(v: &[OwnedQuad]) -> Vec<QuadTerms<'_>> {
        v.iter()
            .map(|q| {
                (
                    q.0.as_slice(),
                    q.1.as_slice(),
                    q.2.as_slice(),
                    q.3.as_deref(),
                )
            })
            .collect()
    }
    commit(store, &refs(dels), &refs(adds))
}

/// Blank node anywhere in a concise term — directly, or nested inside a
/// triple term (predicates cannot be blank by grammar).
fn has_bnode(bytes: &[u8]) -> bool {
    if bytes.starts_with(b"_") {
        return true;
    }
    match graphy_core::concise::decode(bytes) {
        Ok(graphy_core::TermRef::TripleTerm(t)) => {
            term_ref_has_bnode(&t.subject()) || term_ref_has_bnode(&t.object())
        }
        _ => false,
    }
}

fn term_ref_has_bnode(t: &graphy_core::TermRef<'_>) -> bool {
    match t {
        graphy_core::TermRef::BlankNode(_) => true,
        graphy_core::TermRef::TripleTerm(t) => {
            term_ref_has_bnode(&t.subject()) || term_ref_has_bnode(&t.object())
        }
        _ => false,
    }
}

/// Rewrite blank nodes, including those nested inside triple terms, to
/// operation-fresh labels.
fn freshen(bytes: &[u8], map: &mut HashMap<Vec<u8>, Vec<u8>>) -> Vec<u8> {
    if bytes.starts_with(b"_") {
        return map
            .entry(bytes.to_vec())
            .or_insert_with(fresh_label)
            .clone();
    }
    let Ok(graphy_core::TermRef::TripleTerm(t)) = graphy_core::concise::decode(bytes) else {
        return bytes.to_vec();
    };
    fn encode(term: graphy_core::TermRef<'_>) -> Vec<u8> {
        let mut out = Vec::new();
        match term {
            graphy_core::TermRef::Iri(i) => graphy_core::concise::encode_iri(&mut out, i),
            graphy_core::TermRef::BlankNode(b) => graphy_core::concise::encode_blank(&mut out, b),
            graphy_core::TermRef::Literal(l) => {
                if let Some((tag, dir)) = l.lang() {
                    graphy_core::concise::encode_lang(&mut out, l.lexical(), tag, dir);
                } else if l.datatype() == graphy_core::vocab::XSD_STRING {
                    graphy_core::concise::encode_simple(&mut out, l.lexical());
                } else {
                    graphy_core::concise::encode_datatype(&mut out, l.lexical(), l.datatype());
                }
            }
            graphy_core::TermRef::TripleTerm(t) => {
                let s = encode(t.subject());
                let p = encode(t.predicate());
                let o = encode(t.object());
                graphy_core::concise::encode_triple_term(&mut out, &s, &p, &o);
            }
        }
        out
    }
    let s = freshen(&encode(t.subject()), map);
    let p = encode(t.predicate());
    let o = freshen(&encode(t.object()), map);
    let mut out = Vec::new();
    graphy_core::concise::encode_triple_term(&mut out, &s, &p, &o);
    out
}

/// The named-graph column of a concise IRI, if that graph is nonempty.
fn graph_col(snap: &Snapshot, iri: &[u8]) -> Option<u64> {
    snap.resolve(iri, TermPos::Graph)
        .and_then(|id| snap.column(id, TermPos::Graph))
        .filter(|&c| c > 0)
}

/// Every quad in one graph scope (`None` = default graph) as shared terms.
fn graph_quads(snap: &Snapshot, graph: Option<u64>) -> Result<Vec<SharedQuad>, EngineError> {
    let pat = Pattern {
        g: Some(graph.unwrap_or(0)),
        ..Pattern::default()
    };
    collect(snap, &pat, |_| true)
}

/// Decode one column value through a per-position memo: scan columns
/// repeat heavily (predicates, graphs, multi-property subjects), so each
/// distinct value materializes once and every quad shares it.
fn decode_memo(
    memo: &mut HashMap<u64, Term>,
    snap: &Snapshot,
    v: u64,
    pos: TermPos,
) -> Result<Term, EngineError> {
    if let Some(t) = memo.get(&v) {
        return Ok(Rc::clone(t));
    }
    let t: Term = snap.decode_value(v, pos)?.into();
    memo.insert(v, Rc::clone(&t));
    Ok(t)
}

fn collect(
    snap: &Snapshot,
    pat: &Pattern,
    keep: impl Fn(u64) -> bool,
) -> Result<Vec<SharedQuad>, EngineError> {
    let mut scan = snap.scan_best(pat)?;
    let mut batch = QuadBatch::new();
    let mut out = Vec::new();
    let mut memo_s = HashMap::new();
    let mut memo_p = HashMap::new();
    let mut memo_o = HashMap::new();
    let mut memo_g = HashMap::new();
    while scan.next_batch(&mut batch)? {
        for i in 0..batch.len() {
            if !keep(batch.g[i]) {
                continue;
            }
            out.push((
                decode_memo(&mut memo_s, snap, batch.s[i], TermPos::Subject)?,
                decode_memo(&mut memo_p, snap, batch.p[i], TermPos::Predicate)?,
                decode_memo(&mut memo_o, snap, batch.o[i], TermPos::Object)?,
                match batch.g[i] {
                    0 => None,
                    g => Some(decode_memo(&mut memo_g, snap, g, TermPos::Graph)?),
                },
            ))
        }
    }
    Ok(out)
}

/// Decode bindings to shared bytes, memoized per binding — a value bound
/// in many rows (BIND-computed constants, graph names) materializes once.
struct Decoder<'e> {
    ev: Evaluator<'e>,
    cache: HashMap<B, Term>,
    /// Evaluator-local blank identity to update/store identity. This is
    /// shared by DELETE and INSERT templates for one operation.
    fresh: HashMap<Vec<u8>, Vec<u8>>,
}

impl Decoder<'_> {
    fn bytes(&mut self, b: B) -> Result<Term, EngineError> {
        if let Some(t) = self.cache.get(&b) {
            return Ok(Rc::clone(t));
        }
        let bytes = self.ev.bytes_of(b)?;
        let bytes = if matches!(b, B::Ext(_)) && has_bnode(&bytes) {
            freshen(&bytes, &mut self.fresh)
        } else {
            bytes
        };
        let t: Term = bytes.into();
        self.cache.insert(b, Rc::clone(&t));
        Ok(t)
    }
}

/// A template component with constants pre-wrapped as shared terms, so
/// per-row instantiation clones pointers, not bytes.
enum Pc {
    Term(Term),
    Var(graphy_algebra::VarId),
    Triple(Box<[Pc; 3]>),
}

fn prepare(p: &P) -> Pc {
    match p {
        P::Term(bytes) => Pc::Term(Term::from(bytes.as_slice())),
        P::Var(v) => Pc::Var(*v),
        P::Triple(t) => Pc::Triple(Box::new([prepare(&t.s), prepare(&t.p), prepare(&t.o)])),
    }
}

/// A prepared template quad ([`QuadPat`] with constants pre-shared).
struct PQuad {
    g: Option<Pc>,
    s: Pc,
    p: Pc,
    o: Pc,
}

fn prepare_quads(quads: &[QuadPat]) -> Vec<PQuad> {
    quads
        .iter()
        .map(|q| PQuad {
            g: q.g.as_ref().map(prepare),
            s: prepare(&q.s),
            p: prepare(&q.p),
            o: prepare(&q.o),
        })
        .collect()
}

/// Instantiate one template component under a row; `None` = unbound or
/// non-instantiable (the solution contributes nothing for this quad).
fn inst(
    pc: &Pc,
    row: &Row,
    row_bnodes: &mut HashMap<u32, Term>,
    vars: &VarTable,
    dec: &mut Decoder<'_>,
) -> Result<Option<Term>, EngineError> {
    Ok(match pc {
        Pc::Term(t) => Some(Rc::clone(t)),
        Pc::Var(v) => {
            // Template blank nodes (`.b:` interned vars) mint fresh nodes
            // per solution; real variables take their binding.
            let name = vars.name(*v);
            if name.starts_with('.') {
                return Ok(Some(Rc::clone(
                    row_bnodes
                        .entry(v.0)
                        .or_insert_with(|| Term::from(fresh_label())),
                )));
            }
            match row.get(v.0 as usize).copied().flatten() {
                Some(b) => Some(dec.bytes(b)?),
                None => None,
            }
        }
        Pc::Triple(t) => {
            let (Some(s), Some(p), Some(o)) = (
                inst(&t[0], row, row_bnodes, vars, dec)?,
                inst(&t[1], row, row_bnodes, vars, dec)?,
                inst(&t[2], row, row_bnodes, vars, dec)?,
            ) else {
                return Ok(None);
            };
            if !matches!(
                graphy_core::concise::decode(&s),
                Ok(graphy_core::TermRef::Iri(_) | graphy_core::TermRef::BlankNode(_))
            ) || !matches!(
                graphy_core::concise::decode(&p),
                Ok(graphy_core::TermRef::Iri(_))
            ) {
                return Ok(None);
            }
            let mut out = Vec::new();
            graphy_core::concise::encode_triple_term(&mut out, &s, &p, &o);
            Some(out.into())
        }
    })
}

/// Instantiate template quads for every row, deduplicating online (set
/// semantics downstream make this exact): the output is bounded by
/// *distinct* quads, so row-constant template quads cost one entry, not
/// one owned copy per row. `default_graph` is the operation's target when
/// a quad has no GRAPH (WITH or the default).
fn instantiate(
    quads: &[PQuad],
    rows: &[Row],
    vars: &VarTable,
    default_graph: Option<&Term>,
    dec: &mut Decoder<'_>,
) -> Result<Vec<SharedQuad>, EngineError> {
    let mut set: BTreeSet<SharedQuad> = BTreeSet::new();
    for row in rows {
        let mut row_bnodes: HashMap<u32, Term> = HashMap::new();
        for q in quads {
            let g = match &q.g {
                None => default_graph.map(Rc::clone),
                Some(pc) => {
                    let Some(b) = inst(pc, row, &mut row_bnodes, vars, dec)? else {
                        continue;
                    };
                    // Graph names must be IRIs (or blank nodes).
                    if !(b.starts_with(b">") || b.starts_with(b"_")) {
                        continue;
                    }
                    Some(b)
                }
            };
            let (Some(s), Some(p), Some(o)) = (
                inst(&q.s, row, &mut row_bnodes, vars, dec)?,
                inst(&q.p, row, &mut row_bnodes, vars, dec)?,
                inst(&q.o, row, &mut row_bnodes, vars, dec)?,
            ) else {
                continue;
            };
            // Validity: subject IRI/blank, predicate IRI, no literal graphs.
            if !(s.starts_with(b">") || s.starts_with(b"_")) || !p.starts_with(b">") {
                continue;
            }
            set.insert((s, p, o, g));
        }
    }
    Ok(set.into_iter().collect())
}

fn execute_op(
    store: &Store,
    op: &UpdateOpT,
    loader: &mut impl FnMut(&[u8]) -> Result<Vec<LoadedTriple>, EngineError>,
) -> Result<(), EngineError> {
    let snap = store.snapshot();
    let missing = |what: &str, silent: bool| -> Result<(), EngineError> {
        if silent {
            Ok(())
        } else {
            Err(EngineError(format!("graph {what} does not exist")))
        }
    };
    match op {
        UpdateOpT::InsertData(quads) => {
            // Bulk-ingest fast path: without blank nodes there is nothing
            // to freshen — apply borrows the translated quads directly.
            if !quads
                .iter()
                .any(|(_, s, _, o)| has_bnode(s) || has_bnode(o))
            {
                let adds: Vec<QuadTerms<'_>> = quads
                    .iter()
                    .map(|(g, s, p, o)| (s.as_slice(), p.as_slice(), o.as_slice(), g.as_deref()))
                    .collect();
                return commit(store, &[], &adds);
            }
            let mut labels = HashMap::new();
            let adds: Vec<OwnedQuad> = quads
                .iter()
                .map(|(g, s, p, o)| {
                    (
                        freshen(s, &mut labels),
                        p.clone(),
                        freshen(o, &mut labels),
                        g.clone(),
                    )
                })
                .collect();
            apply_owned(store, &[], &adds)
        }
        UpdateOpT::DeleteData(quads) => {
            // DELETE DATA forbids blank nodes by grammar: borrow directly.
            let dels: Vec<QuadTerms<'_>> = quads
                .iter()
                .map(|(g, s, p, o)| (s.as_slice(), p.as_slice(), o.as_slice(), g.as_deref()))
                .collect();
            commit(store, &dels, &[])
        }
        UpdateOpT::DeleteWhere { vars, quads } => {
            let root = quads_algebra(quads);
            let (rows, ev) = evaluate_rows(&snap, vars, &root, DatasetView::default(), None)?;
            let mut dec = Decoder {
                ev,
                cache: HashMap::new(),
                fresh: HashMap::new(),
            };
            let dels = instantiate(&prepare_quads(quads), &rows, vars, None, &mut dec)?;
            apply_shared(store, &dels, &[])
        }
        UpdateOpT::Modify {
            vars,
            with,
            delete,
            insert,
            using,
            pattern,
        } => {
            // Dataset (§3.1.3): USING overrides WITH for the WHERE.
            let (dataset, root_scope) = if using.is_empty() {
                let scope = with.as_ref().map(|iri| {
                    // A missing WITH graph = an empty default graph.
                    graph_col(&snap, iri).unwrap_or(u64::MAX)
                });
                (DatasetView::default(), scope)
            } else {
                let mut default_union = Vec::new();
                let mut named = Vec::new();
                for (default, iri) in using {
                    let Some(col) = graph_col(&snap, iri) else {
                        continue; // missing graphs contribute nothing
                    };
                    if *default {
                        default_union.push(col);
                    } else {
                        named.push(col);
                    }
                }
                (
                    DatasetView {
                        default_union: Some(default_union),
                        named: Some(named),
                    },
                    None,
                )
            };
            let root = graphy_algebra::rewrite(pattern.clone());
            let (rows, ev) = evaluate_rows(&snap, vars, &root, dataset, root_scope)?;
            let mut dec = Decoder {
                ev,
                cache: HashMap::new(),
                fresh: HashMap::new(),
            };
            // Templates without GRAPH target WITH (both delete and insert).
            let with_term: Option<Term> = with.as_ref().map(|iri| Term::from(iri.as_slice()));
            let dels = instantiate(
                &prepare_quads(delete),
                &rows,
                vars,
                with_term.as_ref(),
                &mut dec,
            )?;
            let adds = instantiate(
                &prepare_quads(insert),
                &rows,
                vars,
                with_term.as_ref(),
                &mut dec,
            )?;
            apply_shared(store, &dels, &adds)
        }
        UpdateOpT::Load {
            silent,
            source,
            into,
        } => {
            let triples = match loader(source) {
                Ok(triples) => triples,
                Err(_) if *silent => return Ok(()),
                Err(e) => return Err(e),
            };
            let mut labels = HashMap::new();
            let adds: Vec<OwnedQuad> = triples
                .into_iter()
                .map(|(s, p, o)| {
                    (
                        freshen(&s, &mut labels),
                        p,
                        freshen(&o, &mut labels),
                        into.clone(),
                    )
                })
                .collect();
            apply_owned(store, &[], &adds)
        }
        UpdateOpT::Clear { silent, target } | UpdateOpT::Drop { silent, target } => {
            let dels = match target {
                GraphTargetT::Default => graph_quads(&snap, None)?,
                GraphTargetT::Named(iri) => match graph_col(&snap, iri) {
                    Some(col) => graph_quads(&snap, Some(col))?,
                    None => return missing(&String::from_utf8_lossy(iri), *silent),
                },
                GraphTargetT::AllNamed => collect(&snap, &Pattern::default(), |g| g > 0)?,
                GraphTargetT::All => collect(&snap, &Pattern::default(), |_| true)?,
            };
            apply_shared(store, &dels, &[])
        }
        UpdateOpT::Create { .. } => Ok(()), // quad-set semantics: no-op
        UpdateOpT::Add { silent, from, to }
        | UpdateOpT::Move { silent, from, to }
        | UpdateOpT::Copy { silent, from, to } => {
            if from == to {
                return Ok(());
            }
            let src_col = match from {
                None => None,
                Some(iri) => match graph_col(&snap, iri) {
                    Some(col) => Some(col),
                    None => return missing(&String::from_utf8_lossy(iri), *silent),
                },
            };
            let src = graph_quads(&snap, src_col)?;
            // Shared terms: retargeting the graph clones pointers only.
            let to_term: Option<Term> = to.as_ref().map(|iri| Term::from(iri.as_slice()));
            let adds: Vec<SharedQuad> = src
                .iter()
                .map(|q| {
                    (
                        Rc::clone(&q.0),
                        Rc::clone(&q.1),
                        Rc::clone(&q.2),
                        to_term.as_ref().map(Rc::clone),
                    )
                })
                .collect();
            let mut dels = Vec::new();
            let clears_dest = !matches!(op, UpdateOpT::Add { .. });
            if clears_dest {
                let dst_col = match to {
                    None => None,
                    Some(iri) => graph_col(&snap, iri),
                };
                if dst_col.is_some() || to.is_none() {
                    dels.extend(graph_quads(&snap, dst_col)?);
                }
            }
            if matches!(op, UpdateOpT::Move { .. }) {
                dels.extend(src.iter().cloned());
            }
            apply_shared(store, &dels, &adds)
        }
    }
}

/// The WHERE algebra of a DELETE WHERE: its quads grouped by graph term.
fn quads_algebra(quads: &[QuadPat]) -> graphy_algebra::Algebra {
    use graphy_algebra::{Algebra, TriplePat};
    let mut root: Option<Algebra> = None;
    let mut i = 0;
    while i < quads.len() {
        // Consecutive same-graph quads form one BGP.
        let g = quads[i].g.clone();
        let mut bgp = Vec::new();
        while i < quads.len() && quads[i].g == g {
            bgp.push(TriplePat {
                s: quads[i].s.clone(),
                p: quads[i].p.clone(),
                o: quads[i].o.clone(),
            });
            i += 1;
        }
        let node = match g {
            None => Algebra::Bgp(bgp),
            Some(graph) => Algebra::Graph {
                graph,
                input: Box::new(Algebra::Bgp(bgp)),
            },
        };
        root = Some(match root {
            None => node,
            Some(prev) => Algebra::Join(Box::new(prev), Box::new(node)),
        });
    }
    root.unwrap_or(Algebra::Bgp(Vec::new()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decorrelated_floor_raises_minted_labels() {
        decorrelate_fresh_labels(1_000_000);
        let mut map = HashMap::new();
        let label = freshen(b"_b0", &mut map);
        let ordinal = label
            .iter()
            .rposition(|&b| b == b'c')
            .expect("`_u{session}c{n}` separator");
        let n: u64 = std::str::from_utf8(&label[ordinal + 1..])
            .expect("`_u{session}c{n}` is ASCII")
            .parse()
            .expect("ordinal digits");
        assert!(n >= 1_000_000, "minted update label below the floor");
    }
}
