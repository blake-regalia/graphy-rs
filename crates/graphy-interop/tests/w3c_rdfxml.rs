//! W3C rdf-tests conformance for the RDF/XML codec: drives the rdf11/rdf-xml manifest
//! (eval tests compared under blank-node isomorphism, negative-syntax tests must be
//! rejected). Requires the shallow clone at `graphy-rs/testdata/rdf-tests`; skipped when
//! absent. XML literals are compared in their required canonical form.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use graphy_interop::{parse_rdfxml, Triple};
use graphy_turtle::{NTriplesParser, Options, TurtleParser};

#[path = "../../../test-support/oracles.rs"]
mod oracles;

const RDF_TYPE: &str = "http://www.w3.org/1999/02/22-rdf-syntax-ns#type";
const MF: &str = "http://www.w3.org/2001/sw/DataAccess/tests/test-manifest#";
const RDFT: &str = "http://www.w3.org/ns/rdftest#";

#[derive(Debug, Clone, Copy, PartialEq)]
enum Expect {
    Eval,
    Negative,
}

struct Entry {
    name: String,
    expect: Expect,
    action_url: String,
    action_path: PathBuf,
    result_path: Option<PathBuf>,
}

fn suite_root() -> Option<PathBuf> {
    let root =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../testdata/rdf-tests/rdf/rdf11/rdf-xml");
    root.is_dir().then_some(root)
}

#[test]
fn w3c_rdfxml_tests() {
    let Some(dir) = suite_root() else {
        eprintln!("skipping: testdata/rdf-tests not present");
        return;
    };
    let entries = load_manifest(&dir, None);
    assert!(!entries.is_empty(), "no entries parsed from the manifest");

    let mut failures = Vec::new();
    let mut total = 0usize;
    for entry in &entries {
        total += 1;
        if let Err(message) = run_entry(entry) {
            failures.push(format!("{}: {message}", entry.name));
        }
    }
    if !failures.is_empty() {
        panic!(
            "{} of {total} W3C rdf-xml tests failed:\n{}",
            failures.len(),
            failures.join("\n")
        );
    }
    println!("all {total} W3C rdf-xml tests passed");
}

#[test]
fn oxigraph_rdfxml_tests() {
    let Some(root) = oracles::checkout("oxigraph") else {
        return;
    };
    let dir = root.join("testsuite/oxigraph-tests/parser");
    let entries = load_manifest(
        &dir,
        Some("https://github.com/oxigraph/oxigraph/tests/parser/"),
    );
    assert!(!entries.is_empty(), "no Oxigraph RDF/XML entries parsed");

    let mut failures = Vec::new();
    for entry in &entries {
        if let Err(message) = run_entry(entry) {
            failures.push(format!("{}: {message}", entry.name));
        }
    }
    assert!(
        failures.is_empty(),
        "{} of {} Oxigraph RDF/XML tests failed:\n{}",
        failures.len(),
        entries.len(),
        failures.join("\n")
    );
    println!("all {} Oxigraph RDF/XML tests passed", entries.len());
}

fn run_entry(entry: &Entry) -> Result<(), String> {
    let bytes = std::fs::read(&entry.action_path)
        .map_err(|e| format!("cannot read {}: {e}", entry.action_path.display()))?;
    let source = String::from_utf8_lossy(&bytes);
    let parsed = parse_rdfxml(&source, Some(&entry.action_url));
    match (entry.expect, parsed) {
        (Expect::Negative, Err(_)) => Ok(()),
        (Expect::Negative, Ok(_)) => Err("negative test parsed successfully".to_string()),
        (Expect::Eval, Err(e)) => Err(format!("parse failed: {e}")),
        (Expect::Eval, Ok(actual)) => {
            let result_path = entry.result_path.as_ref().expect("eval test has a result");
            let expected_bytes = std::fs::read(result_path)
                .map_err(|e| format!("cannot read {}: {e}", result_path.display()))?;
            let expected = parse_ntriples(&expected_bytes)?;
            if isomorphic(&actual, &expected) {
                Ok(())
            } else {
                Err(format!(
                    "graphs not isomorphic ({} vs {} triples)",
                    actual.len(),
                    expected.len()
                ))
            }
        }
    }
}

// ---------------------------------------------------------------- manifest

fn load_manifest(dir: &Path, base_override: Option<&str>) -> Vec<Entry> {
    let manifest = std::fs::read(dir.join("manifest.ttl")).expect("manifest readable");
    let quads = parse_turtle_all(&manifest, "http://manifest.local/manifest.ttl");
    let official_base = quads
        .iter()
        .find(|(_, p, _)| p == &format!("{MF}assumedTestBase"))
        .map(|(_, _, o)| o.strip_prefix('>').unwrap().to_string())
        .unwrap_or_else(|| {
            base_override
                .unwrap_or("https://w3c.github.io/rdf-tests/rdf/rdf11/rdf-xml/")
                .to_string()
        });
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
        let expect = match kind {
            "TestXMLEval" => Expect::Eval,
            "TestXMLNegativeSyntax" => Expect::Negative,
            _ => continue,
        };
        let find_iri = |predicate: &str| {
            props
                .iter()
                .find(|(p, _)| *p == format!("{MF}{predicate}"))
                .map(|(_, o)| o.strip_prefix('>').unwrap().to_string())
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
                .map(|(_, n)| n.to_string())
                .unwrap_or_else(|| (*subject).to_string()),
            expect,
            action_path: to_path(&action_url),
            result_path: find_iri("result").map(|u| to_path(&u)),
            action_url,
        });
    }
    entries.sort_by(|a, b| a.name.cmp(&b.name));
    entries
}

fn parse_turtle_all(bytes: &[u8], base: &str) -> Vec<(String, String, String)> {
    let mut parser = TurtleParser::new(Options {
        base: Some(base.to_string()),
        ..Options::default()
    })
    .expect("valid base");
    let mut out = Vec::new();
    parser.feed(bytes).expect("manifest parses");
    for quad in parser.drain() {
        out.push((
            String::from_utf8_lossy(quad.s).into_owned(),
            String::from_utf8_lossy(quad.p)
                .strip_prefix('>')
                .unwrap_or("")
                .to_string(),
            String::from_utf8_lossy(quad.o).into_owned(),
        ));
    }
    parser.finish().expect("manifest parses");
    for quad in parser.drain() {
        out.push((
            String::from_utf8_lossy(quad.s).into_owned(),
            String::from_utf8_lossy(quad.p)
                .strip_prefix('>')
                .unwrap_or("")
                .to_string(),
            String::from_utf8_lossy(quad.o).into_owned(),
        ));
    }
    out
}

fn parse_ntriples(bytes: &[u8]) -> Result<Vec<Triple>, String> {
    let mut parser = NTriplesParser::new(Options::default()).map_err(|e| e.to_string())?;
    let mut out = Vec::new();
    parser.feed(bytes).map_err(|e| e.to_string())?;
    for quad in parser.drain() {
        out.push(Triple {
            s: quad.s.to_vec(),
            p: quad.p.to_vec(),
            o: quad.o.to_vec(),
        });
    }
    parser.finish().map_err(|e| e.to_string())?;
    for quad in parser.drain() {
        out.push(Triple {
            s: quad.s.to_vec(),
            p: quad.p.to_vec(),
            o: quad.o.to_vec(),
        });
    }
    Ok(out)
}

// ------------------------------------------------------------- isomorphism

/// Blank-node-respecting graph isomorphism via backtracking with signature pruning
/// (the suite's graphs are small; no triple terms in RDF/XML).
fn isomorphic(a: &[Triple], b: &[Triple]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let (qa, la) = index(a);
    let (qb, lb) = index(b);
    if la != lb {
        return false;
    }
    if la == 0 {
        let mut sa = qa.clone();
        let mut sb = qb.clone();
        sa.sort();
        sb.sort();
        return sa == sb;
    }
    let mut mapping = vec![usize::MAX; la];
    let mut used = vec![false; lb];
    backtrack(&qa, &qb, &mut mapping, &mut used, 0)
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum Term {
    Ground(Vec<u8>),
    Blank(usize),
}

type ITriple = (Term, Term, Term);

fn index(triples: &[Triple]) -> (Vec<ITriple>, usize) {
    let mut labels: HashMap<Vec<u8>, usize> = HashMap::new();
    let mut convert = |bytes: &Vec<u8>| -> Term {
        if bytes.first() == Some(&b'_') {
            let next = labels.len();
            Term::Blank(*labels.entry(bytes[1..].to_vec()).or_insert(next))
        } else {
            Term::Ground(bytes.clone())
        }
    };
    let out = triples
        .iter()
        .map(|t| (convert(&t.s), convert(&t.p), convert(&t.o)))
        .collect();
    (out, labels.len())
}

fn substitute(term: &Term, mapping: &[usize]) -> Term {
    match term {
        Term::Blank(id) if mapping[*id] != usize::MAX => Term::Blank(mapping[*id]),
        other => other.clone(),
    }
}

fn backtrack(
    a: &[ITriple],
    b: &[ITriple],
    mapping: &mut [usize],
    used: &mut [bool],
    next: usize,
) -> bool {
    if next == mapping.len() {
        let mut mapped: Vec<ITriple> = a
            .iter()
            .map(|(s, p, o)| {
                (
                    substitute(s, mapping),
                    substitute(p, mapping),
                    substitute(o, mapping),
                )
            })
            .collect();
        let mut expected = b.to_vec();
        mapped.sort();
        expected.sort();
        return mapped == expected;
    }
    for candidate in 0..used.len() {
        if used[candidate] {
            continue;
        }
        mapping[next] = candidate;
        used[candidate] = true;
        if backtrack(a, b, mapping, used, next + 1) {
            return true;
        }
        mapping[next] = usize::MAX;
        used[candidate] = false;
    }
    false
}
