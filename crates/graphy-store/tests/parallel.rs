//! Parallel ingestion tests (doc 07 §7): segments built through concurrent
//! ingest lanes must be byte-identical to serially built ones — the sharded
//! dictionary's intern order is nondeterministic, but everything persisted
//! derives from byte-sorted sections.

use std::path::PathBuf;
use std::sync::Mutex;

use graphy_store::{BuilderConfig, Profile, Segment, SegmentBuilder};

/// A concise-encoded quad: (s, p, o, graph).
type CQuad = (Vec<u8>, Vec<u8>, Vec<u8>, Option<Vec<u8>>);

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("graphy-store-par-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

/// Concise-term quads covering all term kinds, shared terms, named graphs,
/// and duplicates.
fn quads(n: usize) -> Vec<CQuad> {
    (0..n)
        .map(|i| {
            let s = format!(">http://x/s{}", i % 40).into_bytes();
            let p = format!(">http://x/p{}", i % 7).into_bytes();
            let o = match i % 4 {
                0 => format!(">http://x/s{}", (i * 3) % 40).into_bytes(), // shared
                1 => format!("\"literal {}", i % 25).into_bytes(),
                2 => format!("^>http://www.w3.org/2001/XMLSchema#integer\"{}", i % 30).into_bytes(),
                _ => b"_someblank".to_vec(),
            };
            let g = (i % 3 != 0).then(|| format!(">http://x/g{}", i % 3).into_bytes());
            (s, p, o, g)
        })
        .collect()
}

fn build_serial(dir: &PathBuf, data: &[CQuad]) {
    let mut cfg = BuilderConfig::new(dir);
    cfg.sort_budget = 1 << 14;
    cfg.profile = Profile::Balanced;
    let mut b = SegmentBuilder::new(cfg).unwrap();
    for (s, p, o, g) in data {
        b.push_quad(s, p, o, g.as_deref()).unwrap();
    }
    b.finish().unwrap();
}

fn build_parallel(dir: &PathBuf, data: &[CQuad], threads: usize) {
    let mut cfg = BuilderConfig::new(dir);
    cfg.sort_budget = 1 << 14;
    cfg.profile = Profile::Balanced;
    let mut b = SegmentBuilder::new(cfg).unwrap();
    let lanes: Vec<Mutex<_>> = b
        .lanes(threads)
        .unwrap()
        .into_iter()
        .map(Mutex::new)
        .collect();
    std::thread::scope(|scope| {
        for (t, lane) in lanes.iter().enumerate() {
            scope.spawn(move || {
                for (s, p, o, g) in data.iter().skip(t).step_by(threads) {
                    lane.lock()
                        .unwrap()
                        .push_quad(s, p, o, g.as_deref())
                        .unwrap();
                }
            });
        }
    });
    let mut n = 0;
    for lane in lanes {
        let lane = lane.into_inner().unwrap();
        n += lane.pushed();
        b.join(lane).unwrap();
    }
    assert_eq!(n as usize, data.len());
    b.finish().unwrap();
}

/// Every on-disk artifact (manifest, components, sidecars) must be
/// byte-identical between a serial build and concurrent lane builds of any
/// thread count.
#[test]
fn lanes_build_byte_identical_segments() {
    let data = quads(4000);
    let base = scratch("serial");
    build_serial(&base, &data);
    let manifest = Segment::open(&base).unwrap().manifest;

    for threads in [1usize, 2, 4, 8] {
        let dir = scratch(&format!("lanes-{threads}"));
        build_parallel(&dir, &data, threads);
        let m1 = std::fs::read(base.join("MANIFEST.json")).unwrap();
        let m2 = std::fs::read(dir.join("MANIFEST.json")).unwrap();
        assert_eq!(m1, m2, "manifest differs at {threads} threads");
        for (rel, _) in manifest.components.iter().chain(manifest.sidecars.iter()) {
            let a = std::fs::read(base.join(rel)).unwrap();
            let b = std::fs::read(dir.join(rel)).unwrap();
            assert_eq!(a, b, "{rel} differs at {threads} threads");
        }
        Segment::verify(&dir).unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
    }
    std::fs::remove_dir_all(&base).unwrap();
}

#[test]
fn lanes_after_serial_push_is_an_error() {
    let dir = scratch("mode-conflict");
    let mut b = SegmentBuilder::new(BuilderConfig::new(&dir)).unwrap();
    b.push_quad(b">http://x/s", b">http://x/p", b"\"v", None)
        .unwrap();
    assert!(b.lanes(2).is_err());
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn finish_with_outstanding_lane_is_an_error() {
    let dir = scratch("outstanding-lane");
    let mut b = SegmentBuilder::new(BuilderConfig::new(&dir)).unwrap();
    let mut lanes = b.lanes(2).unwrap();
    let keep = lanes.pop().unwrap();
    b.join(lanes.pop().unwrap()).unwrap();
    let err = b.finish().unwrap_err().to_string();
    assert!(err.contains("lanes still alive"), "got: {err}");
    drop(keep);
    std::fs::remove_dir_all(&dir).unwrap();
}

/// Spill-to-disk interning (doc 07 §7 memory ceiling): a build whose
/// intern budget forces MANY flushes — every term spills, hot terms
/// re-intern into fresh locals repeatedly — must produce a segment
/// byte-identical to the unbudgeted build, serial AND through lanes.
#[test]
fn intern_spilling_is_byte_identical() {
    let data = quads(4000);
    let base = scratch("nospill");
    build_serial(&base, &data);
    let manifest = Segment::open(&base).unwrap().manifest;

    let compare = |dir: &PathBuf, what: &str| {
        let m1 = std::fs::read(base.join("MANIFEST.json")).unwrap();
        let m2 = std::fs::read(dir.join("MANIFEST.json")).unwrap();
        assert_eq!(m1, m2, "manifest differs ({what})");
        for (rel, _) in manifest.components.iter().chain(manifest.sidecars.iter()) {
            let a = std::fs::read(base.join(rel)).unwrap();
            let b = std::fs::read(dir.join(rel)).unwrap();
            assert_eq!(a, b, "{rel} differs ({what})");
        }
        Segment::verify(dir).unwrap();
    };

    // Serial with a budget so small every few terms flush a run.
    let dir = scratch("spill-serial");
    let mut cfg = BuilderConfig::new(&dir);
    cfg.sort_budget = 1 << 14;
    cfg.profile = Profile::Balanced;
    cfg.intern_budget = Some(1 << 10); // 1 KiB: dozens of flushes
    let mut b = SegmentBuilder::new(cfg).unwrap();
    for (s, p, o, g) in &data {
        b.push_quad(s, p, o, g.as_deref()).unwrap();
    }
    b.finish().unwrap();
    compare(&dir, "serial spill");
    std::fs::remove_dir_all(&dir).unwrap();

    // Lanes with a tiny budget (per-shard split bottoms out at 64 KiB).
    let dir = scratch("spill-lanes");
    let mut cfg = BuilderConfig::new(&dir);
    cfg.sort_budget = 1 << 14;
    cfg.profile = Profile::Balanced;
    cfg.intern_budget = Some(1);
    let mut b = SegmentBuilder::new(cfg).unwrap();
    let lanes: Vec<Mutex<_>> = b.lanes(4).unwrap().into_iter().map(Mutex::new).collect();
    std::thread::scope(|scope| {
        for (t, lane) in lanes.iter().enumerate() {
            let data = &data;
            scope.spawn(move || {
                for (s, p, o, g) in data.iter().skip(t).step_by(4) {
                    lane.lock()
                        .unwrap()
                        .push_quad(s, p, o, g.as_deref())
                        .unwrap();
                }
            });
        }
    });
    for lane in lanes {
        b.join(lane.into_inner().unwrap()).unwrap();
    }
    b.finish().unwrap();
    compare(&dir, "lane spill");
    std::fs::remove_dir_all(&dir).unwrap();
    std::fs::remove_dir_all(&base).unwrap();
}

/// The two-pass build must reproduce the one-pass triple-term table
/// bit-for-bit: nested terms, inline-value components, and terms repeated
/// across quads all take the depth-then-sorted-record ordinal rule.
#[test]
fn two_pass_handles_triple_terms() {
    let mut data = quads(600);
    let iri = |s: &str| format!(">http://x/{s}").into_bytes();
    let mut tts: Vec<Vec<u8>> = Vec::new();
    for i in 0..40 {
        let mut inner = Vec::new();
        graphy_core::concise::encode_triple_term(
            &mut inner,
            &iri(&format!("s{}", i % 40)),
            &iri(&format!("p{}", i % 7)),
            &format!("^>http://www.w3.org/2001/XMLSchema#integer\"{i}").into_bytes(),
        );
        if i % 3 == 0 {
            // Nest: an asserted term quoting another (depth 2).
            let mut outer = Vec::new();
            graphy_core::concise::encode_triple_term(
                &mut outer,
                &iri("nest"),
                &iri("quotes"),
                &inner,
            );
            tts.push(outer);
        }
        tts.push(inner);
    }
    for (i, tt) in tts.into_iter().enumerate() {
        // Each tt appears in several quads (dedup must not skew ordinals).
        for j in 0..3 {
            data.push((
                iri(&format!("s{}", (i + j) % 40)),
                iri("asserts"),
                tt.clone(),
                (i % 2 == 0).then(|| iri("g1")),
            ));
        }
    }

    let base = scratch("tt-nospill");
    build_serial(&base, &data);
    let manifest = Segment::open(&base).unwrap().manifest;

    let dir = scratch("tt-spill");
    let mut cfg = BuilderConfig::new(&dir);
    cfg.sort_budget = 1 << 14;
    cfg.profile = Profile::Balanced;
    cfg.intern_budget = Some(1 << 10);
    let mut b = SegmentBuilder::new(cfg).unwrap();
    for (s, p, o, g) in &data {
        b.push_quad(s, p, o, g.as_deref()).unwrap();
    }
    b.finish().unwrap();

    let m1 = std::fs::read(base.join("MANIFEST.json")).unwrap();
    let m2 = std::fs::read(dir.join("MANIFEST.json")).unwrap();
    assert_eq!(m1, m2, "manifest differs");
    for (rel, _) in manifest.components.iter().chain(manifest.sidecars.iter()) {
        let a = std::fs::read(base.join(rel)).unwrap();
        let b = std::fs::read(dir.join(rel)).unwrap();
        assert_eq!(a, b, "{rel} differs");
    }
    Segment::verify(&dir).unwrap();
    std::fs::remove_dir_all(&base).unwrap();
    std::fs::remove_dir_all(&dir).unwrap();
}
