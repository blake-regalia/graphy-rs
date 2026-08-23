//! W3C rdf-tests conformance harness (doc 03 §5): drives the Turtle, TriG,
//! N-Triples, and N-Quads manifests (1.1 + 1.2 suites) — positive/negative
//! syntax and eval tests, the latter compared under blank-node isomorphism.
//!
//! Requires the shallow clone at `graphy-rs/testdata/rdf-tests` (fetch with
//! `git clone --depth 1 https://github.com/w3c/rdf-tests testdata/rdf-tests`);
//! the test is skipped when the checkout is absent.
//!
//! The manifests themselves are parsed with our own Turtle parser.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use graphy_turtle::{NQuadsParser, NTriplesParser, Options, QuadRef, TriGParser, TurtleParser};

#[path = "../../../test-support/oracles.rs"]
mod oracles;

const RDF_TYPE: &str = "http://www.w3.org/1999/02/22-rdf-syntax-ns#type";
const MF: &str = "http://www.w3.org/2001/sw/DataAccess/tests/test-manifest#";
const RDFT: &str = "http://www.w3.org/ns/rdftest#";

#[derive(Debug, Clone, Copy, PartialEq)]
enum Format {
    Turtle,
    Trig,
    NTriples,
    NQuads,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Expect {
    Eval,
    Positive,
    Negative,
}

#[derive(Debug)]
struct Entry {
    name: String,
    format: Format,
    expect: Expect,
    action_url: String,
    action_path: PathBuf,
    result_path: Option<PathBuf>,
}

/// A term for comparison: raw concise bytes (blank labels are indexed later
/// during isomorphism checking).
type T = Vec<u8>;

type CQuad = (T, T, T, Option<T>);

fn suite_root() -> Option<PathBuf> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../testdata/rdf-tests/rdf");
    root.is_dir().then_some(root)
}

#[test]
fn w3c_rdf_tests() {
    let Some(root) = suite_root() else {
        eprintln!("skipping: testdata/rdf-tests not present");
        return;
    };
    // (relative dir, spec12 mode)
    let suites: &[(&str, bool)] = &[
        ("rdf11/rdf-turtle", false),
        ("rdf11/rdf-trig", false),
        ("rdf11/rdf-n-triples", false),
        ("rdf11/rdf-n-quads", false),
        ("rdf12/rdf-turtle/syntax", true),
        ("rdf12/rdf-turtle/eval", true),
        ("rdf12/rdf-trig/syntax", true),
        ("rdf12/rdf-trig/eval", true),
        ("rdf12/rdf-n-triples/syntax", true),
        ("rdf12/rdf-n-quads/syntax", true),
    ];
    let mut failures = Vec::new();
    let mut total = 0usize;
    for &(rel, spec12) in suites {
        let dir = root.join(rel);
        let entries = load_manifest(&dir, rel, None);
        assert!(!entries.is_empty(), "no entries parsed from {rel}");
        for e in &entries {
            total += 1;
            if let Err(msg) = run_entry(e, spec12) {
                failures.push(format!("[{rel}] {}: {msg}", e.name));
            }
        }
    }
    if !failures.is_empty() {
        panic!(
            "{} of {total} W3C tests failed:\n{}",
            failures.len(),
            failures.join("\n")
        );
    }
    println!("all {total} W3C tests passed");
}

#[test]
fn oxigraph_rdf_parser_tests() {
    let Some(root) = oracles::checkout("oxigraph") else {
        return;
    };
    let root = root.join("testsuite/oxigraph-tests");
    let suites = [
        (
            "parser",
            "https://github.com/oxigraph/oxigraph/tests/parser/",
        ),
        (
            "parser-error",
            "https://github.com/oxigraph/oxigraph/tests/parser-error/",
        ),
    ];
    let mut failures = Vec::new();
    let mut total = 0usize;
    for (rel, base) in suites {
        let dir = root.join(rel);
        let entries = load_manifest(&dir, rel, Some(base));
        assert!(!entries.is_empty(), "no entries parsed from Oxigraph {rel}");
        for entry in &entries {
            total += 1;
            if let Err(message) = run_entry(entry, true) {
                failures.push(format!("[{rel}] {}: {message}", entry.name));
            }
        }
    }
    assert!(
        failures.is_empty(),
        "{} of {total} Oxigraph RDF parser tests failed:\n{}",
        failures.len(),
        failures.join("\n")
    );
    println!("all {total} applicable Oxigraph RDF parser tests passed");
}

// -------------------------------------------------------- manifest loading

fn load_manifest(dir: &Path, rel: &str, base_override: Option<&str>) -> Vec<Entry> {
    let manifest = std::fs::read(dir.join("manifest.ttl")).expect("manifest readable");
    // Pass 1 with a placeholder base: find mf:assumedTestBase if declared.
    let quads = parse_turtle_all(&manifest, "http://manifest.local/manifest.ttl");
    let official_base = quads
        .iter()
        .find(|(_, p, _)| p == &format!("{MF}assumedTestBase"))
        .map(|(_, _, o)| iri_of(o).to_owned())
        .unwrap_or_else(|| {
            base_override
                .map(str::to_owned)
                .unwrap_or_else(|| format!("https://w3c.github.io/rdf-tests/rdf/{rel}/"))
        });
    // Pass 2 with the official base so action/result URLs come out real.
    let quads = parse_turtle_all(&manifest, &format!("{official_base}manifest.ttl"));

    let mut by_subject: HashMap<&str, Vec<(&str, &str)>> = HashMap::new();
    for (s, p, o) in &quads {
        by_subject.entry(s).or_default().push((p, o));
    }
    let mut entries = Vec::new();
    for (subject, props) in &by_subject {
        let Some(kind) = props.iter().find_map(|(p, o)| {
            (*p == RDF_TYPE)
                .then(|| o.strip_prefix(&format!(">{RDFT}")))
                .flatten()
        }) else {
            continue;
        };
        let format = if kind.starts_with("TestTurtle") {
            Format::Turtle
        } else if kind.starts_with("TestTrig") {
            Format::Trig
        } else if kind.starts_with("TestNTriples") {
            Format::NTriples
        } else if kind.starts_with("TestNQuads") {
            Format::NQuads
        } else {
            continue; // RDF/XML and implementation-specific parser modes.
        };
        let expect = match kind {
            k if k.ends_with("Eval") => Expect::Eval,
            k if k.ends_with("PositiveSyntax") => Expect::Positive,
            k if k.ends_with("NegativeSyntax") || k.ends_with("NegativeEval") => Expect::Negative,
            _ => continue,
        };
        let find_iri = |pred: &str| {
            props
                .iter()
                .find(|(p, _)| *p == format!("{MF}{pred}"))
                .map(|(_, o)| iri_of(o).to_owned())
        };
        let action_url = find_iri("action").expect("test entry has mf:action");
        let to_path = |url: &str| {
            dir.join(
                url.strip_prefix(official_base.as_str())
                    .unwrap_or_else(|| panic!("{url} not under {official_base}")),
            )
        };
        entries.push(Entry {
            name: subject
                .rsplit_once('#')
                .map(|(_, n)| n.to_owned())
                .unwrap_or_else(|| (*subject).to_owned()),
            format,
            expect,
            action_path: to_path(&action_url),
            result_path: find_iri("result").map(|u| to_path(&u)),
            action_url,
        });
    }
    entries.sort_by(|a, b| a.name.cmp(&b.name));
    entries
}

/// Parse Turtle bytes into (subject, predicate, object-key) string triples.
/// Terms render as `>iri`, `_label`, or the raw concise literal bytes.
fn parse_turtle_all(bytes: &[u8], base: &str) -> Vec<(String, String, String)> {
    let mut p = TurtleParser::new(Options {
        base: Some(base.to_owned()),
        ..Options::default()
    })
    .expect("valid base");
    let mut out = Vec::new();
    let mut push = |q: QuadRef<'_>| {
        let term = |b: &[u8]| String::from_utf8_lossy(b).into_owned();
        let pred = match q.predicate() {
            graphy_core::TermRef::Iri(i) => i.to_owned(),
            _ => return,
        };
        out.push((term(q.s), pred, term(q.o)));
    };
    p.feed(bytes).expect("manifest parses");
    for q in p.drain() {
        push(q);
    }
    p.finish().expect("manifest parses");
    for q in p.drain() {
        push(q);
    }
    out
}

/// Extract the IRI from a `>iri` term key.
fn iri_of(term: &str) -> &str {
    term.strip_prefix('>').expect("IRI object expected")
}

// ------------------------------------------------------------- test runner

fn run_entry(e: &Entry, spec12: bool) -> Result<(), String> {
    let data = std::fs::read(&e.action_path)
        .map_err(|err| format!("cannot read {}: {err}", e.action_path.display()))?;
    let opts = Options {
        base: Some(e.action_url.clone()),
        spec12,
        lenient: false,
        label_ns: None,
        trusted: false,
    };
    let parsed = parse_quads(&data, e.format, opts.clone());
    // Trusted mode must agree exactly with validating mode on every VALID
    // input (its contract). Negative tests are exempt — trusted mode may
    // accept or misparse invalid input instead of erroring.
    if let (Expect::Positive | Expect::Eval, Ok(quads)) = (e.expect, &parsed) {
        let trusted = parse_quads(
            &data,
            e.format,
            Options {
                trusted: true,
                ..opts
            },
        )
        .map_err(|err| format!("trusted-mode parse failed: {err}"))?;
        if &trusted != quads {
            return Err("trusted-mode parse differs from validating parse".to_owned());
        }
    }
    match (e.expect, parsed) {
        (Expect::Negative, Err(_)) => Ok(()),
        (Expect::Negative, Ok(_)) => Err("negative test parsed successfully".to_owned()),
        (Expect::Positive, Ok(_)) => Ok(()),
        (Expect::Positive | Expect::Eval, Err(err)) => Err(format!("parse failed: {err}")),
        (Expect::Eval, Ok(actual)) => {
            let result_path = e.result_path.as_ref().expect("eval test has mf:result");
            let expected_bytes = std::fs::read(result_path)
                .map_err(|err| format!("cannot read {}: {err}", result_path.display()))?;
            let result_format = match e.format {
                Format::Turtle | Format::NTriples => Format::NTriples,
                Format::Trig | Format::NQuads => Format::NQuads,
            };
            let expected = parse_quads(
                &expected_bytes,
                result_format,
                Options {
                    base: None,
                    spec12: true,
                    lenient: false,
                    label_ns: None,
                    trusted: false,
                },
            )
            .map_err(|err| format!("expected-result parse failed: {err}"))?;
            if isomorphic(&actual, &expected) {
                Ok(())
            } else {
                Err(format!(
                    "graphs not isomorphic ({} vs {} quads)",
                    actual.len(),
                    expected.len()
                ))
            }
        }
    }
}

/// Parse with 8 KiB feeds (exercising chunk resumption across the corpus).
fn parse_quads(data: &[u8], format: Format, opts: Options) -> Result<Vec<CQuad>, String> {
    macro_rules! drive {
        ($parser:expr) => {{
            let mut p = $parser.map_err(|e| e.to_string())?;
            let mut quads = Vec::new();
            for chunk in data.chunks(8192) {
                p.feed(chunk).map_err(|e| e.to_string())?;
                quads.extend(p.drain().map(cquad));
            }
            p.finish().map_err(|e| e.to_string())?;
            quads.extend(p.drain().map(cquad));
            // RDF graphs/datasets are sets: duplicate statements collapse.
            quads.sort_unstable();
            quads.dedup();
            Ok(quads)
        }};
    }
    match format {
        Format::Turtle => drive!(TurtleParser::new(opts)),
        Format::Trig => drive!(TriGParser::new(opts)),
        Format::NTriples => drive!(NTriplesParser::new(opts)),
        Format::NQuads => drive!(NQuadsParser::new(opts)),
    }
}

fn cquad(q: QuadRef<'_>) -> CQuad {
    (
        q.s.to_vec(),
        q.p.to_vec(),
        q.o.to_vec(),
        q.g.map(<[u8]>::to_vec),
    )
}

// ----------------------------------------------- blank-node isomorphism

/// Compare two quad multisets under blank-node bijection. Blank labels are
/// extracted from the concise bytes (including inside triple terms) and
/// mapped by color refinement + backtracking.
fn isomorphic(a: &[CQuad], b: &[CQuad]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let (qa, la) = index_bnodes(a);
    let (qb, lb) = index_bnodes(b);
    if la != lb {
        return false;
    }
    if la == 0 {
        return multiset(&qa) == multiset(&qb);
    }
    // Color refinement to cut the candidate space.
    let ca = refine(&qa, la);
    let cb = refine(&qb, lb);
    {
        let mut sa: Vec<u64> = ca.clone();
        let mut sb: Vec<u64> = cb.clone();
        sa.sort_unstable();
        sb.sort_unstable();
        if sa != sb {
            return false;
        }
    }
    let mut mapping = vec![usize::MAX; la];
    let mut used = vec![false; lb];
    backtrack(&qa, &qb, &ca, &cb, &mut mapping, &mut used, 0)
}

/// Quads with blank labels replaced by dense indices. Labels are found by
/// scanning the concise bytes for `_label` at term level and inside triple
/// terms; the parser's internal labels never collide with other syntax.
type IQuad = (IT, IT, IT, Option<IT>);

#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
enum IT {
    Ground(Vec<u8>),
    B(usize),
    /// Triple term with embedded blank nodes: skeleton bytes + bnode ids in
    /// order of appearance (placeholders spliced out).
    Tt(Vec<u8>, Vec<usize>),
}

fn index_bnodes(quads: &[CQuad]) -> (Vec<IQuad>, usize) {
    let mut labels: HashMap<Vec<u8>, usize> = HashMap::new();
    let mut conv = |bytes: &T| -> IT {
        match bytes.first() {
            Some(b'_') => {
                let n = labels.len();
                let id = *labels.entry(bytes[1..].to_vec()).or_insert(n);
                IT::B(id)
            }
            Some(0x09) => {
                // Triple term: replace each embedded blank label with a marker.
                let (skeleton, ids) = extract_tt_bnodes(bytes, &mut labels);
                if ids.is_empty() {
                    IT::Ground(bytes.clone())
                } else {
                    IT::Tt(skeleton, ids)
                }
            }
            _ => IT::Ground(bytes.clone()),
        }
    };
    let out: Vec<IQuad> = quads
        .iter()
        .map(|(s, p, o, g)| (conv(s), conv(p), conv(o), g.as_ref().map(&mut conv)))
        .collect();
    let n = labels.len();
    (out, n)
}

/// Walk a concise triple term, splicing out `_label` components and
/// recording their ids in order.
fn extract_tt_bnodes(bytes: &[u8], labels: &mut HashMap<Vec<u8>, usize>) -> (Vec<u8>, Vec<usize>) {
    // Concise triple term: 0x09 then 3 × (varint len, term bytes).
    fn read_varint(b: &[u8]) -> (u64, usize) {
        let mut v = 0u64;
        for (i, &x) in b.iter().enumerate() {
            v |= u64::from(x & 0x7F) << (7 * i);
            if x & 0x80 == 0 {
                return (v, i + 1);
            }
        }
        unreachable!("valid concise bytes")
    }
    let mut skeleton = vec![0x09];
    let mut ids = Vec::new();
    let mut at = 1;
    for _ in 0..3 {
        let (len, n) = read_varint(&bytes[at..]);
        at += n;
        let comp = &bytes[at..at + len as usize];
        at += len as usize;
        match comp.first() {
            Some(b'_') => {
                let next = labels.len();
                let id = *labels.entry(comp[1..].to_vec()).or_insert(next);
                ids.push(id);
                skeleton.extend_from_slice(b"\x01B"); // placeholder marker
            }
            Some(0x09) => {
                let (inner_skel, inner_ids) = extract_tt_bnodes(comp, labels);
                if inner_ids.is_empty() {
                    skeleton.push(1);
                    skeleton.extend_from_slice(comp);
                } else {
                    ids.extend(inner_ids);
                    skeleton.push(2);
                    skeleton.extend_from_slice(&inner_skel);
                }
            }
            _ => {
                skeleton.push(1);
                skeleton.extend_from_slice(comp);
            }
        }
    }
    (skeleton, ids)
}

fn multiset(quads: &[IQuad]) -> HashMap<&IQuad, usize> {
    let mut m = HashMap::new();
    for q in quads {
        *m.entry(q).or_insert(0) += 1;
    }
    m
}

/// Two rounds of color refinement over bnode ids.
fn refine(quads: &[IQuad], n: usize) -> Vec<u64> {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut colors = vec![0u64; n];
    for _round in 0..3 {
        let mut acc: Vec<Vec<u64>> = vec![Vec::new(); n];
        for q in quads {
            let sig = |t: &IT, colors: &[u64]| -> u64 {
                let mut h = DefaultHasher::new();
                match t {
                    IT::Ground(g) => g.hash(&mut h),
                    IT::B(i) => colors[*i].hash(&mut h),
                    IT::Tt(skel, ids) => {
                        skel.hash(&mut h);
                        for i in ids {
                            colors[*i].hash(&mut h);
                        }
                    }
                }
                h.finish()
            };
            let quad_sig = {
                let mut h = DefaultHasher::new();
                sig(&q.0, &colors).hash(&mut h);
                sig(&q.1, &colors).hash(&mut h);
                sig(&q.2, &colors).hash(&mut h);
                q.3.as_ref().map(|g| sig(g, &colors)).hash(&mut h);
                h.finish()
            };
            let mut touch = |t: &IT, pos: u64| {
                let mut record = |i: usize| {
                    let mut h = DefaultHasher::new();
                    pos.hash(&mut h);
                    quad_sig.hash(&mut h);
                    acc[i].push(h.finish());
                };
                match t {
                    IT::B(i) => record(*i),
                    IT::Tt(_, ids) => ids.iter().for_each(|&i| record(i)),
                    IT::Ground(_) => {}
                }
            };
            touch(&q.0, 0);
            touch(&q.1, 1);
            touch(&q.2, 2);
            if let Some(g) = &q.3 {
                touch(g, 3);
            }
        }
        for i in 0..n {
            use std::hash::{Hash, Hasher};
            let mut h = DefaultHasher::new();
            colors[i].hash(&mut h);
            acc[i].sort_unstable();
            acc[i].hash(&mut h);
            colors[i] = h.finish();
        }
    }
    colors
}

/// Map bnode `i` (in a) to candidates in b sharing its color; verify the
/// full multiset equality at complete assignment.
fn backtrack(
    qa: &[IQuad],
    qb: &[IQuad],
    ca: &[u64],
    cb: &[u64],
    mapping: &mut [usize],
    used: &mut [bool],
    i: usize,
) -> bool {
    if i == mapping.len() {
        let mapped: Vec<IQuad> = qa.iter().map(|q| apply(q, mapping)).collect();
        return multiset(&mapped) == multiset(qb);
    }
    for cand in 0..used.len() {
        if !used[cand] && cb[cand] == ca[i] {
            mapping[i] = cand;
            used[cand] = true;
            if backtrack(qa, qb, ca, cb, mapping, used, i + 1) {
                return true;
            }
            used[cand] = false;
            mapping[i] = usize::MAX;
        }
    }
    false
}

fn apply(q: &IQuad, mapping: &[usize]) -> IQuad {
    let m = |t: &IT| match t {
        IT::B(i) => IT::B(mapping[*i]),
        IT::Tt(s, ids) => IT::Tt(s.clone(), ids.iter().map(|&i| mapping[i]).collect()),
        IT::Ground(g) => IT::Ground(g.clone()),
    };
    (m(&q.0), m(&q.1), m(&q.2), q.3.as_ref().map(m))
}
