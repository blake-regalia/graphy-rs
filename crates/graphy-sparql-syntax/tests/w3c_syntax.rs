//! W3C SPARQL syntax suites (doc 04 §5): positive tests must parse,
//! negative tests must be rejected. Manifests are Turtle, read through
//! graphy-turtle; the sparql11 query+update suites and the sparql12
//! syntax suites run. Skips silently when `testdata/rdf-tests` is absent
//! (gitignored checkout — clone https://github.com/w3c/rdf-tests).

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use graphy_sparql_syntax::{parse_query, parse_update};
use graphy_turtle::{Options, TurtleParser};

#[path = "../../../test-support/oracles.rs"]
mod oracles;

const MF: &str = "http://www.w3.org/2001/sw/DataAccess/tests/test-manifest#";
const RDF: &str = "http://www.w3.org/1999/02/22-rdf-syntax-ns#";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    PositiveQuery,
    NegativeQuery,
    PositiveUpdate,
    NegativeUpdate,
}

fn kind_of(type_iri: &str) -> Option<Kind> {
    match type_iri.rsplit('#').next()? {
        "PositiveSyntaxTest" | "PositiveSyntaxTest11" => Some(Kind::PositiveQuery),
        "NegativeSyntaxTest" | "NegativeSyntaxTest11" => Some(Kind::NegativeQuery),
        "PositiveUpdateSyntaxTest" | "PositiveUpdateSyntaxTest11" => Some(Kind::PositiveUpdate),
        "NegativeUpdateSyntaxTest" | "NegativeUpdateSyntaxTest11" => Some(Kind::NegativeUpdate),
        _ => None,
    }
}

/// Predicate–object pairs (concise term bytes).
type PoList = Vec<(Vec<u8>, Vec<u8>)>;

/// Minimal triple index over one manifest file: concise subject → (p, o).
struct Graph {
    spo: HashMap<Vec<u8>, PoList>,
}

impl Graph {
    fn load(path: &Path) -> Graph {
        let src = std::fs::read(path).expect("read manifest");
        let opts = Options {
            base: Some(format!("file://{}", path.display())),
            ..Options::default()
        };
        let mut spo: HashMap<Vec<u8>, PoList> = HashMap::new();
        let mut p = TurtleParser::new(opts).expect("parser options");
        let mut sink = |q: graphy_turtle::QuadRef<'_>| {
            spo.entry(q.s.to_vec())
                .or_default()
                .push((q.p.to_vec(), q.o.to_vec()));
        };
        p.read_from(&src[..], &mut sink)
            .unwrap_or_else(|e| panic!("parse {}: {e}", path.display()));
        Graph { spo }
    }

    fn object(&self, s: &[u8], p: &str) -> Option<&[u8]> {
        self.spo
            .get(s)?
            .iter()
            .find(|(pp, _)| pp == format!(">{p}").as_bytes())
            .map(|(_, o)| o.as_slice())
    }

    /// Walk an rdf:List from its head, yielding member terms.
    fn list(&self, head: &[u8]) -> Vec<Vec<u8>> {
        let mut out = Vec::new();
        let mut cur = head.to_vec();
        loop {
            if cur == format!(">{RDF}nil").as_bytes() {
                return out;
            }
            let Some(first) = self.object(&cur, &format!("{RDF}first")) else {
                return out;
            };
            out.push(first.to_vec());
            match self.object(&cur, &format!("{RDF}rest")) {
                Some(rest) => cur = rest.to_vec(),
                None => return out,
            }
        }
    }
}

/// Concise IRI term → path (strips `>file://`).
fn iri_path(term: &[u8]) -> PathBuf {
    let s = std::str::from_utf8(term).expect("utf8 term");
    let iri = s.strip_prefix('>').expect("IRI term");
    PathBuf::from(iri.strip_prefix("file://").expect("file IRI"))
}

struct Outcome {
    ran: usize,
    excluded: usize,
    exclusion_hits: Vec<usize>,
    failures: Vec<String>,
}

fn run_manifest(path: &Path, exclusions: &[(&str, &str)], outcome: &mut Outcome) {
    let g = Graph::load(path);
    // The manifest node may be `<>` or a named node typed mf:Manifest.
    let manifest_type = format!(">{MF}Manifest");
    let manifest_node = g
        .spo
        .iter()
        .find(|(_, pos)| {
            pos.iter().any(|(p, o)| {
                p == format!(">{RDF}type").as_bytes() && o == manifest_type.as_bytes()
            })
        })
        .map(|(s, _)| s.clone())
        .unwrap_or_else(|| panic!("{}: no mf:Manifest node", path.display()));
    let entries_head = g
        .object(&manifest_node, &format!("{MF}entries"))
        .unwrap_or_else(|| panic!("{}: no mf:entries", path.display()));
    for entry in g.list(entries_head) {
        let Some(type_o) = g.object(&entry, &format!("{RDF}type")) else {
            continue;
        };
        let type_iri = std::str::from_utf8(type_o).unwrap();
        let Some(kind) = kind_of(type_iri.strip_prefix('>').unwrap_or(type_iri)) else {
            continue; // evaluation tests etc. — other milestones
        };
        let action = g
            .object(&entry, &format!("{MF}action"))
            .expect("syntax test has mf:action");
        let file = iri_path(action);
        let src = std::fs::read_to_string(&file)
            .unwrap_or_else(|e| panic!("read {}: {e}", file.display()));
        let name = file
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        if let Some((index, (_, reason))) = exclusions
            .iter()
            .enumerate()
            .find(|(_, (file, _))| *file == name)
        {
            outcome.exclusion_hits[index] += 1;
            outcome.excluded += 1;
            eprintln!("excluded {name}: {reason}");
            continue;
        }
        outcome.ran += 1;
        match kind {
            Kind::PositiveQuery => {
                if let Err(e) = parse_query(&src) {
                    outcome
                        .failures
                        .push(format!("{name}: expected parse, got: {e}"));
                }
            }
            Kind::NegativeQuery => {
                if parse_query(&src).is_ok() {
                    outcome
                        .failures
                        .push(format!("{name}: parsed but must be rejected"));
                }
            }
            Kind::PositiveUpdate => {
                if let Err(e) = parse_update(&src) {
                    outcome
                        .failures
                        .push(format!("{name}: expected parse, got: {e}"));
                }
            }
            Kind::NegativeUpdate => {
                if parse_update(&src).is_ok() {
                    outcome
                        .failures
                        .push(format!("{name}: parsed but must be rejected"));
                }
            }
        }
    }
}

fn suite_root() -> Option<PathBuf> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../testdata/rdf-tests/sparql");
    root.exists()
        .then(|| root.canonicalize().expect("canonical"))
}

fn run_manifest_paths(manifests: &[PathBuf], exclusions: &[(&str, &str)]) -> Outcome {
    let mut outcome = Outcome {
        ran: 0,
        excluded: 0,
        exclusion_hits: vec![0; exclusions.len()],
        failures: Vec::new(),
    };
    for path in manifests {
        if !path.exists() {
            panic!("manifest missing: {}", path.display());
        }
        run_manifest(path, exclusions, &mut outcome);
    }
    for ((file, _), hits) in exclusions.iter().zip(&outcome.exclusion_hits) {
        assert_eq!(
            *hits, 1,
            "exclusion `{file}` must match exactly one test, matched {hits}"
        );
    }
    outcome
}

fn run_suites(manifests: &[&str]) -> Outcome {
    let Some(root) = suite_root() else {
        eprintln!("skipping: testdata/rdf-tests not present");
        return Outcome {
            ran: 0,
            excluded: 0,
            exclusion_hits: Vec::new(),
            failures: Vec::new(),
        };
    };
    let manifests: Vec<PathBuf> = manifests.iter().map(|rel| root.join(rel)).collect();
    run_manifest_paths(&manifests, &[])
}

const OXIGRAPH_EXCLUDED: &[(&str, &str)] = &[
    (
        "nested_path.rq",
        "exceeds Graphy's defensive parser nesting limit",
    ),
    (
        "nested_expression.rq",
        "exceeds Graphy's defensive parser nesting limit",
    ),
    (
        "port_local_name.rq",
        "Oxigraph applies URL-scheme port validation beyond the RFC 3987 IRI grammar",
    ),
];

#[test]
fn oxigraph_sparql_syntax() {
    let Some(root) = oracles::checkout("oxigraph") else {
        return;
    };
    let o = run_manifest_paths(
        &[root.join("testsuite/oxigraph-tests/sparql/manifest.ttl")],
        OXIGRAPH_EXCLUDED,
    );
    assert!(
        o.failures.is_empty(),
        "{} of {} failed:\n{}",
        o.failures.len(),
        o.ran,
        o.failures.join("\n")
    );
    println!(
        "Oxigraph SPARQL syntax: {} tests green ({} excluded)",
        o.ran, o.excluded
    );
}

#[test]
fn sparql11_syntax_suites() {
    let o = run_suites(&[
        "sparql11/syntax-query/manifest.ttl",
        "sparql11/syntax-update-1/manifest.ttl",
        "sparql11/syntax-update-2/manifest.ttl",
        "sparql11/syntax-fed/manifest.ttl",
    ]);
    assert!(
        o.failures.is_empty(),
        "{} of {} failed:\n{}",
        o.failures.len(),
        o.ran,
        o.failures.join("\n")
    );
    println!("sparql11 syntax: {} tests green", o.ran);
}

#[test]
fn sparql10_syntax_suites() {
    let o = run_suites(&[
        "sparql10/syntax-sparql1/manifest.ttl",
        "sparql10/syntax-sparql2/manifest.ttl",
        "sparql10/syntax-sparql3/manifest.ttl",
        "sparql10/syntax-sparql4/manifest.ttl",
        "sparql10/syntax-sparql5/manifest.ttl",
    ]);
    assert!(
        o.failures.is_empty(),
        "{} of {} failed:\n{}",
        o.failures.len(),
        o.ran,
        o.failures.join("\n")
    );
    println!("sparql10 syntax: {} tests green", o.ran);
}

#[test]
fn sparql12_syntax_suites() {
    let o = run_suites(&[
        "sparql12/syntax/manifest.ttl",
        "sparql12/syntax-triple-terms-positive/manifest.ttl",
        "sparql12/syntax-triple-terms-negative/manifest.ttl",
        "sparql12/codepoint-escapes/manifest.ttl",
        "sparql12/lang-basedir/manifest.ttl",
        "sparql12/version/manifest.ttl",
    ]);
    assert!(
        o.failures.is_empty(),
        "{} of {} failed:\n{}",
        o.failures.len(),
        o.ran,
        o.failures.join("\n")
    );
    println!("sparql12 syntax: {} tests green", o.ran);
}
