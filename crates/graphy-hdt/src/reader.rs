//! HDT file reader: FourSection dictionary + BitmapTriples in SPO order.
//!
//! Id spaces (mapping=1): subject ids are 1..=n_shared then the subjects
//! section; object ids are 1..=n_shared then the objects section;
//! predicate ids are 1..=n_predicates. BitmapTriples: `Sy[k]` lists the
//! predicate of the k-th (subject, predicate) pair with `By[k] = 1`
//! marking each subject's last pair; `Sz[m]` lists objects with
//! `Bz[m] = 1` marking each pair's last object. Subjects are implicit,
//! ascending from 1.

use std::path::Path;

use crate::codec::Cur;
use crate::quads::GraphAnnex;
use crate::section::{Bitmap, LogSeq, PfcSection};
use crate::term::hdt_to_concise;
use crate::HdtError;

const FMT_GLOBAL: &str = "<http://purl.org/HDT/hdt#HDTv1>";
pub(crate) const FMT_DICT_FOUR: &str = "<http://purl.org/HDT/hdt#dictionaryFour>";
pub(crate) const FMT_DICT_FOUR_QUAD: &str = "<http://purl.org/HDT/hdt#dictionaryFourQuad>";
pub(crate) const FMT_TRIPLES_BITMAP: &str = "<http://purl.org/HDT/hdt#triplesBitmap>";
pub(crate) const FMT_TRIPLES_BITMAP_QUAD: &str = "<http://purl.org/HDT/hdt#triplesBitmapQuad>";
const _: () = {
    // Order property values (hdt-cpp TripleComponentOrder): SPO = 1.
    assert!(ORDER_SPO == 1);
};
pub(crate) const ORDER_SPO: u64 = 1;

/// A parsed HDT file (owns the raw bytes; sections view into them).
#[derive(Debug)]
pub struct HdtReader {
    data: Vec<u8>,
    layout: Layout,
}

#[derive(Debug)]
struct Layout {
    shared: std::ops::Range<usize>,
    subjects: std::ops::Range<usize>,
    predicates: std::ops::Range<usize>,
    objects: std::ops::Range<usize>,
    /// HDTQ only (qEndpoint dialect): the graphs dictionary section.
    graphs: Option<std::ops::Range<usize>>,
    triples: std::ops::Range<usize>,
    /// HDTQ only: the per-graph triple-annotation annex.
    annex: Option<std::ops::Range<usize>>,
    n_triples: u64,
}

/// Borrowed decoded view (sections parse zero-copy over the buffer).
pub(crate) struct View<'a> {
    pub shared: PfcSection<'a>,
    pub subjects: PfcSection<'a>,
    pub predicates: PfcSection<'a>,
    pub objects: PfcSection<'a>,
    pub graphs: Option<PfcSection<'a>>,
    pub by: Bitmap<'a>,
    pub bz: Bitmap<'a>,
    pub sy: LogSeq<'a>,
    pub sz: LogSeq<'a>,
    pub annex: Option<GraphAnnex>,
}

impl HdtReader {
    pub fn open(path: &Path) -> Result<HdtReader, HdtError> {
        let data = std::fs::read(path)?;
        let layout = Self::parse_layout(&data)?;
        Ok(HdtReader { data, layout })
    }

    fn parse_layout(data: &[u8]) -> Result<Layout, HdtError> {
        let mut c = Cur::new(data);
        let (ty, fmt, _) = c.control_info()?;
        if ty != 1 || fmt != FMT_GLOBAL {
            return Err(HdtError::Format(format!("not an HDT v1 file ({fmt:?})")));
        }
        // Header: raw payload of `length` bytes (RDF metadata; skipped).
        let (ty, _, props) = c.control_info()?;
        if ty != 2 {
            return Err(HdtError::Format("expected header block".into()));
        }
        let hlen: usize = props
            .get("length")
            .and_then(|v| v.parse().ok())
            .ok_or_else(|| HdtError::Format("header length missing".into()))?;
        c.take(hlen)?;

        // Dictionary: four PFC sections, order shared/subjects/predicates/
        // objects (pinned against hdt-cpp output).
        let (ty, fmt, props) = c.control_info()?;
        let quads_dict = fmt == FMT_DICT_FOUR_QUAD;
        if ty != 3 || (fmt != FMT_DICT_FOUR && !quads_dict) {
            return Err(HdtError::Format(format!(
                "unsupported dictionary {fmt:?} (only dictionaryFour / dictionaryFourQuad)"
            )));
        }
        if props.get("mapping") != Some("1") {
            return Err(HdtError::Format("unsupported dictionary mapping".into()));
        }
        let section = |c: &mut Cur<'_>| -> Result<std::ops::Range<usize>, HdtError> {
            let start = c.pos;
            PfcSection::read(c)?;
            Ok(start..c.pos)
        };
        let shared = section(&mut c)?;
        let subjects = section(&mut c)?;
        let predicates = section(&mut c)?;
        let objects = section(&mut c)?;
        let graphs = quads_dict.then(|| section(&mut c)).transpose()?;

        // Triples: BitmapTriples in SPO (+ the HDTQ graph annex).
        let (ty, fmt, props) = c.control_info()?;
        let quads_triples = fmt == FMT_TRIPLES_BITMAP_QUAD;
        if ty != 4 || (fmt != FMT_TRIPLES_BITMAP && !quads_triples) {
            return Err(HdtError::Format(format!(
                "unsupported triples encoding {fmt:?} (only triplesBitmap / triplesBitmapQuad)"
            )));
        }
        if quads_dict != quads_triples {
            return Err(HdtError::Format(
                "mismatched quad dictionary/triples encodings".into(),
            ));
        }
        if props.get("order").map(str::trim) != Some("1") {
            return Err(HdtError::Format("only SPO-order HDT is supported".into()));
        }
        let tstart = c.pos;
        Bitmap::read(&mut c)?;
        Bitmap::read(&mut c)?;
        let sy = LogSeq::read(&mut c)?;
        LogSeq::read(&mut c)?;
        let triples = tstart..c.pos;
        let annex = quads_triples
            .then(|| {
                let start = c.pos;
                GraphAnnex::read(&mut c)?;
                Ok::<_, HdtError>(start..c.pos)
            })
            .transpose()?;
        Ok(Layout {
            shared,
            subjects,
            predicates,
            objects,
            graphs,
            triples,
            annex,
            n_triples: sy.n,
        })
    }

    pub(crate) fn view(&self) -> Result<View<'_>, HdtError> {
        let sec = |r: &std::ops::Range<usize>| -> Result<PfcSection<'_>, HdtError> {
            PfcSection::read(&mut Cur::new(&self.data[r.clone()]))
        };
        let mut c = Cur::new(&self.data[self.layout.triples.clone()]);
        Ok(View {
            shared: sec(&self.layout.shared)?,
            subjects: sec(&self.layout.subjects)?,
            predicates: sec(&self.layout.predicates)?,
            objects: sec(&self.layout.objects)?,
            graphs: self.layout.graphs.as_ref().map(&sec).transpose()?,
            by: Bitmap::read(&mut c)?,
            bz: Bitmap::read(&mut c)?,
            sy: LogSeq::read(&mut c)?,
            sz: LogSeq::read(&mut c)?,
            annex: self
                .layout
                .annex
                .as_ref()
                .map(|r| GraphAnnex::read(&mut Cur::new(&self.data[r.clone()])))
                .transpose()?,
        })
    }

    /// Whether this file carries HDTQ graph annotations.
    pub fn has_graphs(&self) -> bool {
        self.layout.annex.is_some()
    }

    pub fn n_triples(&self) -> u64 {
        self.layout.n_triples
    }

    /// Stream every triple as raw HDT ids (mapping=1 spaces: subject and
    /// object ids overlap the shared section; predicates are their own
    /// space), in (s, p, o)-id order — the fast import's input.
    pub fn each_triple_ids(
        &self,
        mut sink: impl FnMut(u64, u64, u64) -> Result<(), HdtError>,
    ) -> Result<(), HdtError> {
        let v = self.view()?;
        let (mut s_id, mut y, mut z) = (1u64, 0u64, 0u64);
        while z < v.sz.n {
            let p_id = v.sy.get(y);
            loop {
                sink(s_id, p_id, v.sz.get(z))?;
                let last = v.bz.get(z);
                z += 1;
                if last {
                    break;
                }
            }
            let last_pair = v.by.get(y);
            y += 1;
            if last_pair {
                s_id += 1;
            }
        }
        Ok(())
    }

    /// Stream every QUAD as raw ids: `(s, p, o, g)` with `g = 0` for the
    /// default graph (the empty-string graph entry, our dialect choice)
    /// and `g = i + 1` for graph-section entry `i` otherwise. A triple
    /// annotated with several graphs yields one quad per graph. Plain HDT
    /// files yield every triple with `g = 0`.
    pub fn each_quad_ids(
        &self,
        mut sink: impl FnMut(u64, u64, u64, u64) -> Result<(), HdtError>,
    ) -> Result<(), HdtError> {
        let v = self.view()?;
        let (annex, default_layer) = match (&v.annex, &v.graphs) {
            (Some(a), Some(g)) => {
                // Locate the empty-string graph entry, if present.
                let mut dl = None;
                for (i, t) in g.iter().enumerate() {
                    if t?.is_empty() {
                        dl = Some(i);
                    }
                }
                (Some(a), dl)
            }
            _ => (None, None),
        };
        let mut k = 0u64; // triple index
        let mut inner = |s: u64, p: u64, o: u64| -> Result<(), HdtError> {
            match annex {
                None => sink(s, p, o, 0)?,
                Some(a) => {
                    let mut any = false;
                    for g in 0..a.n_layers() {
                        if a.get(g, k) {
                            any = true;
                            let col = if default_layer == Some(g) {
                                0
                            } else {
                                g as u64 + 1
                            };
                            sink(s, p, o, col)?;
                        }
                    }
                    if !any {
                        return Err(HdtError::Format(format!(
                            "triple {k} annotated with no graph"
                        )));
                    }
                }
            }
            k += 1;
            Ok(())
        };
        self.each_triple_ids(&mut inner)
    }

    /// The four dictionary sections decoded to concise terms, in HDT
    /// (string) order: (shared, subjects, predicates, objects). The
    /// sections are independent, so they decode on scoped threads.
    #[allow(clippy::type_complexity)]
    pub fn dictionary_concise(
        &self,
    ) -> Result<(Vec<Vec<u8>>, Vec<Vec<u8>>, Vec<Vec<u8>>, Vec<Vec<u8>>), HdtError> {
        let v = self.view()?;
        let decode_all = |s: &PfcSection<'_>| -> Result<Vec<Vec<u8>>, HdtError> {
            s.iter()
                .map(|r| r.and_then(|b| hdt_to_concise(&String::from_utf8(b).map_err(bad_utf8)?)))
                .collect()
        };
        let (shared, (subjects, (predicates, objects))) = std::thread::scope(|scope| {
            let sh = scope.spawn(|| decode_all(&v.shared));
            let su = scope.spawn(|| decode_all(&v.subjects));
            let pr = scope.spawn(|| decode_all(&v.predicates));
            let ob = decode_all(&v.objects);
            (
                sh.join().expect("decode thread"),
                (
                    su.join().expect("decode thread"),
                    (pr.join().expect("decode thread"), ob),
                ),
            )
        });
        Ok((shared?, subjects?, predicates?, objects?))
    }

    /// Stream every quad as concise terms (graph `None` = default). Uses
    /// pre-decoded sections like [`HdtReader::each_triple`].
    pub fn each_quad(
        &self,
        mut sink: impl FnMut(&[u8], &[u8], &[u8], Option<&[u8]>) -> Result<(), HdtError>,
    ) -> Result<(), HdtError> {
        let v = self.view()?;
        let n_shared = v.shared.n;
        let decode_all = |s: &PfcSection<'_>| -> Result<Vec<Vec<u8>>, HdtError> {
            s.iter()
                .map(|r| r.and_then(|b| hdt_to_concise(&String::from_utf8(b).map_err(bad_utf8)?)))
                .collect()
        };
        let shared: Vec<Vec<u8>> = decode_all(&v.shared)?;
        let objects: Vec<Vec<u8>> = decode_all(&v.objects)?;
        let preds: Vec<Vec<u8>> = decode_all(&v.predicates)?;
        let graphs: Vec<Vec<u8>> = self.graphs_concise()?;
        drop(v);
        let mut last_s: Option<(u64, Vec<u8>)> = None;
        self.each_quad_ids(|s, p, o, g| {
            if last_s.as_ref().map(|(id, _)| *id) != Some(s) {
                let bytes = if s <= n_shared {
                    shared[(s - 1) as usize].clone()
                } else {
                    let view = self.view()?;
                    let raw = view.subjects.get(s - n_shared - 1)?;
                    hdt_to_concise(&String::from_utf8(raw).map_err(bad_utf8)?)?
                };
                last_s = Some((s, bytes));
            }
            let s_bytes = &last_s.as_ref().expect("just set").1;
            let o_bytes: &[u8] = if o <= n_shared {
                &shared[(o - 1) as usize]
            } else {
                &objects[(o - n_shared - 1) as usize]
            };
            let g_bytes = match g {
                0 => None,
                v => Some(graphs[(v - 1) as usize].as_slice()),
            };
            sink(s_bytes, &preds[(p - 1) as usize], o_bytes, g_bytes)
        })
    }

    /// The HDTQ graphs section decoded to concise terms in HDT order
    /// (empty for plain HDT). The empty-string entry (our default-graph
    /// spelling) is preserved as an empty Vec.
    pub fn graphs_concise(&self) -> Result<Vec<Vec<u8>>, HdtError> {
        let v = self.view()?;
        match &v.graphs {
            None => Ok(Vec::new()),
            Some(g) => g
                .iter()
                .map(|r| {
                    let b = r?;
                    if b.is_empty() {
                        return Ok(Vec::new());
                    }
                    hdt_to_concise(&String::from_utf8(b).map_err(bad_utf8)?)
                })
                .collect(),
        }
    }

    /// Stream every triple as concise terms, in (s, p, o)-id order, into
    /// `sink`. Subject and predicate terms are decoded once per run (the
    /// stream is grouped); objects decode per occurrence with the PFC
    /// block cache inside the section.
    pub fn each_triple(
        &self,
        mut sink: impl FnMut(&[u8], &[u8], &[u8]) -> Result<(), HdtError>,
    ) -> Result<(), HdtError> {
        let v = self.view()?;
        let n_shared = v.shared.n;
        // Pre-decode the shared + object sections once (sequential PFC
        // walks) instead of block-decoding per access — objects arrive in
        // scattered id order, and an import holds O(terms) in its build
        // dictionary anyway, so this adds a comparable constant.
        let decode_all = |s: &PfcSection<'_>| -> Result<Vec<Vec<u8>>, HdtError> {
            s.iter()
                .map(|r| r.and_then(|b| hdt_to_concise(&String::from_utf8(b).map_err(bad_utf8)?)))
                .collect()
        };
        let shared: Vec<Vec<u8>> = decode_all(&v.shared)?;
        let objects: Vec<Vec<u8>> = decode_all(&v.objects)?;
        let preds: Vec<Vec<u8>> = decode_all(&v.predicates)?;
        let subject = |id: u64| -> Result<Vec<u8>, HdtError> {
            if id <= n_shared {
                return Ok(shared[(id - 1) as usize].clone());
            }
            let s = v.subjects.get(id - n_shared - 1)?;
            hdt_to_concise(&String::from_utf8(s).map_err(bad_utf8)?)
        };
        let object = |id: u64| -> Result<&[u8], HdtError> {
            let t = if id <= n_shared {
                shared.get((id - 1) as usize)
            } else {
                objects.get((id - n_shared - 1) as usize)
            };
            t.map(Vec::as_slice)
                .ok_or_else(|| HdtError::Format(format!("object id {id} out of range")))
        };

        let (mut s_id, mut y, mut z) = (1u64, 0u64, 0u64);
        let mut s_bytes = subject(1)?;
        while z < v.sz.n {
            let p_id = v.sy.get(y);
            let p_bytes: &[u8] = preds
                .get((p_id - 1) as usize)
                .ok_or_else(|| HdtError::Format(format!("predicate id {p_id} out of range")))?;
            // All objects of this (s, p) pair.
            loop {
                let o_bytes = object(v.sz.get(z))?;
                sink(&s_bytes, p_bytes, o_bytes)?;
                let last = v.bz.get(z);
                z += 1;
                if last {
                    break;
                }
            }
            let last_pair = v.by.get(y);
            y += 1;
            if last_pair && z < v.sz.n {
                s_id += 1;
                s_bytes = subject(s_id)?;
            }
        }
        Ok(())
    }
}

fn bad_utf8(e: std::string::FromUtf8Error) -> HdtError {
    HdtError::Format(format!("non-UTF-8 dictionary entry: {e}"))
}
