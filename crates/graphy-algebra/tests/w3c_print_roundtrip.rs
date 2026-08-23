//! §M13c corpus gate, syntax half: every positive test of the W3C SPARQL
//! syntax suites must survive parse → print → re-parse with (1) the
//! printed text a fixpoint (printing the re-parse is byte-identical) and
//! (2) the §18.2 translation of both trees equal modulo consistent
//! renaming of parser-fresh/mangled blank labels (sugar reconstruction
//! may renumber fresh nodes; fallback label mapping may rename them).
//! Manifest machinery mirrors graphy-sparql-syntax/tests/w3c_syntax.rs.
//! Skips silently when `testdata/rdf-tests` is absent.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use graphy_algebra::{
    to_sse, translate_query, translate_update, Form, GraphTargetT, TranslatedQuery,
    TranslatedUpdate, UpdateOpT, VarTable, P,
};
use graphy_core::concise;
use graphy_core::TermRef;
use graphy_sparql_syntax::{parse_query, parse_update, print_query, print_update};
use graphy_turtle::{Options, TurtleParser};

const MF: &str = "http://www.w3.org/2001/sw/DataAccess/tests/test-manifest#";
const RDF: &str = "http://www.w3.org/1999/02/22-rdf-syntax-ns#";

// ------------------------------------------------------------- manifests

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Query,
    Update,
}

fn kind_of(type_iri: &str) -> Option<Kind> {
    match type_iri.rsplit('#').next()? {
        "PositiveSyntaxTest" | "PositiveSyntaxTest11" => Some(Kind::Query),
        "PositiveUpdateSyntaxTest" | "PositiveUpdateSyntaxTest11" => Some(Kind::Update),
        _ => None, // negatives have nothing to print
    }
}

type PoList = Vec<(Vec<u8>, Vec<u8>)>;

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

fn iri_path(term: &[u8]) -> PathBuf {
    let s = std::str::from_utf8(term).expect("utf8 term");
    let iri = s.strip_prefix('>').expect("IRI term");
    PathBuf::from(iri.strip_prefix("file://").expect("file IRI"))
}

// ------------------------------------------- label-insensitive rendering

/// Canonicalize renameable blank labels in an SSE-ish rendering: the
/// variables minted for pattern bnodes (`?.b:<label>`) and blank-node
/// terms whose label starts with `.` or `_` (parser-fresh, or mangled by
/// the printer's injective fallback). First-occurrence numbering on each
/// side turns "equal modulo consistent renaming" into string equality.
fn canon(s: &str) -> String {
    let chars: Vec<char> = s.chars().collect();
    let stop = |c: char| c.is_whitespace() || c == '(' || c == ')' || c == '"';
    let mut map: HashMap<String, usize> = HashMap::new();
    let mut out = String::new();
    let mut i = 0;
    while i < chars.len() {
        // `?.b:<label>` — a pattern-bnode variable.
        if chars[i] == '?'
            && i + 3 < chars.len()
            && chars[i + 1] == '.'
            && chars[i + 2] == 'b'
            && chars[i + 3] == ':'
        {
            let mut j = i + 4;
            while j < chars.len() && !stop(chars[j]) {
                j += 1;
            }
            let label: String = chars[i + 4..j].iter().collect();
            let n = map.len();
            let id = *map.entry(label).or_insert(n);
            let _ = std::fmt::Write::write_fmt(&mut out, format_args!("?.b:~{id}"));
            i = j;
            continue;
        }
        // `_:<label>` with a renameable label.
        if chars[i] == '_'
            && i + 2 < chars.len()
            && chars[i + 1] == ':'
            && (chars[i + 2] == '.' || chars[i + 2] == '_')
        {
            let mut j = i + 2;
            while j < chars.len() && !stop(chars[j]) {
                j += 1;
            }
            let label: String = chars[i + 2..j].iter().collect();
            let n = map.len();
            let id = *map.entry(label).or_insert(n);
            let _ = std::fmt::Write::write_fmt(&mut out, format_args!("_:~{id}"));
            i = j;
            continue;
        }
        out.push(chars[i]);
        i += 1;
    }
    out
}

fn term(bytes: &[u8]) -> String {
    match concise::decode(bytes) {
        Ok(t) => term_ref(&t),
        Err(_) => format!("{bytes:?}"),
    }
}

fn term_ref(t: &TermRef<'_>) -> String {
    match t {
        TermRef::Iri(i) => format!("<{i}>"),
        TermRef::BlankNode(l) => format!("_:{l}"),
        TermRef::Literal(l) => {
            let mut s = format!("{:?}", l.lexical());
            if let Some((tag, dir)) = l.lang() {
                s.push('@');
                s.push_str(tag);
                if dir.is_some() {
                    s.push_str("~dir");
                }
            } else {
                s.push_str("^^<");
                s.push_str(l.datatype());
                s.push('>');
            }
            s
        }
        TermRef::TripleTerm(v) => format!(
            "(tt {} {} {})",
            term_ref(&v.subject()),
            term_ref(&v.predicate()),
            term_ref(&v.object())
        ),
    }
}

fn pt(x: &P, vars: &VarTable) -> String {
    match x {
        P::Var(v) => format!("?{}", vars.name(*v)),
        P::Term(bytes) => term(bytes),
        P::Triple(tp) => format!(
            "(tt {} {} {})",
            pt(&tp.s, vars),
            pt(&tp.p, vars),
            pt(&tp.o, vars)
        ),
    }
}

/// Whole translated query, minus the name table (VarIds are assigned in
/// encounter order, so shape equality is id equality).
fn query_sse(t: &TranslatedQuery) -> String {
    let mut s = String::new();
    for (default, iri) in &t.dataset {
        s.push_str(if *default { "(from " } else { "(fromnamed " });
        s.push_str(&term(iri));
        s.push_str(")\n");
    }
    if let Some(b) = &t.base {
        s.push_str(&format!("(base <{b}>)\n"));
    }
    match &t.form {
        Form::Select => s.push_str("(select)\n"),
        Form::Ask => s.push_str("(ask)\n"),
        Form::Construct(template) => {
            s.push_str("(construct");
            for tp in template {
                s.push_str(&format!(
                    " (triple {} {} {})",
                    pt(&tp.s, &t.vars),
                    pt(&tp.p, &t.vars),
                    pt(&tp.o, &t.vars)
                ));
            }
            s.push_str(")\n");
        }
        Form::Describe(targets) => {
            s.push_str("(describe");
            for x in targets {
                s.push(' ');
                s.push_str(&pt(x, &t.vars));
            }
            s.push_str(")\n");
        }
    }
    s.push_str(&to_sse(&t.root, &t.vars));
    s
}

fn ground_quads(s: &mut String, tag: &str, quads: &[graphy_algebra::GroundQuad]) {
    s.push_str(&format!("({tag}"));
    for (g, sq, p, o) in quads {
        s.push_str(&format!(
            " (quad {} {} {} {})",
            g.as_deref().map_or("_".to_owned(), term),
            term(sq),
            term(p),
            term(o)
        ));
    }
    s.push_str(")\n");
}

fn quad_pats(s: &mut String, quads: &[graphy_algebra::QuadPat], vars: &VarTable) {
    for q in quads {
        s.push_str(&format!(
            " (quad {} {} {} {})",
            q.g.as_ref().map_or("_".to_owned(), |g| pt(g, vars)),
            pt(&q.s, vars),
            pt(&q.p, vars),
            pt(&q.o, vars)
        ));
    }
}

fn target(t: &GraphTargetT) -> String {
    match t {
        GraphTargetT::Default => "default".into(),
        GraphTargetT::Named(g) => term(g),
        GraphTargetT::AllNamed => "named".into(),
        GraphTargetT::All => "all".into(),
    }
}

fn update_sse(u: &TranslatedUpdate) -> String {
    let mut s = String::new();
    for op in &u.ops {
        match op {
            UpdateOpT::InsertData(q) => ground_quads(&mut s, "insertdata", q),
            UpdateOpT::DeleteData(q) => ground_quads(&mut s, "deletedata", q),
            UpdateOpT::DeleteWhere { vars, quads } => {
                s.push_str("(deletewhere");
                quad_pats(&mut s, quads, vars);
                s.push_str(")\n");
            }
            UpdateOpT::Modify {
                vars,
                with,
                delete,
                insert,
                using,
                pattern,
            } => {
                s.push_str("(modify");
                if let Some(w) = with {
                    s.push_str(&format!(" (with {})", term(w)));
                }
                s.push_str(" (delete");
                quad_pats(&mut s, delete, vars);
                s.push_str(") (insert");
                quad_pats(&mut s, insert, vars);
                s.push_str(") (using");
                for (default, iri) in using {
                    s.push_str(&format!(
                        " ({} {})",
                        if *default { "default" } else { "named" },
                        term(iri)
                    ));
                }
                s.push_str(")\n");
                s.push_str(&to_sse(pattern, vars));
                s.push_str(")\n");
            }
            UpdateOpT::Load {
                silent,
                source,
                into,
            } => {
                s.push_str(&format!(
                    "(load{} {} {})\n",
                    if *silent { " silent" } else { "" },
                    term(source),
                    into.as_deref().map_or("_".to_owned(), term)
                ));
            }
            UpdateOpT::Clear { silent, target: t } => {
                s.push_str(&format!(
                    "(clear{} {})\n",
                    if *silent { " silent" } else { "" },
                    target(t)
                ));
            }
            UpdateOpT::Drop { silent, target: t } => {
                s.push_str(&format!(
                    "(drop{} {})\n",
                    if *silent { " silent" } else { "" },
                    target(t)
                ));
            }
            UpdateOpT::Create { silent, graph } => {
                s.push_str(&format!(
                    "(create{} {})\n",
                    if *silent { " silent" } else { "" },
                    term(graph)
                ));
            }
            UpdateOpT::Add { silent, from, to }
            | UpdateOpT::Move { silent, from, to }
            | UpdateOpT::Copy { silent, from, to } => {
                let tag = match op {
                    UpdateOpT::Add { .. } => "add",
                    UpdateOpT::Move { .. } => "move",
                    _ => "copy",
                };
                s.push_str(&format!(
                    "({tag}{} {} {})\n",
                    if *silent { " silent" } else { "" },
                    from.as_deref().map_or("default".to_owned(), term),
                    to.as_deref().map_or("default".to_owned(), term)
                ));
            }
        }
    }
    s
}

// ------------------------------------------------------------ the gate

struct Outcome {
    ran: usize,
    failures: Vec<String>,
}

fn check_query_roundtrip(name: &str, src: &str, outcome: &mut Outcome) {
    let q1 = match parse_query(src) {
        Ok(q) => q,
        Err(_) => return, // strict-parse coverage belongs to w3c_syntax.rs
    };
    outcome.ran += 1;
    let printed = print_query(&q1);
    let q2 = match parse_query(&printed) {
        Ok(q) => q,
        Err(e) => {
            outcome.failures.push(format!(
                "{name}: printed form fails to parse: {e}\n{printed}"
            ));
            return;
        }
    };
    let printed2 = print_query(&q2);
    if printed2 != printed {
        outcome.failures.push(format!(
            "{name}: print not a fixpoint\n--- 1:\n{printed}\n--- 2:\n{printed2}"
        ));
        return;
    }
    match (translate_query(&q1), translate_query(&q2)) {
        (Ok(t1), Ok(t2)) => {
            let (a, b) = (canon(&query_sse(&t1)), canon(&query_sse(&t2)));
            if a != b {
                outcome.failures.push(format!(
                    "{name}: algebra differs after round-trip\n--- original:\n{a}\n--- printed:\n{b}\n--- text:\n{printed}"
                ));
            }
        }
        (Err(_), Err(_)) => {} // consistently untranslatable
        (Ok(_), Err(e)) => outcome.failures.push(format!(
            "{name}: printed form fails to translate: {e}\n{printed}"
        )),
        (Err(e), Ok(_)) => outcome.failures.push(format!(
            "{name}: original untranslatable ({e}) but printed form translates"
        )),
    }
}

fn check_update_roundtrip(name: &str, src: &str, outcome: &mut Outcome) {
    let u1 = match parse_update(src) {
        Ok(u) => u,
        Err(_) => return,
    };
    outcome.ran += 1;
    let printed = print_update(&u1);
    let u2 = match parse_update(&printed) {
        Ok(u) => u,
        Err(e) => {
            outcome.failures.push(format!(
                "{name}: printed form fails to parse: {e}\n{printed}"
            ));
            return;
        }
    };
    let printed2 = print_update(&u2);
    if printed2 != printed {
        outcome.failures.push(format!(
            "{name}: print not a fixpoint\n--- 1:\n{printed}\n--- 2:\n{printed2}"
        ));
        return;
    }
    match (translate_update(&u1), translate_update(&u2)) {
        (Ok(t1), Ok(t2)) => {
            let (a, b) = (canon(&update_sse(&t1)), canon(&update_sse(&t2)));
            if a != b {
                outcome.failures.push(format!(
                    "{name}: update algebra differs after round-trip\n--- original:\n{a}\n--- printed:\n{b}\n--- text:\n{printed}"
                ));
            }
        }
        (Err(_), Err(_)) => {}
        (Ok(_), Err(e)) => outcome.failures.push(format!(
            "{name}: printed form fails to translate: {e}\n{printed}"
        )),
        (Err(e), Ok(_)) => outcome.failures.push(format!(
            "{name}: original untranslatable ({e}) but printed form translates"
        )),
    }
}

fn run_manifest(path: &Path, outcome: &mut Outcome) {
    let g = Graph::load(path);
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
            continue;
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
        match kind {
            Kind::Query => check_query_roundtrip(&name, &src, outcome),
            Kind::Update => check_update_roundtrip(&name, &src, outcome),
        }
    }
}

fn suite_root() -> Option<PathBuf> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../testdata/rdf-tests/sparql");
    root.exists()
        .then(|| root.canonicalize().expect("canonical"))
}

#[test]
fn w3c_syntax_print_roundtrip() {
    let Some(root) = suite_root() else {
        eprintln!("skipping: testdata/rdf-tests not present");
        return;
    };
    let manifests = [
        "sparql10/syntax-sparql1/manifest.ttl",
        "sparql10/syntax-sparql2/manifest.ttl",
        "sparql10/syntax-sparql3/manifest.ttl",
        "sparql10/syntax-sparql4/manifest.ttl",
        "sparql10/syntax-sparql5/manifest.ttl",
        "sparql11/syntax-query/manifest.ttl",
        "sparql11/syntax-update-1/manifest.ttl",
        "sparql11/syntax-update-2/manifest.ttl",
        "sparql11/syntax-fed/manifest.ttl",
        "sparql12/codepoint-escapes/manifest.ttl",
        "sparql12/lang-basedir/manifest.ttl",
        "sparql12/version/manifest.ttl",
        "sparql12/syntax/manifest.ttl",
        "sparql12/syntax-triple-terms-positive/manifest.ttl",
    ];
    let mut outcome = Outcome {
        ran: 0,
        failures: Vec::new(),
    };
    for rel in manifests {
        let path = root.join(rel);
        if !path.exists() {
            panic!("manifest missing: {}", path.display());
        }
        run_manifest(&path, &mut outcome);
    }
    assert!(
        outcome.failures.is_empty(),
        "{} of {} round-trips failed:\n{}",
        outcome.failures.len(),
        outcome.ran,
        outcome.failures.join("\n=====\n")
    );
    println!(
        "print round-trip: {} positive syntax tests green",
        outcome.ran
    );
}
