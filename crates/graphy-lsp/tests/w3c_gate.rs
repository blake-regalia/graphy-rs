//! M11b exit gate (docs/10 §14) over the vendored W3C rdf-tests corpus:
//!
//! 1. **Diagnostics match a fail-fast full parse**: every `.ttl`/`.trig` file
//!    that strict-parses (as TriG — the LSP's dialect collapse) publishes
//!    zero Error diagnostics; every file that fails strict parsing publishes
//!    at least one localized error.
//! 2. **Format round-trips**: for every parseable file with data, the
//!    canonical pretty-print re-parses to the same distinct quad set
//!    (blank-node documents compare distinct counts — labels are
//!    regenerated), and formatting its own output is idempotent —
//!    byte-identical for bnode-free documents, identical modulo blank-node
//!    label renaming otherwise (label-stable canonical output is MC C4
//!    graphy-canon territory, not the formatter's).
//!
//! Skips silently when `testdata/rdf-tests` (gitignored clone) is absent.

use std::path::{Path, PathBuf};

use graphy_lsp::{turtle_diagnostics, turtle_pretty, Sev};
use graphy_turtle::{Options, TriGParser};

fn corpus_root() -> Option<PathBuf> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../testdata/rdf-tests");
    root.is_dir().then_some(root)
}

fn corpus_files(root: &Path, out: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(root).expect("readable corpus dir") {
        let path = entry.expect("dir entry").path();
        if path.is_dir() {
            corpus_files(&path, out);
        } else if matches!(
            path.extension().and_then(|e| e.to_str()),
            Some("ttl" | "trig")
        ) {
            out.push(path);
        }
    }
}

/// Rewrite every `_:label` to a first-occurrence ordinal, so two serializations
/// that differ only by blank-node label choice compare equal.
fn norm_labels(s: &str) -> String {
    let mut map: std::collections::HashMap<&str, usize> = std::collections::HashMap::new();
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(at) = rest.find("_:") {
        out.push_str(&rest[..at + 2]);
        rest = &rest[at + 2..];
        let end = rest
            .find(|c: char| !(c.is_alphanumeric() || c == '_' || c == '-'))
            .unwrap_or(rest.len());
        let next = map.len();
        let n = *map.entry(&rest[..end]).or_insert(next);
        out.push_str(&format!("x{n}"));
        rest = &rest[end..];
    }
    out.push_str(rest);
    out
}

/// Strict TriG parse: `Some((distinct quad rows, any_blank_nodes))` or `None`.
fn strict_parse(src: &str, base: &str) -> Option<(Vec<Vec<u8>>, bool)> {
    let mut p = TriGParser::new(Options {
        base: Some(base.to_string()),
        ..Options::default()
    })
    .ok()?;
    let mut rows = std::collections::BTreeSet::new();
    let mut blanks = false;
    p.read_from(src.as_bytes(), |q| {
        blanks |= q.s.first() == Some(&b'_')
            || q.o.first() == Some(&b'_')
            || q.g.is_some_and(|g| g.first() == Some(&b'_'));
        let mut row = q.s.to_vec();
        row.push(0);
        row.extend_from_slice(q.p);
        row.push(0);
        row.extend_from_slice(q.o);
        row.push(0);
        row.extend_from_slice(q.g.unwrap_or(b""));
        rows.insert(row);
    })
    .ok()?;
    Some((rows.into_iter().collect(), blanks))
}

#[test]
fn w3c_corpus_gate() {
    let Some(root) = corpus_root() else {
        eprintln!("testdata/rdf-tests absent; gate skipped");
        return;
    };
    let mut files = Vec::new();
    corpus_files(&root, &mut files);
    assert!(
        files.len() > 1000,
        "suspiciously small corpus: {}",
        files.len()
    );

    let (mut parsed, mut rejected, mut formatted) = (0u32, 0u32, 0u32);
    for path in &files {
        let Ok(src) = std::fs::read_to_string(path) else {
            continue; // non-UTF-8 negative fixtures are not LSP inputs
        };
        let base = format!("file://{}", path.display());
        let strict = strict_parse(&src, &base);
        let errors = turtle_diagnostics(&src, Some(&base))
            .into_iter()
            .filter(|d| d.sev == Sev::Error)
            .count();

        match strict {
            Some((quads, blanks)) => {
                parsed += 1;
                assert_eq!(
                    errors,
                    0,
                    "{}: strict parse succeeds but diagnostics report {errors} error(s)",
                    path.display()
                );
                if quads.is_empty() {
                    continue; // nothing to canonicalize
                }
                let out = turtle_pretty(&src, Some(&base)).unwrap_or_else(|| {
                    panic!("{}: parseable doc refused to format", path.display())
                });
                let (requads, _) = strict_parse(&out, &base).unwrap_or_else(|| {
                    panic!(
                        "{}: pretty output does not re-parse:\n{out}",
                        path.display()
                    )
                });
                if blanks {
                    // Labels are regenerated; distinct counts must agree.
                    assert_eq!(requads.len(), quads.len(), "{}", path.display());
                } else {
                    assert_eq!(requads, quads, "{}", path.display());
                }
                let again = turtle_pretty(&out, Some(&base)).unwrap_or_else(|| {
                    panic!("{}: pretty output refused to re-format", path.display())
                });
                if blanks {
                    // Labels renumber under reordering; structure must not.
                    assert_eq!(
                        norm_labels(&again),
                        norm_labels(&out),
                        "{}: formatting is not idempotent modulo labels",
                        path.display()
                    );
                } else {
                    assert_eq!(
                        again,
                        out,
                        "{}: formatting is not idempotent",
                        path.display()
                    );
                }
                formatted += 1;
            }
            None => {
                rejected += 1;
                assert!(
                    errors > 0,
                    "{}: strict parse fails but diagnostics are silent",
                    path.display()
                );
            }
        }
    }
    eprintln!(
        "w3c gate: {} files — {parsed} parsed clean, {formatted} formatted+round-tripped, {rejected} rejected with localized errors",
        files.len()
    );
}
