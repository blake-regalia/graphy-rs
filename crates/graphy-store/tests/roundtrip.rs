//! End-to-end segment tests: parse N-Quads → build a segment → open → scan
//! everything back and compare, exercise pattern scans and exact counts,
//! deterministic rebuild, and corruption detection (M2 exit criteria).

use std::collections::BTreeSet;
use std::path::PathBuf;

use graphy_store::{BuilderConfig, OpenMode, Pattern, Profile, Segment, SegmentBuilder, TermPos};
use graphy_turtle::{NQuadsParser, Options};

/// A decoded concise quad for model comparison.
type CQuad = (Vec<u8>, Vec<u8>, Vec<u8>, Option<Vec<u8>>);

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("graphy-store-e2e-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

const DATA: &str = concat!(
    // Shared terms, multiple graphs, repeated + duplicate quads.
    "<http://x/a> <http://x/p> <http://x/b> .\n",
    "<http://x/a> <http://x/p> <http://x/b> .\n", // duplicate collapses
    "<http://x/b> <http://x/p> <http://x/a> .\n",
    "<http://x/a> <http://x/q> \"plain\" .\n",
    "<http://x/a> <http://x/q> \"tagged\"@en-US <http://x/g1> .\n",
    "<http://x/b> <http://x/q> \"42\"^^<http://www.w3.org/2001/XMLSchema#integer> <http://x/g1> .\n",
    "<http://x/b> <http://x/q> \"042\"^^<http://www.w3.org/2001/XMLSchema#integer> <http://x/g2> .\n",
    "_:blank <http://x/p> \"4.5\"^^<http://www.w3.org/2001/XMLSchema#decimal> _:gb .\n",
    // Same triple in several graphs (graph layer must fan out).
    "<http://x/m> <http://x/p> <http://x/a> <http://x/g1> .\n",
    "<http://x/m> <http://x/p> <http://x/a> <http://x/g2> .\n",
    "<http://x/m> <http://x/p> <http://x/a> .\n",
    // RDF 1.2 triple term object (nested).\n
    "<http://x/r> <http://x/reifies> <<( <http://x/a> <http://x/p> <<( <http://x/b> <http://x/q> \"5\"^^<http://www.w3.org/2001/XMLSchema#integer> )>> )>> .\n",
    // Datatyped literal that stays in the dictionary.\n
    "<http://x/a> <http://x/q> \"x\"^^<http://x/dt> .\n",
);

fn parse_quads(input: &str) -> Vec<CQuad> {
    let mut p = NQuadsParser::new(Options::default()).unwrap();
    p.feed(input.as_bytes()).unwrap();
    let mut out: Vec<_> = p
        .drain()
        .map(|q| {
            (
                q.s.to_vec(),
                q.p.to_vec(),
                q.o.to_vec(),
                q.g.map(<[u8]>::to_vec),
            )
        })
        .collect();
    p.finish().unwrap();
    out.extend(p.drain().map(|q| {
        (
            q.s.to_vec(),
            q.p.to_vec(),
            q.o.to_vec(),
            q.g.map(<[u8]>::to_vec),
        )
    }));
    out
}

fn build(dir: &PathBuf, input: &str, profile: Profile) -> graphy_store::Manifest {
    let mut cfg = BuilderConfig::new(dir);
    cfg.profile = profile;
    cfg.sort_budget = 1 << 16; // tiny budget exercises spill/merge
    let mut b = SegmentBuilder::new(cfg).unwrap();
    for (s, p, o, g) in parse_quads(input) {
        b.push_quad(&s, &p, &o, g.as_deref()).unwrap();
    }
    b.finish().unwrap()
}

/// Scan everything and decode back to concise quads (set form).
fn dump(seg: &Segment) -> BTreeSet<CQuad> {
    let mut out = BTreeSet::new();
    for q in seg.scan(&Pattern::default()).unwrap() {
        let s = seg.decode_value(q[0], TermPos::Subject).unwrap();
        let p = seg.decode_value(q[1], TermPos::Predicate).unwrap();
        let o = seg.decode_value(q[2], TermPos::Object).unwrap();
        let g = if q[3] == 0 {
            None
        } else {
            Some(seg.decode_value(q[3] - 1, TermPos::Graph).unwrap())
        };
        out.insert((s, p, o, g));
    }
    out
}

#[test]
fn round_trip_all_profiles() {
    let expected: BTreeSet<_> = parse_quads(DATA).into_iter().collect();
    for profile in [Profile::Compact, Profile::Balanced, Profile::Covering] {
        let dir = scratch(&format!("rt-{}", profile.name()));
        let manifest = build(&dir, DATA, profile);
        assert_eq!(manifest.counts.quads, expected.len() as u64);
        let seg = Segment::open(&dir).unwrap();
        assert_eq!(dump(&seg), expected, "profile {}", profile.name());
        // Verify passes on a fresh build.
        graphy_store::Segment::verify(&dir).unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
    }
}

#[test]
fn pattern_scans_and_counts() {
    let dir = scratch("patterns");
    build(&dir, DATA, Profile::Balanced);
    let seg = Segment::open(&dir).unwrap();
    let iri = |s: &str| {
        let mut v = vec![b'>'];
        v.extend_from_slice(s.as_bytes());
        v
    };

    // Subject-bound.
    let pat = seg
        .resolve_pattern(Some(&iri("http://x/a")), None, None, None)
        .unwrap();
    assert_eq!(seg.count(&pat).unwrap(), 4);
    assert_eq!(seg.scan(&pat).unwrap().len(), 4);

    // Predicate-bound (uses POS).
    let pat = seg
        .resolve_pattern(None, Some(&iri("http://x/q")), None, None)
        .unwrap();
    assert_eq!(seg.count(&pat).unwrap(), 5);

    // Object-bound on an inline numeric (uses OSP).
    let mut o42 = Vec::new();
    o42.extend_from_slice(b"^>http://www.w3.org/2001/XMLSchema#integer\"42");
    let pat = seg.resolve_pattern(None, None, Some(&o42), None).unwrap();
    assert_eq!(seg.count(&pat).unwrap(), 1);
    // The non-canonical spelling is a distinct term.
    let mut o042 = Vec::new();
    o042.extend_from_slice(b"^>http://www.w3.org/2001/XMLSchema#integer\"042");
    let pat2 = seg.resolve_pattern(None, None, Some(&o042), None).unwrap();
    assert_eq!(seg.count(&pat2).unwrap(), 1);
    assert_ne!(pat, pat2);

    // Graph-bound.
    let pat = seg
        .resolve_pattern(None, None, None, Some(Some(&iri("http://x/g1"))))
        .unwrap();
    assert_eq!(seg.count(&pat).unwrap(), 3);
    // Default graph only.
    let pat = seg.resolve_pattern(None, None, None, Some(None)).unwrap();
    assert_eq!(seg.count(&pat).unwrap(), 6);

    // Fully bound.
    let pat = seg
        .resolve_pattern(
            Some(&iri("http://x/m")),
            Some(&iri("http://x/p")),
            Some(&iri("http://x/a")),
            Some(Some(&iri("http://x/g2"))),
        )
        .unwrap();
    assert_eq!(seg.count(&pat).unwrap(), 1);

    // Absent term → no pattern.
    assert!(seg
        .resolve_pattern(Some(&iri("http://x/nope")), None, None, None)
        .is_none());

    // Triple-term object resolves and scans.
    let quads = parse_quads(DATA);
    let tt = quads
        .iter()
        .find(|(_, _, o, _)| o.first() == Some(&0x09))
        .map(|(_, _, o, _)| o.clone())
        .unwrap();
    let pat = seg.resolve_pattern(None, None, Some(&tt), None).unwrap();
    assert_eq!(seg.count(&pat).unwrap(), 1);
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn deterministic_rebuild() {
    let d1 = scratch("det-1");
    let d2 = scratch("det-2");
    build(&d1, DATA, Profile::Balanced);
    build(&d2, DATA, Profile::Balanced);
    // Byte-identical components (manifest includes digests of all of them).
    let m1 = std::fs::read(d1.join("MANIFEST.json")).unwrap();
    let m2 = std::fs::read(d2.join("MANIFEST.json")).unwrap();
    assert_eq!(m1, m2);
    let manifest = Segment::open(&d1).unwrap().manifest;
    for (rel, _) in manifest.components.iter().chain(manifest.sidecars.iter()) {
        let a = std::fs::read(d1.join(rel)).unwrap();
        let b = std::fs::read(d2.join(rel)).unwrap();
        assert_eq!(a, b, "{rel} differs between rebuilds");
    }
    std::fs::remove_dir_all(&d1).unwrap();
    std::fs::remove_dir_all(&d2).unwrap();
}

#[test]
fn hash_sidecars_are_optional_for_open_but_verified() {
    let dir = scratch("sidecar");
    build(&dir, DATA, Profile::Balanced);
    let iri = |s: &str| {
        let mut v = vec![b'>'];
        v.extend_from_slice(s.as_bytes());
        v
    };

    // Baseline: sidecars listed and resolution works.
    let seg = Segment::open(&dir).unwrap();
    assert_eq!(seg.manifest.sidecars.len(), 5);
    assert!(seg
        .resolve_term(&iri("http://x/a"), TermPos::Subject)
        .is_some());
    Segment::verify(&dir).unwrap();

    // Corrupt a sidecar payload: open falls back to PFC binary search and
    // resolution still agrees; deep verify reports the corruption.
    let victim = dir.join("dict/subjects.hash");
    let pristine = std::fs::read(&victim).unwrap();
    let mut bytes = pristine.clone();
    let at = bytes.len() - 1;
    bytes[at] ^= 0xFF;
    std::fs::write(&victim, &bytes).unwrap();
    let seg = Segment::open(&dir).unwrap();
    assert!(seg
        .resolve_term(&iri("http://x/m"), TermPos::Subject)
        .is_some());
    assert!(Segment::verify(&dir).is_err());

    // Missing sidecar: same split (open tolerates, verify does not).
    std::fs::remove_file(&victim).unwrap();
    let seg = Segment::open(&dir).unwrap();
    assert!(seg
        .resolve_term(&iri("http://x/m"), TermPos::Subject)
        .is_some());
    assert!(Segment::verify(&dir).is_err());

    std::fs::write(&victim, &pristine).unwrap();
    Segment::verify(&dir).unwrap();
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn verify_catches_corruption() {
    let dir = scratch("corrupt");
    build(&dir, DATA, Profile::Balanced);
    Segment::verify(&dir).unwrap();
    // Flip one byte in a component payload.
    let victim = dir.join("idx/spo.bt");
    let mut bytes = std::fs::read(&victim).unwrap();
    let at = bytes.len() - 3;
    bytes[at] ^= 0x01;
    std::fs::write(&victim, &bytes).unwrap();
    assert!(Segment::verify(&dir).is_err());
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn mmap_open_agrees_with_heap() {
    for (name, data) in [
        ("mmap-graphs", DATA),
        (
            "mmap-triples",
            "<http://x/s> <http://x/p> <http://x/o> .\n<http://x/s> <http://x/p> \"v\" .\n",
        ),
    ] {
        let dir = scratch(name);
        build(&dir, data, Profile::Balanced);
        let heap = Segment::open_with(&dir, OpenMode::Heap).unwrap();
        let mapped = Segment::open_with(&dir, OpenMode::Mmap).unwrap();
        assert_eq!(dump(&heap), dump(&mapped), "{name}: dumps differ");

        // Pattern scans and counts agree across modes (incl. sidecar-backed
        // resolution and Pz-backed graph counts on secondary orderings).
        let iri = |s: &str| {
            let mut v = vec![b'>'];
            v.extend_from_slice(s.as_bytes());
            v
        };
        for (s, p, g) in [
            (None, None, None),
            (Some(iri("http://x/a")), None, None),
            (None, Some(iri("http://x/p")), None),
            (
                None,
                Some(iri("http://x/q")),
                Some(Some(iri("http://x/g1"))),
            ),
            (None, None, Some(None)),
        ] {
            let hp = heap.resolve_pattern(
                s.as_deref(),
                p.as_deref(),
                None,
                g.as_ref().map(|x| x.as_deref()),
            );
            let mp = mapped.resolve_pattern(
                s.as_deref(),
                p.as_deref(),
                None,
                g.as_ref().map(|x| x.as_deref()),
            );
            assert_eq!(hp, mp, "{name}: resolved patterns differ");
            let Some(pat) = hp else { continue };
            assert_eq!(heap.scan(&pat).unwrap(), mapped.scan(&pat).unwrap());
            assert_eq!(heap.count(&pat).unwrap(), mapped.count(&pat).unwrap());
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }
}

#[test]
fn mmap_open_skips_digests_but_validates_headers() {
    let dir = scratch("mmap-integrity");
    build(&dir, DATA, Profile::Balanced);

    // Flip a byte inside a PFC suffix: structurally valid, wrong content.
    // Heap open catches it via the payload digest; mmap open (by design,
    // doc 02 §6) does not — that is `verify`'s job.
    let victim = dir.join("dict/objects.pfc");
    let mut bytes = std::fs::read(&victim).unwrap();
    let last = bytes.len() - 1;
    bytes[last] ^= 0x01;
    std::fs::write(&victim, &bytes).unwrap();
    assert!(Segment::open_with(&dir, OpenMode::Heap).is_err());
    assert!(Segment::open_with(&dir, OpenMode::Mmap).is_ok());
    assert!(Segment::verify(&dir).is_err());

    // Header damage is caught in both modes.
    bytes[last] ^= 0x01; // restore payload
    bytes[10] ^= 0xFF; // version field
    std::fs::write(&victim, &bytes).unwrap();
    assert!(Segment::open_with(&dir, OpenMode::Heap).is_err());
    assert!(Segment::open_with(&dir, OpenMode::Mmap).is_err());

    // Truncation is caught in both modes.
    bytes[10] ^= 0xFF;
    bytes.truncate(bytes.len() - 3);
    std::fs::write(&victim, &bytes).unwrap();
    assert!(Segment::open_with(&dir, OpenMode::Heap).is_err());
    assert!(Segment::open_with(&dir, OpenMode::Mmap).is_err());
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn triples_only_dataset_skips_graph_layer() {
    let dir = scratch("triples-only");
    let manifest = build(
        &dir,
        "<http://x/s> <http://x/p> <http://x/o> .\n<http://x/s> <http://x/p> \"v\" .\n",
        Profile::Balanced,
    );
    assert!(!manifest.has_graphs);
    assert!(!dir.join("graphs/at.roar").exists());
    let seg = Segment::open(&dir).unwrap();
    assert_eq!(seg.scan(&Pattern::default()).unwrap().len(), 2);
    Segment::verify(&dir).unwrap();
    std::fs::remove_dir_all(&dir).unwrap();
}
