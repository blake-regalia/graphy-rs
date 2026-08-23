//! HDT interop tests: read a FOREIGN hdt-cpp/hdt-java-produced file
//! (dbpedia.hdt from the hdt-java test suite; expectations below were
//! produced by pyHDT over the same file), and round-trip our own writer
//! through our reader.
//!
//! CAVEAT: the foreign-file test SKIPS SILENTLY when testdata/hdt is
//! absent (it is gitignored, like the W3C suite) — confirm "foreign:"
//! output under --nocapture before trusting green. Fetch with:
//!   mkdir -p testdata/hdt && curl -sL -o testdata/hdt/dbpedia.hdt \
//!     https://raw.githubusercontent.com/rdfhdt/hdt-java/master/hdt-java-core/src/test/resources/dbpedia.hdt

use std::collections::BTreeSet;
use std::path::PathBuf;

use graphy_core::Term;
use graphy_hdt::{HdtReader, HdtWriter};

fn foreign() -> Option<PathBuf> {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../testdata/hdt/dbpedia.hdt");
    p.exists().then_some(p)
}

/// Ground truth from pyHDT over the same file.
#[test]
fn reads_foreign_hdt_cpp_file() {
    let Some(path) = foreign() else {
        eprintln!("testdata/hdt/dbpedia.hdt absent — foreign interop SKIPPED");
        return;
    };
    let r = HdtReader::open(&path).unwrap();
    assert_eq!(r.n_triples(), 320_771);

    let mut n = 0u64;
    let mut first: Option<(Vec<u8>, Vec<u8>, Vec<u8>)> = None;
    let mut monotone = true;
    let mut prev: Option<(Vec<u8>, Vec<u8>, Vec<u8>)> = None;
    r.each_triple(|s, p, o| {
        if first.is_none() {
            first = Some((s.to_vec(), p.to_vec(), o.to_vec()));
        }
        // Spot the id-order contract (grouped by s, then p).
        let cur = (s.to_vec(), p.to_vec(), o.to_vec());
        if let Some(pv) = &prev {
            monotone &= pv.0 <= cur.0 || pv.0 != cur.0;
        }
        prev = Some(cur);
        n += 1;
        Ok(())
    })
    .unwrap();
    assert_eq!(n, 320_771);
    assert!(monotone);

    // pyHDT's first triple over this file.
    let (s, p, o) = first.unwrap();
    assert_eq!(
        s,
        Term::iri("http://commons.wikimedia.org/wiki/Special:FilePath/!!!善福寺.JPG?width=300")
            .unwrap()
            .as_concise()
    );
    assert_eq!(
        p,
        Term::iri("http://purl.org/dc/elements/1.1/rights")
            .unwrap()
            .as_concise()
    );
    assert_eq!(
        o,
        Term::iri("http://en.wikipedia.org/wiki/File:!!!善福寺.JPG")
            .unwrap()
            .as_concise()
    );
    println!("foreign: 320771 triples verified against pyHDT ground truth");
}

/// Writer → reader round-trip across every term kind HDT can carry.
#[test]
fn writer_reader_round_trip() {
    let iri = |s: &str| Term::iri(s).unwrap().as_concise().to_vec();
    let mut triples: BTreeSet<(Vec<u8>, Vec<u8>, Vec<u8>)> = BTreeSet::new();
    let mut w = HdtWriter::new();
    let mut add = |s: Vec<u8>, p: Vec<u8>, o: Vec<u8>| {
        w.add_triple(&s, &p, &o).unwrap();
        triples.insert((s, p, o));
    };
    let p0 = iri("http://x/p0");
    let p1 = iri("http://x/p1");
    for i in 0..300u32 {
        add(
            iri(&format!("http://x/s{i}")),
            p0.clone(),
            iri(&format!("http://x/o{}", i % 40)),
        );
    }
    // Shared terms (subject that is also an object), every literal kind,
    // blank nodes, duplicates.
    add(iri("http://x/o1"), p1.clone(), iri("http://x/s0"));
    add(
        iri("http://x/s0"),
        p1.clone(),
        Term::literal_simple("plain \"quoted\" text")
            .as_concise()
            .to_vec(),
    );
    add(
        iri("http://x/s1"),
        p1.clone(),
        Term::literal_lang("bonjour", "fr", None)
            .unwrap()
            .as_concise()
            .to_vec(),
    );
    add(
        iri("http://x/s2"),
        p1.clone(),
        Term::literal_typed("42", "http://www.w3.org/2001/XMLSchema#integer")
            .unwrap()
            .as_concise()
            .to_vec(),
    );
    add(
        Term::blank_node("b0").unwrap().as_concise().to_vec(),
        p1.clone(),
        Term::blank_node("b1").unwrap().as_concise().to_vec(),
    );
    add(iri("http://x/s0"), p0.clone(), iri("http://x/o0")); // duplicate
    add(iri("http://x/s0"), p0.clone(), iri("http://x/o0"));

    let dir = std::env::temp_dir().join(format!("graphy-hdt-rt-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("rt.hdt");
    w.write_to_path(&path).unwrap();

    let r = HdtReader::open(&path).unwrap();
    assert_eq!(r.n_triples(), triples.len() as u64);
    let mut got: BTreeSet<(Vec<u8>, Vec<u8>, Vec<u8>)> = BTreeSet::new();
    r.each_triple(|s, p, o| {
        got.insert((s.to_vec(), p.to_vec(), o.to_vec()));
        Ok(())
    })
    .unwrap();
    assert_eq!(got, triples);
    std::fs::remove_dir_all(&dir).ok();
}

/// Compare two segment directories byte-for-byte (component files +
/// manifest).
fn dirs_identical(a: &std::path::Path, b: &std::path::Path) -> bool {
    fn files(d: &std::path::Path, base: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
        for e in std::fs::read_dir(d).unwrap().flatten() {
            let p = e.path();
            if p.is_dir() {
                files(&p, base, out);
            } else if p.extension().is_none_or(|x| x != "scratch") {
                out.push(p.strip_prefix(base).unwrap().to_owned());
            }
        }
    }
    let (mut fa, mut fb) = (Vec::new(), Vec::new());
    files(a, a, &mut fa);
    files(b, b, &mut fb);
    fa.sort();
    fb.sort();
    if fa != fb {
        eprintln!("file sets differ: {fa:?} vs {fb:?}");
        return false;
    }
    fa.iter().all(|rel| {
        let same = std::fs::read(a.join(rel)).unwrap() == std::fs::read(b.join(rel)).unwrap();
        if !same {
            eprintln!("component differs: {}", rel.display());
        }
        same
    })
}

/// The fast import (sections → build_from_sorted_dict, no re-interning)
/// produces a segment BYTE-IDENTICAL to the parser/intern path over the
/// same file — inline extraction, shared handling, and the concise
/// re-sort all line up with what BuildDict computes.
#[test]
fn fast_import_is_byte_identical_to_slow_path() {
    use graphy_store::{BuilderConfig, Profile, SegmentBuilder};

    // A file with every wrinkle: shared terms, bnodes, inline-able typed
    // literals (extracted from the dictionary), non-inline typed literals,
    // language literals.
    let iri = |s: &str| Term::iri(s).unwrap().as_concise().to_vec();
    let mut w = HdtWriter::new();
    let p0 = iri("http://x/p0");
    for i in 0..200u32 {
        w.add_triple(
            &iri(&format!("http://x/s{i}")),
            &p0,
            &iri(&format!("http://x/o{}", i % 30)),
        )
        .unwrap();
    }
    w.add_triple(&iri("http://x/o5"), &p0, &iri("http://x/s0"))
        .unwrap();
    w.add_triple(
        &iri("http://x/s1"),
        &p0,
        Term::literal_typed("42", "http://www.w3.org/2001/XMLSchema#integer")
            .unwrap()
            .as_concise(),
    )
    .unwrap();
    w.add_triple(
        &iri("http://x/s2"),
        &p0,
        Term::literal_typed("not-a-number", "http://x/customType")
            .unwrap()
            .as_concise(),
    )
    .unwrap();
    w.add_triple(
        &iri("http://x/s3"),
        &p0,
        Term::literal_lang("hej", "sv", None).unwrap().as_concise(),
    )
    .unwrap();
    w.add_triple(
        Term::blank_node("n0").unwrap().as_concise(),
        &p0,
        Term::literal_simple("plain").as_concise(),
    )
    .unwrap();

    let base = std::env::temp_dir().join(format!("graphy-hdt-fast-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(&base).unwrap();
    let file = base.join("data.hdt");
    w.write_to_path(&file).unwrap();
    let r = HdtReader::open(&file).unwrap();

    // Slow path: re-intern every term through the builder.
    let slow_dir = base.join("slow");
    let mut cfg = BuilderConfig::new(&slow_dir);
    cfg.profile = Profile::Balanced;
    let mut b = SegmentBuilder::new(cfg).unwrap();
    r.each_triple(|s, p, o| {
        b.push_quad(s, p, o, None)
            .map_err(|e| graphy_hdt::HdtError::Format(e.to_string()))?;
        Ok(())
    })
    .unwrap();
    let slow_manifest = b.finish().unwrap();

    // Fast path.
    let fast_dir = base.join("fast");
    let mut cfg = BuilderConfig::new(&fast_dir);
    cfg.profile = Profile::Balanced;
    let fast_manifest = graphy_hdt::import_segment(&r, &cfg).unwrap();

    assert_eq!(fast_manifest.counts.quads, slow_manifest.counts.quads);
    assert!(dirs_identical(&slow_dir, &fast_dir), "fast ≠ slow segment");
    graphy_store::Segment::verify(&fast_dir).unwrap();
    std::fs::remove_dir_all(&base).ok();
}

/// Same equivalence over the foreign hdt-cpp file (skips without it).
#[test]
fn fast_import_matches_slow_on_foreign_file() {
    use graphy_store::{BuilderConfig, Profile, SegmentBuilder};
    let Some(path) = foreign() else {
        eprintln!("testdata/hdt/dbpedia.hdt absent — foreign fast-import SKIPPED");
        return;
    };
    let r = HdtReader::open(&path).unwrap();
    let base = std::env::temp_dir().join(format!("graphy-hdt-fastf-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(&base).unwrap();

    let slow_dir = base.join("slow");
    let mut cfg = BuilderConfig::new(&slow_dir);
    cfg.profile = Profile::Balanced;
    let mut b = SegmentBuilder::new(cfg).unwrap();
    r.each_triple(|s, p, o| {
        b.push_quad(s, p, o, None)
            .map_err(|e| graphy_hdt::HdtError::Format(e.to_string()))?;
        Ok(())
    })
    .unwrap();
    b.finish().unwrap();

    let fast_dir = base.join("fast");
    let mut cfg = BuilderConfig::new(&fast_dir);
    cfg.profile = Profile::Balanced;
    let manifest = graphy_hdt::import_segment(&r, &cfg).unwrap();
    assert_eq!(manifest.counts.quads, 320_771);
    assert!(dirs_identical(&slow_dir, &fast_dir), "fast ≠ slow segment");
    println!("foreign fast-import: byte-identical to the intern path");
    std::fs::remove_dir_all(&base).ok();
}

/// HDTQ (qEndpoint dialect): quads round-trip through writer → reader,
/// including a triple present in several graphs and default-graph mixes;
/// and the fast import produces a segment byte-identical to the intern
/// path over the same .hdtq file.
#[test]
fn hdtq_quads_round_trip_and_fast_import() {
    use graphy_store::{BuilderConfig, Profile, SegmentBuilder};

    let iri = |s: &str| Term::iri(s).unwrap().as_concise().to_vec();
    type Q = (Vec<u8>, Vec<u8>, Vec<u8>, Option<Vec<u8>>);
    let mut quads: BTreeSet<Q> = BTreeSet::new();
    let mut w = HdtWriter::new();
    let g1 = iri("http://x/g1");
    let g2 = iri("http://x/g2");
    let p0 = iri("http://x/p0");
    let mut add = |s: Vec<u8>, p: Vec<u8>, o: Vec<u8>, g: Option<Vec<u8>>| {
        w.add_quad(&s, &p, &o, g.as_deref()).unwrap();
        quads.insert((s, p, o, g));
    };
    for i in 0..60u32 {
        let g = match i % 3 {
            0 => None,
            1 => Some(g1.clone()),
            _ => Some(g2.clone()),
        };
        add(
            iri(&format!("http://x/s{i}")),
            p0.clone(),
            iri(&format!("http://x/o{}", i % 10)),
            g,
        );
    }
    // Same triple in BOTH named graphs and the default graph.
    let t = (iri("http://x/multi"), p0.clone(), iri("http://x/o1"));
    add(t.0.clone(), t.1.clone(), t.2.clone(), None);
    add(t.0.clone(), t.1.clone(), t.2.clone(), Some(g1.clone()));
    add(t.0.clone(), t.1.clone(), t.2.clone(), Some(g2.clone()));
    // Typed inline-able literal under a named graph (fast-import wrinkle).
    add(
        iri("http://x/s0"),
        p0.clone(),
        Term::literal_typed("7", "http://www.w3.org/2001/XMLSchema#integer")
            .unwrap()
            .as_concise()
            .to_vec(),
        Some(g1.clone()),
    );

    let base = std::env::temp_dir().join(format!("graphy-hdtq-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(&base).unwrap();
    let file = base.join("data.hdtq");
    w.write_to_path(&file).unwrap();

    // Reader round-trip.
    let r = HdtReader::open(&file).unwrap();
    assert!(r.has_graphs());
    let mut got: BTreeSet<Q> = BTreeSet::new();
    r.each_quad(|s, p, o, g| {
        got.insert((s.to_vec(), p.to_vec(), o.to_vec(), g.map(<[u8]>::to_vec)));
        Ok(())
    })
    .unwrap();
    assert_eq!(got, quads);

    // Fast import ≡ slow path, byte for byte.
    let slow_dir = base.join("slow");
    let mut cfg = BuilderConfig::new(&slow_dir);
    cfg.profile = Profile::Balanced;
    let mut b = SegmentBuilder::new(cfg).unwrap();
    r.each_quad(|s, p, o, g| {
        b.push_quad(s, p, o, g)
            .map_err(|e| graphy_hdt::HdtError::Format(e.to_string()))?;
        Ok(())
    })
    .unwrap();
    let slow_manifest = b.finish().unwrap();

    let fast_dir = base.join("fast");
    let mut cfg = BuilderConfig::new(&fast_dir);
    cfg.profile = Profile::Balanced;
    let fast_manifest = graphy_hdt::import_segment(&r, &cfg).unwrap();
    assert_eq!(fast_manifest.counts.quads, slow_manifest.counts.quads);
    assert_eq!(fast_manifest.counts.graphs, 2);
    assert!(dirs_identical(&slow_dir, &fast_dir), "hdtq fast ≠ slow");
    graphy_store::Segment::verify(&fast_dir).unwrap();
    std::fs::remove_dir_all(&base).ok();
}
