//! Data-parallel NT/NQ parsing tests (doc 03 §4.1): thread-count
//! invariance, agreement with the serial parser, blank-label consistency,
//! global error positions, and the trusted-mode combination.

use std::sync::Mutex;

use graphy_turtle::{par, NQuadsParser, Options, QuadRef};

type CQuad = (Vec<u8>, Vec<u8>, Vec<u8>, Option<Vec<u8>>);

fn cquad(q: QuadRef<'_>) -> CQuad {
    (
        q.s.to_vec(),
        q.p.to_vec(),
        q.o.to_vec(),
        q.g.map(<[u8]>::to_vec),
    )
}

/// A corpus exercising every NQ term kind, blank nodes reused across distant
/// statements, comments, blank lines, and duplicate statements.
fn corpus(statements: usize) -> String {
    use std::fmt::Write as _;
    let mut s = String::new();
    s.push_str("# leading comment\n\n");
    for i in 0..statements {
        match i % 6 {
            0 => writeln!(
                s,
                "<http://x/s{i}> <http://x/p{}> <http://x/o{}> .",
                i % 13,
                i * 7 % statements
            ),
            1 => writeln!(
                s,
                "<http://x/s{i}> <http://x/label> \"value {i} with text\" <http://x/g{}> .",
                i % 3
            ),
            2 => writeln!(s, "_:n{} <http://x/link> _:n{} .", i % 50, (i + 1) % 50),
            3 => writeln!(
                s,
                "<http://x/s{i}> <http://x/c> \"{i}\"^^<http://www.w3.org/2001/XMLSchema#integer> ."
            ),
            4 => writeln!(s, "<http://x/s{i}> <http://x/t> \"étiquette {i}\"@fr ."),
            _ => writeln!(
                s,
                "<http://x/s{i}> <http://x/e> \"esc\\t\\\"q\\\" {i}\" <http://x/g0> ."
            ),
        }
        .expect("write");
        if i % 97 == 0 {
            s.push_str("# interior comment\n\n");
        }
    }
    s
}

/// Run the parallel parser, collecting per-segment then flattening in
/// segment (= document) order.
fn par_quads(data: &[u8], options: &Options, threads: usize) -> Result<Vec<CQuad>, String> {
    let buckets: Vec<Mutex<Vec<CQuad>>> = (0..threads.max(1))
        .map(|_| Mutex::new(Vec::new()))
        .collect();
    par::nquads(data, options, threads, |seg, q| {
        buckets[seg].lock().unwrap().push(cquad(q));
    })
    .map_err(|e| e.to_string())?;
    Ok(buckets
        .into_iter()
        .flat_map(|b| b.into_inner().unwrap())
        .collect())
}

fn serial_quads(data: &[u8], options: &Options) -> Vec<CQuad> {
    let mut p = NQuadsParser::new(options.clone()).unwrap();
    let mut out = Vec::new();
    for chunk in data.chunks(8192) {
        p.feed(chunk).unwrap();
        out.extend(p.drain().map(cquad));
    }
    p.finish().unwrap();
    out.extend(p.drain().map(cquad));
    out
}

#[test]
fn thread_count_invariance() {
    let data = corpus(3000);
    let opts = Options::default();
    let reference = par_quads(data.as_bytes(), &opts, 1).unwrap();
    assert!(!reference.is_empty());
    for threads in [2, 3, 4, 7, 8, 64] {
        let got = par_quads(data.as_bytes(), &opts, threads).unwrap();
        assert_eq!(got, reference, "threads={threads}");
    }
    // Degenerate splits: more threads than lines, tiny input.
    let tiny = "<http://x/a> <http://x/p> <http://x/b> .\n";
    assert_eq!(par_quads(tiny.as_bytes(), &opts, 32).unwrap().len(), 1);
    assert!(par_quads(b"", &opts, 8).unwrap().is_empty());
}

#[test]
fn agrees_with_serial_parse() {
    let data = corpus(2000);
    let opts = Options::default();
    let serial = serial_quads(data.as_bytes(), &opts);
    let parallel = par_quads(data.as_bytes(), &opts, 8).unwrap();
    assert_eq!(serial.len(), parallel.len());

    // Quads agree in document order; blank labels differ by convention
    // (serial: first-seen `_b{n}`; parallel: content-derived `_s{surface}`).
    // Aligning in order must yield a consistent bijection between the two
    // label namespaces — that is exactly isomorphism for ground-or-labeled
    // datasets with a 1:1 statement alignment.
    let mut fwd = std::collections::HashMap::new();
    let mut bwd = std::collections::HashMap::new();
    let mut check = |a: &[u8], b: &[u8]| {
        if a.first() == Some(&b'_') {
            assert_eq!(b.first(), Some(&b'_'), "term kind mismatch");
            let f = fwd.entry(a.to_vec()).or_insert_with(|| b.to_vec());
            assert_eq!(f.as_slice(), b, "label bijection broken (fwd)");
            let g = bwd.entry(b.to_vec()).or_insert_with(|| a.to_vec());
            assert_eq!(g.as_slice(), a, "label bijection broken (bwd)");
        } else {
            assert_eq!(a, b, "non-blank terms must be identical");
        }
    };
    for (s, p) in serial.iter().zip(parallel.iter()) {
        check(&s.0, &p.0);
        check(&s.1, &p.1);
        check(&s.2, &p.2);
        match (&s.3, &p.3) {
            (None, None) => {}
            (Some(a), Some(b)) => check(a, b),
            _ => panic!("graph presence mismatch"),
        }
    }
}

#[test]
fn trusted_mode_composes() {
    let data = corpus(1500);
    let validated = par_quads(data.as_bytes(), &Options::default(), 6).unwrap();
    let trusted = par_quads(
        data.as_bytes(),
        &Options {
            trusted: true,
            ..Options::default()
        },
        6,
    )
    .unwrap();
    assert_eq!(validated, trusted);
}

#[test]
fn errors_report_global_positions() {
    // Valid statements, then a bad IRI deep in the file (segment > 0 for
    // any thread count ≥ 2), then more valid data.
    let mut data = corpus(1000);
    let line = data.lines().count() + 1;
    let offset = data.len() as u64;
    data.push_str("<http://x/bad iri> <http://x/p> <http://x/o> .\n");
    data.push_str(&corpus(1000));

    for threads in [1, 4, 8] {
        let err = par_quads(data.as_bytes(), &Options::default(), threads).unwrap_err();
        assert!(
            err.contains(&format!("{line}:")),
            "threads={threads}: expected line {line} in {err:?}"
        );
        // The offset of the space inside the IRI.
        let space = offset + "<http://x/bad".len() as u64;
        assert!(
            err.contains(&format!("byte {space}")),
            "threads={threads}: expected byte {space} in {err:?}"
        );
    }
}

#[test]
fn ntriples_variant() {
    let data = "<http://x/a> <http://x/p> _:b .\n_:b <http://x/q> \"v\" .\n";
    let quads = Mutex::new(Vec::new());
    par::ntriples(data.as_bytes(), &Options::default(), 4, |_, q| {
        quads.lock().unwrap().push(cquad(q));
    })
    .unwrap();
    let mut quads = quads.into_inner().unwrap();
    quads.sort();
    assert_eq!(quads.len(), 2);
    // Content-derived label, unified across segments (object of the first
    // statement, subject of the second).
    assert_eq!(quads[0].2, b"_sb".to_vec());
    assert_eq!(quads[1].0, b"_sb".to_vec());
    // Graph position rejected in N-Triples.
    let nq_line = "<http://x/a> <http://x/p> <http://x/o> <http://x/g> .\n";
    assert!(par::ntriples(nq_line.as_bytes(), &Options::default(), 2, |_, _| {}).is_err());
}
