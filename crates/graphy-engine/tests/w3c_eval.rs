//! W3C SPARQL 1.0, 1.1, and 1.2 **Query Evaluation** suites through the reference
//! evaluator (M7 exit criterion, doc 05 §9 — the conformance gate the
//! vectorized engine must later match). Manifests are Turtle, read through
//! graphy-turtle; each test builds a fresh store from `qt:data` /
//! `qt:graphData`, evaluates the query, and compares against the expected
//! results with blank-node bijection (multiset by default, sequence under
//! a top-level ORDER BY; graph isomorphism for CONSTRUCT).
//!
//! Every applicable evaluation manifest runs here, including property
//! paths. Federated `SERVICE` and optional entailment-regime tests are
//! tracked separately because they require an external endpoint or
//! entailment implementation. Skips silently when
//! `testdata/rdf-tests` is absent (gitignored checkout).
//!
//! Base-IRI convention: the harness prepends `BASE <file://…/>` (the query
//! file's directory) to every query, so relative IRIs in queries, data
//! files, FROM clauses, and `qt:graphData` names all land in the same
//! absolute `file://` space. FROM / FROM NAMED are handled *here* (dataset
//! construction is a protocol concern): the named files load into the
//! store as directed and the clause list is cleared before evaluation.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use graphy_algebra::{rewrite, translate_query, Algebra, TranslatedQuery};
use graphy_engine::exec::evaluate_vec;
use graphy_engine::{evaluate_ref, EngineError, Output};
use graphy_sparql_syntax::parse_query;
use graphy_store::{BuilderConfig, Profile, SegmentBuilder, Snapshot, Store};
use graphy_turtle::{NQuadsParser, NTriplesParser, Options, TriGParser, TurtleParser};

#[path = "../../../test-support/oracles.rs"]
mod oracles;

const MF: &str = "http://www.w3.org/2001/sw/DataAccess/tests/test-manifest#";
const QT: &str = "http://www.w3.org/2001/sw/DataAccess/tests/test-query#";
const RDF: &str = "http://www.w3.org/1999/02/22-rdf-syntax-ns#";
const XSD: &str = "http://www.w3.org/2001/XMLSchema#";

// ---------------------------------------------------------------------------
// Minimal triple index over one Turtle file (manifest reading).
// ---------------------------------------------------------------------------

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

    fn from_triples(triples: impl IntoIterator<Item = (Vec<u8>, Vec<u8>, Vec<u8>)>) -> Graph {
        let mut spo: HashMap<Vec<u8>, PoList> = HashMap::new();
        for (s, p, o) in triples {
            spo.entry(s).or_default().push((p, o));
        }
        Graph { spo }
    }

    fn object(&self, s: &[u8], p: &str) -> Option<&[u8]> {
        self.objects(s, p).next()
    }

    fn objects<'a>(&'a self, s: &[u8], p: &str) -> impl Iterator<Item = &'a [u8]> {
        let want = format!(">{p}").into_bytes();
        self.spo
            .get(s)
            .into_iter()
            .flatten()
            .filter(move |(pp, _)| *pp == want)
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

fn iri_str(term: &[u8]) -> &str {
    std::str::from_utf8(term)
        .expect("utf8 term")
        .strip_prefix('>')
        .expect("IRI term")
}

fn iri_path(term: &[u8]) -> PathBuf {
    PathBuf::from(iri_str(term).strip_prefix("file://").expect("file IRI"))
}

// ---------------------------------------------------------------------------
// Comparable term model (expected and actual results both normalize here).
// ---------------------------------------------------------------------------

/// Language tags carry an optional base direction (RDF 1.2); unused in the
/// 1.1 suites but kept so the model round-trips concise decode losslessly.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
enum T {
    Iri(String),
    BNode(String),
    /// `lang`/`dir` empty for non-language literals. Simple literals
    /// normalize to `dt = xsd:string`.
    Lit {
        lex: String,
        dt: String,
        lang: String,
        dir: String,
    },
    /// Unused by the 1.1 suites; the 1.2 eval manifests will exercise it.
    #[allow(dead_code)]
    Triple(Box<(T, T, T)>),
}

fn lit(lex: &str, dt: Option<&str>, lang: Option<&str>) -> T {
    let dt = match (dt, lang) {
        (_, Some(_)) => format!("{RDF}langString"),
        (Some(d), None) => d.to_owned(),
        (None, None) => format!("{XSD}string"),
    };
    T::Lit {
        lex: norm_numeric(lex, &dt),
        dt,
        lang: lang.unwrap_or("").to_ascii_lowercase(),
        dir: String::new(),
    }
}

/// Value-normalize numeric/boolean lexicals so that value-equal literals of
/// the SAME datatype compare equal ("2" ≡ "2.0" as decimals, "0.4" ≡
/// "4.0E-1" as doubles) — the standard comparison regime for these suites;
/// datatype differences still distinguish.
fn norm_numeric(lex: &str, dt: &str) -> String {
    let local = match dt.strip_prefix(XSD) {
        Some(l) => l,
        None => return lex.to_owned(),
    };
    match local {
        "integer" | "long" | "int" | "short" | "byte" | "nonNegativeInteger"
        | "positiveInteger" | "negativeInteger" | "nonPositiveInteger" | "unsignedLong"
        | "unsignedInt" | "unsignedShort" | "unsignedByte" => lex
            .trim()
            .parse::<i128>()
            .map(|v| v.to_string())
            .unwrap_or_else(|_| lex.to_owned()),
        "decimal" => {
            // sign + digits + optional fraction → minimal form with a point.
            let t = lex.trim();
            let (neg, rest) = match t.strip_prefix('-') {
                Some(r) => (true, r),
                None => (false, t.strip_prefix('+').unwrap_or(t)),
            };
            let (int, frac) = match rest.split_once('.') {
                Some((i, f)) => (i, f),
                None => (rest, ""),
            };
            if int.bytes().chain(frac.bytes()).any(|c| !c.is_ascii_digit()) {
                return lex.to_owned();
            }
            let int_t = int.trim_start_matches('0');
            let frac_t = frac.trim_end_matches('0');
            let int_n = if int_t.is_empty() { "0" } else { int_t };
            let frac_n = if frac_t.is_empty() { "0" } else { frac_t };
            let zero = int_n == "0" && frac_n == "0";
            format!("{}{int_n}.{frac_n}", if neg && !zero { "-" } else { "" })
        }
        "double" | "float" => match lex.trim() {
            "INF" | "+INF" => "INF".to_owned(),
            "-INF" => "-INF".to_owned(),
            "NaN" => "NaN".to_owned(),
            t => t
                .parse::<f64>()
                .map(|v| format!("{v:E}"))
                .unwrap_or_else(|_| lex.to_owned()),
        },
        "boolean" => match lex.trim() {
            "1" => "true".to_owned(),
            "0" => "false".to_owned(),
            t => t.to_owned(),
        },
        _ => lex.to_owned(),
    }
}

/// Concise term bytes → comparable term.
fn concise_to_t(bytes: &[u8]) -> T {
    use graphy_core::TermRef;
    match graphy_core::concise::decode(bytes).expect("valid concise term") {
        TermRef::Iri(i) => T::Iri(i.to_owned()),
        TermRef::BlankNode(l) => T::BNode(l.to_owned()),
        TermRef::Literal(l) => match l.lang() {
            Some((tag, dir)) => T::Lit {
                lex: l.lexical().to_owned(),
                dt: l.datatype().to_owned(),
                lang: tag.to_ascii_lowercase(),
                dir: dir
                    .map(|d| format!("{d:?}").to_ascii_lowercase())
                    .unwrap_or_default(),
            },
            None => lit(l.lexical(), Some(l.datatype()), None),
        },
        TermRef::TripleTerm(tt) => T::Triple(Box::new((
            term_ref_to_t(tt.subject()),
            term_ref_to_t(tt.predicate()),
            term_ref_to_t(tt.object()),
        ))),
    }
}

fn term_ref_to_t(term: graphy_core::TermRef<'_>) -> T {
    let mut bytes = Vec::new();
    fn write(out: &mut Vec<u8>, term: graphy_core::TermRef<'_>) {
        use graphy_core::{concise, TermRef};
        match term {
            TermRef::Iri(i) => concise::encode_iri(out, i),
            TermRef::BlankNode(b) => concise::encode_blank(out, b),
            TermRef::Literal(l) => {
                if let Some((lang, dir)) = l.lang() {
                    concise::encode_lang(out, l.lexical(), lang, dir);
                } else if l.datatype() == graphy_core::vocab::XSD_STRING {
                    concise::encode_simple(out, l.lexical());
                } else {
                    concise::encode_datatype(out, l.lexical(), l.datatype());
                }
            }
            TermRef::TripleTerm(t) => {
                let mut s = Vec::new();
                let mut p = Vec::new();
                let mut o = Vec::new();
                write(&mut s, t.subject());
                write(&mut p, t.predicate());
                write(&mut o, t.object());
                concise::encode_triple_term(out, &s, &p, &o);
            }
        }
    }
    write(&mut bytes, term);
    concise_to_t(&bytes)
}

// ---------------------------------------------------------------------------
// Expected-result parsing: SRX, SRJ, TSV, CSV, and Turtle graphs.
// ---------------------------------------------------------------------------

#[derive(Debug)]
enum Expected {
    Solutions {
        vars: Vec<String>,
        rows: Vec<Vec<Option<T>>>,
    },
    Boolean(bool),
    Graph(Vec<(T, T, T)>),
    /// CSV results are lossy (plain strings).
    Csv {
        vars: Vec<String>,
        rows: Vec<Vec<String>>,
    },
}

fn xml_unescape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find('&') {
        out.push_str(&rest[..i]);
        rest = &rest[i..];
        let semi = rest.find(';').unwrap_or(0);
        let ent = &rest[1..semi];
        match ent {
            "lt" => out.push('<'),
            "gt" => out.push('>'),
            "amp" => out.push('&'),
            "quot" => out.push('"'),
            "apos" => out.push('\''),
            _ if ent.starts_with("#x") || ent.starts_with("#X") => {
                let c = u32::from_str_radix(&ent[2..], 16).expect("hex char ref");
                out.push(char::from_u32(c).expect("valid char"));
            }
            _ if ent.starts_with('#') => {
                let c: u32 = ent[1..].parse().expect("dec char ref");
                out.push(char::from_u32(c).expect("valid char"));
            }
            _ => panic!("unknown XML entity &{ent};"),
        }
        rest = &rest[semi + 1..];
    }
    out.push_str(rest);
    out
}

fn xml_attr(tag: &str, name: &str) -> Option<String> {
    let mut search = 0usize;
    while let Some(i) = tag[search..].find(name) {
        let i = search + i;
        let after = tag[i + name.len()..].trim_start();
        if let Some(rest) = after.strip_prefix('=') {
            let rest = rest.trim_start();
            let q = rest.chars().next()?;
            if q == '"' || q == '\'' {
                let end = rest[1..].find(q)?;
                return Some(xml_unescape(&rest[1..1 + end]));
            }
        }
        search = i + name.len();
    }
    None
}

fn parse_xml_term_at(src: &str, at: &mut usize) -> T {
    let open = src[*at..].find('<').expect("result term") + *at;
    let close = src[open..].find('>').expect("closed result term") + open;
    let tag = &src[open + 1..close];
    let name = tag
        .split(|c: char| c.is_whitespace() || c == '/')
        .next()
        .unwrap_or("");
    *at = close + 1;
    match name {
        "uri" | "bnode" | "literal" => {
            let end = src[*at..]
                .find(&format!("</{name}"))
                .expect("closing result term")
                + *at;
            let value = xml_unescape(&src[*at..end]);
            *at = src[end..].find('>').expect("closed result term") + end + 1;
            match name {
                "uri" => T::Iri(value),
                "bnode" => T::BNode(value),
                _ => {
                    let mut term = lit(
                        &value,
                        xml_attr(tag, "datatype").as_deref(),
                        xml_attr(tag, "xml:lang")
                            .or_else(|| xml_attr(tag, "lang"))
                            .as_deref(),
                    );
                    if let T::Lit { dir, .. } = &mut term {
                        *dir = xml_attr(tag, "its:dir")
                            .or_else(|| xml_attr(tag, "direction"))
                            .unwrap_or_default();
                    }
                    term
                }
            }
        }
        "triple" => {
            let mut parts = Vec::new();
            for wrapper in ["subject", "predicate", "object"] {
                let wrapper_open = src[*at..]
                    .find(&format!("<{wrapper}"))
                    .expect("triple component")
                    + *at;
                *at = src[wrapper_open..].find('>').expect("component open") + wrapper_open + 1;
                parts.push(parse_xml_term_at(src, at));
                let wrapper_close = src[*at..]
                    .find(&format!("</{wrapper}"))
                    .expect("component close")
                    + *at;
                *at = src[wrapper_close..].find('>').expect("component closed") + wrapper_close + 1;
            }
            let triple_close = src[*at..].find("</triple").expect("triple close") + *at;
            *at = src[triple_close..].find('>').expect("triple closed") + triple_close + 1;
            T::Triple(Box::new((
                parts.remove(0),
                parts.remove(0),
                parts.remove(0),
            )))
        }
        other => panic!("unknown XML result term <{other}>"),
    }
}

/// Minimal SPARQL-XML-results reader — tags + attributes only, enough for
/// the suite's files, including recursively nested SPARQL 1.2 triple terms.
fn parse_srx(src: &str) -> Expected {
    // Tag scanner: yields (name, attrs, self_closing, text-before).
    let mut vars: Vec<String> = Vec::new();
    let mut rows: Vec<Vec<Option<T>>> = Vec::new();
    let mut at = 0usize;
    let mut cur: Option<Vec<Option<T>>> = None;
    let mut binding_var: Option<String> = None;
    let bytes = src;

    let attr = |tag: &str, name: &str| -> Option<String> {
        // name="value" | name='value'
        let mut search = 0usize;
        while let Some(i) = tag[search..].find(name) {
            let i = search + i;
            let after = &tag[i + name.len()..];
            let after = after.trim_start();
            if let Some(rest) = after.strip_prefix('=') {
                let rest = rest.trim_start();
                let q = rest.chars().next()?;
                if q == '"' || q == '\'' {
                    let end = rest[1..].find(q)?;
                    return Some(xml_unescape(&rest[1..1 + end]));
                }
            }
            search = i + name.len();
        }
        None
    };

    while let Some(open) = bytes[at..].find('<') {
        let open = at + open;
        let close = bytes[open..].find('>').expect("closed tag") + open;
        let tag = &bytes[open + 1..close];
        at = close + 1;
        if tag.starts_with('?') || tag.starts_with('!') {
            continue;
        }
        if let Some(closer) = tag.strip_prefix('/') {
            if closer.trim() == "result" {
                rows.push(cur.take().expect("open result"));
            }
            continue;
        }
        let name = tag
            .split(|c: char| c.is_whitespace() || c == '/')
            .next()
            .unwrap_or("");
        let text_until = |end_tag: &str, from: usize| -> (String, usize) {
            let end = bytes[from..]
                .find(&format!("</{end_tag}"))
                .expect("closing tag")
                + from;
            let text = xml_unescape(&bytes[from..end]);
            let after = bytes[end..].find('>').expect("closed") + end + 1;
            (text, after)
        };
        match name {
            "variable" => {
                if let Some(v) = attr(tag, "name") {
                    vars.push(v);
                }
            }
            "boolean" => {
                let (text, _) = text_until("boolean", at);
                return Expected::Boolean(text.trim() == "true");
            }
            "result" => {
                if !tag.ends_with('/') {
                    cur = Some(vec![None; vars.len()]);
                }
            }
            "binding" => {
                binding_var = attr(tag, "name");
            }
            "uri" => {
                let (text, after) = text_until("uri", at);
                at = after;
                set_binding(&mut cur, &vars, &binding_var, T::Iri(text));
            }
            "bnode" => {
                let (text, after) = text_until("bnode", at);
                at = after;
                set_binding(&mut cur, &vars, &binding_var, T::BNode(text));
            }
            "literal" => {
                if tag.ends_with('/') {
                    set_binding(&mut cur, &vars, &binding_var, lit("", None, None));
                    continue;
                }
                let dt = attr(tag, "datatype");
                let lang = attr(tag, "xml:lang");
                let (text, after) = text_until("literal", at);
                at = after;
                set_binding(
                    &mut cur,
                    &vars,
                    &binding_var,
                    lit(&text, dt.as_deref(), lang.as_deref()),
                );
            }
            "triple" => {
                // The opening tag was consumed by the outer scanner. Rewind
                // to it so the recursive term reader consumes the full tree.
                let mut term_at = open;
                let term = parse_xml_term_at(src, &mut term_at);
                at = term_at;
                set_binding(&mut cur, &vars, &binding_var, term);
            }
            _ => {}
        }
        // A self-closed `<result/>` is an empty row.
        if name == "result" && tag.ends_with('/') {
            cur = None;
            rows.push(vec![None; vars.len()]);
        }
    }
    Expected::Solutions { vars, rows }
}

fn set_binding(cur: &mut Option<Vec<Option<T>>>, vars: &[String], var: &Option<String>, t: T) {
    let (Some(row), Some(v)) = (cur.as_mut(), var.as_ref()) else {
        panic!("binding outside result");
    };
    let i = vars
        .iter()
        .position(|x| x == v)
        .unwrap_or_else(|| panic!("unknown result variable ?{v}"));
    row[i] = Some(t);
}

fn parse_srj(src: &str) -> Expected {
    let v: serde_json::Value = serde_json::from_str(src).expect("valid srj");
    if let Some(b) = v.get("boolean") {
        return Expected::Boolean(b.as_bool().expect("boolean"));
    }
    let vars: Vec<String> = v["head"]["vars"]
        .as_array()
        .map(|a| {
            a.iter()
                .map(|x| x.as_str().expect("var name").to_owned())
                .collect()
        })
        .unwrap_or_default();
    let mut rows = Vec::new();
    for b in v["results"]["bindings"].as_array().expect("bindings") {
        let mut row: Vec<Option<T>> = vec![None; vars.len()];
        let obj = b.as_object().expect("binding object");
        for (name, cell) in obj {
            let i = vars.iter().position(|x| x == name).expect("known var");
            row[i] = Some(parse_srj_term(cell));
        }
        rows.push(row);
    }
    Expected::Solutions { vars, rows }
}

fn parse_srj_term(cell: &serde_json::Value) -> T {
    let ty = cell["type"].as_str().expect("type");
    match ty {
        "uri" => T::Iri(cell["value"].as_str().expect("IRI value").to_owned()),
        "bnode" => T::BNode(cell["value"].as_str().expect("bnode value").to_owned()),
        "literal" | "typed-literal" => {
            let value = cell["value"].as_str().expect("literal value");
            let mut out = lit(
                value,
                cell["datatype"].as_str(),
                cell["xml:lang"].as_str().or_else(|| cell["lang"].as_str()),
            );
            if let T::Lit { dt, dir, .. } = &mut out {
                *dir = cell["its:dir"]
                    .as_str()
                    .or_else(|| cell["direction"].as_str())
                    .unwrap_or("")
                    .to_owned();
                if !dir.is_empty() {
                    *dt = format!("{RDF}dirLangString");
                }
            }
            out
        }
        "triple" => {
            let value = cell["value"].as_object().expect("triple value");
            T::Triple(Box::new((
                parse_srj_term(&value["subject"]),
                parse_srj_term(&value["predicate"]),
                parse_srj_term(&value["object"]),
            )))
        }
        other => panic!("srj term type {other:?}"),
    }
}

/// One TSV results cell (SPARQL 1.1 TSV: Turtle-ish term syntax).
fn parse_tsv_term(s: &str) -> Option<T> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    if let Some(rest) = s.strip_prefix('<') {
        return Some(T::Iri(rest.strip_suffix('>').expect("closed IRI").into()));
    }
    if let Some(rest) = s.strip_prefix("_:") {
        return Some(T::BNode(rest.to_owned()));
    }
    if let Some(body) = s.strip_prefix('"') {
        // "lex"^^<dt> | "lex"@lang | "lex"
        let mut lex = String::new();
        let mut chars = body.char_indices();
        let mut end = 0usize;
        while let Some((i, c)) = chars.next() {
            match c {
                '\\' => {
                    let (_, e) = chars.next().expect("escape");
                    lex.push(match e {
                        't' => '\t',
                        'n' => '\n',
                        'r' => '\r',
                        'b' => '\u{8}',
                        'f' => '\u{c}',
                        '"' => '"',
                        '\'' => '\'',
                        '\\' => '\\',
                        other => other,
                    });
                }
                '"' => {
                    end = i + 2; // past opening + closing quote
                    break;
                }
                c => lex.push(c),
            }
        }
        let rest = &s[end..];
        if let Some(dt) = rest.strip_prefix("^^<") {
            return Some(lit(
                &lex,
                Some(dt.strip_suffix('>').expect("closed dt")),
                None,
            ));
        }
        if let Some(tag) = rest.strip_prefix('@') {
            return Some(lit(&lex, None, Some(tag)));
        }
        return Some(lit(&lex, None, None));
    }
    // Bare numeric / boolean shorthand.
    if s == "true" || s == "false" {
        return Some(lit(s, Some(&format!("{XSD}boolean")), None));
    }
    let dt = if s.contains(['e', 'E']) {
        "double"
    } else if s.contains('.') {
        "decimal"
    } else {
        "integer"
    };
    Some(lit(s, Some(&format!("{XSD}{dt}")), None))
}

fn parse_tsv(src: &str) -> Expected {
    let mut lines = src.lines();
    let header = lines.next().unwrap_or("");
    let vars: Vec<String> = header
        .split('\t')
        .filter(|s| !s.trim().is_empty())
        .map(|s| s.trim().trim_start_matches('?').to_owned())
        .collect();
    let mut rows = Vec::new();
    for line in lines {
        if line.is_empty() && rows.is_empty() {
            continue;
        }
        rows.push(line.split('\t').map(parse_tsv_term).collect());
    }
    Expected::Solutions { vars, rows }
}

fn parse_csv(src: &str) -> Expected {
    let mut records: Vec<Vec<String>> = Vec::new();
    let mut cur: Vec<String> = Vec::new();
    let mut field = String::new();
    let mut chars = src.chars().peekable();
    let mut quoted = false;
    while let Some(c) = chars.next() {
        if quoted {
            match c {
                '"' if chars.peek() == Some(&'"') => {
                    chars.next();
                    field.push('"');
                }
                '"' => quoted = false,
                c => field.push(c),
            }
            continue;
        }
        match c {
            '"' => quoted = true,
            ',' => cur.push(std::mem::take(&mut field)),
            '\r' => {}
            '\n' => {
                cur.push(std::mem::take(&mut field));
                records.push(std::mem::take(&mut cur));
            }
            c => field.push(c),
        }
    }
    if !field.is_empty() || !cur.is_empty() {
        cur.push(field);
        records.push(cur);
    }
    let mut it = records.into_iter();
    let vars = it.next().unwrap_or_default();
    Expected::Csv {
        vars,
        rows: it.collect(),
    }
}

/// Parse an RDF graph or dataset file, preserving graph names for TriG and
/// N-Quads inputs.
fn parse_dataset_file(path: &Path) -> Vec<(Option<T>, T, T, T)> {
    let src = std::fs::read(path).expect("read graph");
    let base = format!("file://{}", path.display());
    let mut out = Vec::new();
    let mut sink = |q: graphy_turtle::QuadRef<'_>| {
        out.push((
            q.g.map(concise_to_t),
            concise_to_t(q.s),
            concise_to_t(q.p),
            concise_to_t(q.o),
        ));
    };
    match path.extension().and_then(|e| e.to_str()) {
        Some("nt") => {
            let mut p = NTriplesParser::new(Options::default()).expect("options");
            p.read_from(&src[..], &mut sink)
                .unwrap_or_else(|e| panic!("parse {}: {e}", path.display()));
        }
        Some("nq") => {
            let mut p = NQuadsParser::new(Options::default()).expect("options");
            p.read_from(&src[..], &mut sink)
                .unwrap_or_else(|e| panic!("parse {}: {e}", path.display()));
        }
        Some("trig") => {
            let opts = Options {
                base: Some(base),
                ..Options::default()
            };
            let mut p = TriGParser::new(opts).expect("options");
            p.read_from(&src[..], &mut sink)
                .unwrap_or_else(|e| panic!("parse {}: {e}", path.display()));
        }
        _ => {
            let opts = Options {
                base: Some(base),
                ..Options::default()
            };
            let mut p = TurtleParser::new(opts).expect("options");
            p.read_from(&src[..], &mut sink)
                .unwrap_or_else(|e| panic!("parse {}: {e}", path.display()));
        }
    }
    out
}

fn parse_graph_file(path: &Path) -> Vec<(T, T, T)> {
    parse_dataset_file(path)
        .into_iter()
        .map(|(_, s, p, o)| (s, p, o))
        .collect()
}

const RS: &str = "http://www.w3.org/2001/sw/DataAccess/tests/result-set#";

/// A Turtle expected file is either a DAWG `rs:ResultSet` (SELECT/ASK
/// result sets in RDF) or a plain graph (CONSTRUCT/DESCRIBE).
fn parse_ttl_expected(path: &Path) -> Expected {
    let g = Graph::load(path);
    let rs_type = format!(">{RS}ResultSet");
    let Some(node) = g
        .spo
        .iter()
        .find(|(_, pos)| {
            pos.iter()
                .any(|(p, o)| p == format!(">{RDF}type").as_bytes() && o == rs_type.as_bytes())
        })
        .map(|(s, _)| s.clone())
    else {
        return Expected::Graph(parse_graph_file(path));
    };
    if let Some(b) = g.object(&node, &format!("{RS}boolean")) {
        return Expected::Boolean(
            concise_to_t(b) == lit("true", Some(&format!("{XSD}boolean")), None),
        );
    }
    let vars: Vec<String> = g
        .objects(&node, &format!("{RS}resultVariable"))
        .map(|o| match concise_to_t(o) {
            T::Lit { lex, .. } => lex,
            other => panic!("rs:resultVariable {other:?}"),
        })
        .collect();
    let mut solutions: Vec<&[u8]> = g.objects(&node, &format!("{RS}solution")).collect();
    solutions.sort_by_key(|sol| {
        g.object(sol, &format!("{RS}index"))
            .map(concise_to_t)
            .and_then(|t| match t {
                T::Lit { lex, .. } => lex.parse::<u64>().ok(),
                _ => None,
            })
            .unwrap_or(u64::MAX)
    });
    let mut rows = Vec::new();
    for sol in solutions {
        let mut row: Vec<Option<T>> = vec![None; vars.len()];
        for binding in g.objects(sol, &format!("{RS}binding")) {
            let var = g
                .object(binding, &format!("{RS}variable"))
                .map(concise_to_t)
                .expect("rs:variable");
            let T::Lit { lex: name, .. } = var else {
                panic!("rs:variable is a literal");
            };
            let value = g.object(binding, &format!("{RS}value")).expect("rs:value");
            let i = vars
                .iter()
                .position(|v| *v == name)
                .unwrap_or_else(|| panic!("unknown rs variable ?{name}"));
            row[i] = Some(concise_to_t(value));
        }
        rows.push(row);
    }
    Expected::Solutions { vars, rows }
}

fn parse_expected(path: &Path) -> Expected {
    match path.extension().and_then(|e| e.to_str()) {
        Some("srx") => parse_srx(&std::fs::read_to_string(path).expect("read srx")),
        Some("srj") => parse_srj(&std::fs::read_to_string(path).expect("read srj")),
        Some("tsv") => parse_tsv(&std::fs::read_to_string(path).expect("read tsv")),
        Some("csv") => parse_csv(&std::fs::read_to_string(path).expect("read csv")),
        Some("ttl" | "nt") => parse_ttl_expected(path),
        Some("rdf") => {
            let src = std::fs::read_to_string(path).expect("read RDF/XML result");
            let triples =
                graphy_interop::parse_rdfxml(&src, Some(&format!("file://{}", path.display())))
                    .unwrap_or_else(|e| panic!("parse {}: {e}", path.display()));
            let g = Graph::from_triples(triples.into_iter().map(|t| (t.s, t.p, t.o)));
            parse_result_set_graph(&g)
                .unwrap_or_else(|| panic!("{}: no rs:ResultSet", path.display()))
        }
        other => panic!("unhandled result format {other:?} ({})", path.display()),
    }
}

fn parse_result_set_graph(g: &Graph) -> Option<Expected> {
    let rs_type = format!(">{RS}ResultSet");
    let node = g
        .spo
        .iter()
        .find(|(_, pos)| {
            pos.iter()
                .any(|(p, o)| p == format!(">{RDF}type").as_bytes() && o == rs_type.as_bytes())
        })
        .map(|(s, _)| s.clone())?;
    if let Some(b) = g.object(&node, &format!("{RS}boolean")) {
        return Some(Expected::Boolean(
            concise_to_t(b) == lit("true", Some(&format!("{XSD}boolean")), None),
        ));
    }
    let vars: Vec<String> = g
        .objects(&node, &format!("{RS}resultVariable"))
        .map(|o| match concise_to_t(o) {
            T::Lit { lex, .. } => lex,
            other => panic!("rs:resultVariable {other:?}"),
        })
        .collect();
    let mut solutions: Vec<&[u8]> = g.objects(&node, &format!("{RS}solution")).collect();
    solutions.sort_by_key(|sol| {
        g.object(sol, &format!("{RS}index"))
            .map(concise_to_t)
            .and_then(|t| match t {
                T::Lit { lex, .. } => lex.parse::<u64>().ok(),
                _ => None,
            })
            .unwrap_or(u64::MAX)
    });
    let mut rows = Vec::new();
    for sol in solutions {
        let mut row: Vec<Option<T>> = vec![None; vars.len()];
        for binding in g.objects(sol, &format!("{RS}binding")) {
            let T::Lit { lex: name, .. } =
                concise_to_t(g.object(binding, &format!("{RS}variable"))?)
            else {
                return None;
            };
            let value = g.object(binding, &format!("{RS}value"))?;
            let i = vars.iter().position(|v| *v == name)?;
            row[i] = Some(concise_to_t(value));
        }
        rows.push(row);
    }
    Some(Expected::Solutions { vars, rows })
}

// ---------------------------------------------------------------------------
// Comparison with blank-node bijection.
// ---------------------------------------------------------------------------

/// Try to extend the expected→actual bnode bijection so `e` equals `a`.
fn unify(e: &T, a: &T, map: &mut HashMap<String, String>, used: &mut Vec<String>) -> bool {
    match (e, a) {
        (T::BNode(el), T::BNode(al)) => match map.get(el) {
            Some(m) => m == al,
            None => {
                if map.values().any(|v| v == al) {
                    return false;
                }
                map.insert(el.clone(), al.clone());
                used.push(el.clone());
                true
            }
        },
        (T::Triple(et), T::Triple(at)) => {
            unify(&et.0, &at.0, map, used)
                && unify(&et.1, &at.1, map, used)
                && unify(&et.2, &at.2, map, used)
        }
        _ => e == a,
    }
}

fn row_unify(
    e: &[Option<T>],
    a: &[Option<T>],
    map: &mut HashMap<String, String>,
) -> Option<Vec<String>> {
    if e.len() != a.len() {
        return None;
    }
    let mut added = Vec::new();
    for (ec, ac) in e.iter().zip(a) {
        let ok = match (ec, ac) {
            (None, None) => true,
            (Some(et), Some(at)) => unify(et, at, map, &mut added),
            _ => false,
        };
        if !ok {
            for k in added {
                map.remove(&k);
            }
            return None;
        }
    }
    Some(added)
}

/// Multiset (or sequence) row comparison under a bnode bijection.
fn rows_match(
    expected: &[Vec<Option<T>>],
    actual: &[Vec<Option<T>>],
    ordered: bool,
    map: &mut HashMap<String, String>,
) -> bool {
    if expected.len() != actual.len() {
        return false;
    }
    if ordered {
        for (e, a) in expected.iter().zip(actual) {
            if row_unify(e, a, map).is_none() {
                return false;
            }
        }
        return true;
    }
    fn backtrack(
        expected: &[Vec<Option<T>>],
        actual: &[Vec<Option<T>>],
        taken: &mut Vec<bool>,
        i: usize,
        map: &mut HashMap<String, String>,
    ) -> bool {
        if i == expected.len() {
            return true;
        }
        for j in 0..actual.len() {
            if taken[j] {
                continue;
            }
            if let Some(added) = row_unify(&expected[i], &actual[j], map) {
                taken[j] = true;
                if backtrack(expected, actual, taken, i + 1, map) {
                    return true;
                }
                taken[j] = false;
                for k in added {
                    map.remove(&k);
                }
            }
        }
        false
    }
    backtrack(expected, actual, &mut vec![false; actual.len()], 0, map)
}

/// Graph isomorphism over triple sets (small suite graphs; backtracking).
fn graphs_match(expected: &[(T, T, T)], actual: &[(T, T, T)]) -> bool {
    let e_rows: Vec<Vec<Option<T>>> = dedup(expected);
    let a_rows: Vec<Vec<Option<T>>> = dedup(actual);
    fn dedup(g: &[(T, T, T)]) -> Vec<Vec<Option<T>>> {
        let mut v: Vec<Vec<Option<T>>> = g
            .iter()
            .map(|(s, p, o)| vec![Some(s.clone()), Some(p.clone()), Some(o.clone())])
            .collect();
        v.sort();
        v.dedup();
        v
    }
    e_rows.len() == a_rows.len() && rows_match(&e_rows, &a_rows, false, &mut HashMap::new())
}

// ---------------------------------------------------------------------------
// Test discovery + execution.
// ---------------------------------------------------------------------------

struct EvalTest {
    name: String,
    query: PathBuf,
    data: Vec<PathBuf>,
    /// (graph name IRI, file path)
    graph_data: Vec<(String, PathBuf)>,
    result: PathBuf,
}

fn collect_manifest(path: &Path, out: &mut Vec<EvalTest>) {
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
    let Some(entries_head) = g.object(&manifest_node, &format!("{MF}entries")) else {
        return;
    };
    for entry in g.list(entries_head) {
        let is_eval = g.objects(&entry, &format!("{RDF}type")).any(|o| {
            o == format!(">{MF}QueryEvaluationTest").as_bytes()
                || o == format!(">{MF}CSVResultFormatTest").as_bytes()
        });
        if !is_eval {
            continue;
        }
        let name = g
            .object(&entry, &format!("{MF}name"))
            .map(|o| String::from_utf8_lossy(o).into_owned())
            .unwrap_or_else(|| String::from_utf8_lossy(&entry).into_owned());
        let action = g
            .object(&entry, &format!("{MF}action"))
            .expect("eval test has mf:action")
            .to_vec();
        let query = iri_path(
            g.object(&action, &format!("{QT}query"))
                .expect("action has qt:query"),
        );
        let data: Vec<PathBuf> = g
            .objects(&action, &format!("{QT}data"))
            .map(iri_path)
            .collect();
        let graph_data: Vec<(String, PathBuf)> = g
            .objects(&action, &format!("{QT}graphData"))
            .map(|o| {
                if o.starts_with(b">") {
                    (iri_str(o).to_owned(), iri_path(o))
                } else {
                    // Bnode form: qt:graph <file> (+ rdfs:label name).
                    let file = g
                        .object(o, &format!("{QT}graph"))
                        .expect("graphData node has qt:graph");
                    let name = g
                        .object(o, "http://www.w3.org/2000/01/rdf-schema#label")
                        .map(|l| {
                            String::from_utf8_lossy(l)
                                .trim_start_matches('"')
                                .to_owned()
                        })
                        .unwrap_or_else(|| iri_str(file).to_owned());
                    (name, iri_path(file))
                }
            })
            .collect();
        let result = iri_path(
            g.object(&entry, &format!("{MF}result"))
                .expect("eval test has mf:result"),
        );
        out.push(EvalTest {
            name,
            query,
            data,
            graph_data,
            result,
        });
    }
}

static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn scratch() -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "graphy-w3c-eval-{}-{}",
        std::process::id(),
        SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

/// Build a store: `data` files into the default graph, plus named graphs.
fn build_store(
    dir: &Path,
    data: &[PathBuf],
    graphs: &[(String, PathBuf)],
) -> Result<Store, String> {
    let mut cfg = BuilderConfig::new(dir);
    cfg.profile = Profile::Balanced;
    cfg.sort_budget = 1 << 16;
    let mut b = SegmentBuilder::new(cfg).map_err(|e| e.to_string())?;
    let mut file_no = 0u32;
    let mut load = |path: &Path, graph: Option<&[u8]>| -> Result<(), String> {
        let src = std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
        let opts = Options {
            base: Some(format!("file://{}", path.display())),
            label_ns: Some(u128::from(file_no)),
            ..Options::default()
        };
        file_no += 1;
        let mut err: Option<String> = None;
        let mut feed = |q: graphy_turtle::QuadRef<'_>| {
            if err.is_none() {
                let g = q.g.or(graph);
                if let Err(e) = b.push_quad(q.s, q.p, q.o, g) {
                    err = Some(e.to_string());
                }
            }
        };
        let ext = path.extension().and_then(|e| e.to_str());
        if ext == Some("rdf") {
            let text = std::str::from_utf8(&src)
                .map_err(|e| format!("{}: invalid UTF-8: {e}", path.display()))?;
            let base = format!("file://{}", path.display());
            for t in graphy_interop::parse_rdfxml(text, Some(&base))
                .map_err(|e| format!("{}: {e}", path.display()))?
            {
                b.push_quad(&t.s, &t.p, &t.o, graph)
                    .map_err(|e| e.to_string())?;
            }
            return Ok(());
        }
        let r = if ext == Some("nt") {
            NTriplesParser::new(Options {
                label_ns: Some(u128::from(file_no - 1)),
                ..Options::default()
            })
            .expect("options")
            .read_from(&src[..], &mut feed)
        } else if ext == Some("nq") {
            NQuadsParser::new(Options {
                label_ns: Some(u128::from(file_no - 1)),
                ..Options::default()
            })
            .expect("options")
            .read_from(&src[..], &mut feed)
        } else if ext == Some("trig") {
            TriGParser::new(opts)
                .expect("options")
                .read_from(&src[..], &mut feed)
        } else {
            TurtleParser::new(opts)
                .expect("options")
                .read_from(&src[..], &mut feed)
        };
        r.map_err(|e| format!("{}: {e}", path.display()))?;
        if let Some(e) = err {
            return Err(e);
        }
        Ok(())
    };
    for d in data {
        load(d, None)?;
    }
    for (name, path) in graphs {
        let g = format!(">{name}").into_bytes();
        load(path, Some(&g))?;
    }
    b.finish().map_err(|e| e.to_string())?;
    Store::open(dir).map_err(|e| e.to_string())
}

/// Outermost-modifier walk: is the query's result a sequence (ORDER BY)?
fn is_ordered(a: &Algebra) -> bool {
    match a {
        Algebra::OrderBy { .. } => true,
        Algebra::Project { input, .. }
        | Algebra::Distinct(input)
        | Algebra::Reduced(input)
        | Algebra::Slice { input, .. } => is_ordered(input),
        _ => false,
    }
}

/// CSV projection of a term (SPARQL 1.1 CSV results: plain string forms),
/// from concise bytes directly — CSV compares raw lexicals, so the value
/// normalization applied to `T` must not interfere.
fn csv_str(bytes: &[u8]) -> String {
    use graphy_core::TermRef;
    match graphy_core::concise::decode(bytes).expect("valid concise term") {
        TermRef::Iri(i) => i.to_owned(),
        TermRef::BlankNode(l) => format!("_:{l}"),
        TermRef::Literal(l) => l.lexical().to_owned(),
        TermRef::TripleTerm(_) => "<< >>".into(),
    }
}

fn run_test(t: &EvalTest) -> Result<(), String> {
    // Parse (with the file's directory as base — see module docs).
    let src = std::fs::read_to_string(&t.query).map_err(|e| e.to_string())?;
    let dir = t.query.parent().expect("query dir");
    let based = format!("BASE <file://{}/>\n{src}", dir.display());
    let q = parse_query(&based).map_err(|e| format!("parse: {e}"))?;
    let mut tq = translate_query(&q).map_err(|e| format!("translate: {e}"))?;
    tq.root = rewrite(tq.root.clone());

    // §M13c print round-trip: the printed form must parse, translate,
    // and evaluate to the same results through both engines.
    let printed = graphy_sparql_syntax::print_query(&q);
    let q2 = parse_query(&printed)
        .map_err(|e| format!("printed form fails to parse: {e}\n---\n{printed}"))?;
    let mut tq2 = translate_query(&q2)
        .map_err(|e| format!("printed form fails to translate: {e}\n---\n{printed}"))?;
    tq2.root = rewrite(tq2.root.clone());

    // Dataset construction (protocol concern, handled here): FROM / FROM
    // NAMED override qt:data / qt:graphData when present.
    let (data, graphs): (Vec<PathBuf>, Vec<(String, PathBuf)>) = if tq.dataset.is_empty() {
        (t.data.clone(), t.graph_data.clone())
    } else {
        let mut data = Vec::new();
        let mut graphs = Vec::new();
        for (default, iri) in &tq.dataset {
            let path = iri_path(iri);
            if *default {
                data.push(path);
            } else {
                graphs.push((iri_str(iri).to_owned(), path));
            }
        }
        tq.dataset.clear();
        (data, graphs)
    };

    // Mirror the dataset-override postprocessing on the printed form (its
    // FROM clauses are byte-identical by construction).
    tq2.dataset.clear();

    let dir = scratch();
    let store = build_store(&dir, &data, &graphs)?;
    let snap = store.snapshot();
    // Dual-run conformance (doc 05 §9): the reference evaluator AND the
    // vectorized engine must independently match the expected results —
    // for the original AND its printed round-trip (§M13c).
    type Engine = fn(&Snapshot, &TranslatedQuery) -> Result<Output, EngineError>;
    let engines: [(&str, Engine); 2] = [("ref", evaluate_ref), ("vec", evaluate_vec)];
    let mut result = Ok(());
    'outer: for (form, query) in [("", &tq), ("printed/", &tq2)] {
        let ordered = is_ordered(&query.root);
        for (name, run) in engines {
            let out = match run(&snap, query).map_err(|e| format!("[{form}{name}] evaluate: {e}")) {
                Ok(out) => out,
                Err(e) => {
                    result = Err(e);
                    break 'outer;
                }
            };
            let expected = parse_expected(&t.result);
            if let Err(e) = check_output(expected, out, ordered) {
                result = Err(format!("[{form}{name}] {e}"));
                break 'outer;
            }
        }
    }
    std::fs::remove_dir_all(&dir).ok();
    result
}

fn check_output(expected: Expected, out: Output, ordered: bool) -> Result<(), String> {
    match (expected, out) {
        (Expected::Boolean(e), Output::Boolean(a)) => {
            if e != a {
                return Err(format!("ASK: expected {e}, got {a}"));
            }
        }
        (Expected::Solutions { vars, rows }, Output::Solutions { vars: av, rows: ar }) => {
            let mut evs: Vec<&String> = vars.iter().collect();
            let mut avs: Vec<&String> = av.iter().collect();
            evs.sort();
            avs.sort();
            if evs != avs {
                return Err(format!("vars: expected {vars:?}, got {av:?}"));
            }
            // Reorder actual rows into the expected var order.
            let order: Vec<usize> = vars
                .iter()
                .map(|v| av.iter().position(|x| x == v).expect("var present"))
                .collect();
            let actual: Vec<Vec<Option<T>>> = ar
                .iter()
                .map(|r| {
                    order
                        .iter()
                        .map(|&i| r[i].as_deref().map(concise_to_t))
                        .collect()
                })
                .collect();
            if !rows_match(&rows, &actual, ordered, &mut HashMap::new()) {
                fn masked(t: &T) -> T {
                    match t {
                        T::BNode(_) => T::BNode("*".into()),
                        T::Triple(parts) => T::Triple(Box::new((
                            masked(&parts.0),
                            masked(&parts.1),
                            masked(&parts.2),
                        ))),
                        other => other.clone(),
                    }
                }
                let mask_row = |r: &Vec<Option<T>>| {
                    r.iter().map(|t| t.as_ref().map(masked)).collect::<Vec<_>>()
                };
                let expected_masked: Vec<_> = rows.iter().map(mask_row).collect();
                let actual_masked: Vec<_> = actual.iter().map(mask_row).collect();
                let missing: Vec<_> = expected_masked
                    .iter()
                    .filter(|r| !actual_masked.contains(r))
                    .take(12)
                    .collect();
                let extra: Vec<_> = actual_masked
                    .iter()
                    .filter(|r| !expected_masked.contains(r))
                    .take(12)
                    .collect();
                return Err(format!(
                    "solutions differ ({} expected vs {} actual rows{})\nmissing: {missing:?}\nextra:   {extra:?}",
                    rows.len(),
                    actual.len(),
                    if ordered { ", ordered" } else { "" },
                ));
            }
        }
        (Expected::Csv { vars, rows }, Output::Solutions { vars: av, rows: ar }) => {
            if vars != av {
                return Err(format!("csv vars: expected {vars:?}, got {av:?}"));
            }
            let mut actual: Vec<Vec<String>> = ar
                .iter()
                .map(|r| {
                    r.iter()
                        .map(|c| c.as_deref().map(csv_str).unwrap_or_default())
                        .collect()
                })
                .collect();
            let mut exp = rows;
            if !ordered {
                actual.sort();
                exp.sort();
            }
            // CSV is lossy for bnode labels: normalize label columns away.
            let norm = |rows: &mut Vec<Vec<String>>| {
                for r in rows {
                    for c in r {
                        if c.starts_with("_:") {
                            *c = "_:".into();
                        }
                    }
                }
            };
            norm(&mut actual);
            norm(&mut exp);
            if exp != actual {
                return Err(format!(
                    "csv rows differ\nexpected: {exp:?}\nactual:   {actual:?}"
                ));
            }
        }
        (Expected::Graph(e), Output::Triples(a)) => {
            let actual: Vec<(T, T, T)> = a
                .iter()
                .map(|(s, p, o)| (concise_to_t(s), concise_to_t(p), concise_to_t(o)))
                .collect();
            if !graphs_match(&e, &actual) {
                return Err(format!(
                    "graphs differ ({} expected vs {} actual triples)\nexpected: {e:?}\nactual:   {actual:?}",
                    e.len(),
                    actual.len(),
                ));
            }
        }
        (e, a) => return Err(format!("result shape mismatch: expected {e:?}, got {a:?}")),
    }
    Ok(())
}

fn suite_root() -> Option<PathBuf> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../testdata/rdf-tests/sparql");
    root.exists()
        .then(|| root.canonicalize().expect("canonical"))
}

/// Known-unsupported individual tests (tracked, not silently skipped):
/// name → reason. Directory-level omissions are limited to SERVICE and
/// optional entailment regimes, as documented above.
const EXCLUDED: &[(&str, &str)] = &[];

/// Execute arbitrary W3C-manifest-shaped query suites. Exclusions are keyed
/// by query filename so upstream display names may change without silently
/// changing the conformance boundary.
fn run_query_manifests(label: &str, manifests: &[PathBuf], exclusions: &[(&str, &str)]) {
    let mut tests = Vec::new();
    for manifest in manifests {
        assert!(
            manifest.is_file(),
            "manifest missing: {}",
            manifest.display()
        );
        collect_manifest(manifest, &mut tests);
    }
    assert!(
        !tests.is_empty(),
        "{label}: no query evaluation tests found"
    );
    let mut ran = 0usize;
    let mut excluded = 0usize;
    let mut exclusion_hits = vec![0usize; exclusions.len()];
    let mut failures: Vec<String> = Vec::new();
    for t in &tests {
        let query_file = t.query.file_name().unwrap_or_default().to_string_lossy();
        if let Some((index, (_, why))) = exclusions
            .iter()
            .enumerate()
            .find(|(_, (n, _))| *n == query_file)
        {
            exclusion_hits[index] += 1;
            eprintln!("excluded: {} [{}] ({why})", t.name, query_file);
            excluded += 1;
            continue;
        }
        ran += 1;
        // One bad test must not kill the census: panics report as failures.
        let outcome = std::panic::catch_unwind(|| run_test(t)).unwrap_or_else(|p| {
            let msg = p
                .downcast_ref::<String>()
                .cloned()
                .or_else(|| p.downcast_ref::<&str>().map(|s| (*s).to_owned()))
                .unwrap_or_else(|| "panic".into());
            Err(format!("panicked: {msg}"))
        });
        if let Err(e) = outcome {
            failures.push(format!("{} [{}]: {e}", t.name, query_file));
        }
    }
    for ((file, _), hits) in exclusions.iter().zip(exclusion_hits) {
        assert_eq!(
            hits, 1,
            "{label}: exclusion `{file}` must match exactly one test, matched {hits}"
        );
    }
    assert!(
        failures.is_empty(),
        "{} of {} failed:\n{}",
        failures.len(),
        ran,
        failures.join("\n---\n")
    );
    println!("{label}: {ran} tests green ({excluded} excluded)");
}

fn run_query_evaluation(version: &str, dirs: &[&str]) {
    let Some(root) = suite_root() else {
        eprintln!("skipping: testdata/rdf-tests not present");
        return;
    };
    let manifests: Vec<PathBuf> = dirs
        .iter()
        .map(|d| root.join(version).join(d).join("manifest.ttl"))
        .collect();
    run_query_manifests(&format!("{version} query evaluation"), &manifests, EXCLUDED);
}

const LATERAL_EXCLUSION: &str = "SPARQL 1.2 LATERAL is not implemented";

const OXIGRAPH_EXCLUDED: &[(&str, &str)] = &[
    ("filter.rq", LATERAL_EXCLUSION),
    ("graph.rq", LATERAL_EXCLUSION),
    ("join.rq", LATERAL_EXCLUSION),
    ("optional.rq", LATERAL_EXCLUSION),
    ("subselect.rq", LATERAL_EXCLUSION),
    ("subselect_aggregate.rq", LATERAL_EXCLUSION),
    ("subselect_inside_optional.rq", LATERAL_EXCLUSION),
    ("subselect_outside_optional.rq", LATERAL_EXCLUSION),
    (
        "ask_union_error_left.rq",
        "SERVICE execution is not implemented",
    ),
    (
        "ask_union_error_right.rq",
        "SERVICE execution is not implemented",
    ),
    (
        "order_terms.rq",
        "asserts Oxigraph's implementation-defined ordering of incomparable literal types",
    ),
    (
        "xsd_string_cast.rq",
        "expected results canonicalize source RDF terms, conflicting with lexical-form term identity",
    ),
];

#[test]
fn oxigraph_query_evaluation() {
    let Some(root) = oracles::checkout("oxigraph") else {
        return;
    };
    let root = root.join("testsuite/oxigraph-tests/sparql");
    run_query_manifests(
        "Oxigraph query evaluation",
        &[root.join("manifest.ttl"), root.join("lateral/manifest.ttl")],
        OXIGRAPH_EXCLUDED,
    );
}

const RDF4J_EXCLUDED: &[(&str, &str)] = &[
    ("lateral-01.rq", LATERAL_EXCLUSION),
    ("lateral-02.rq", LATERAL_EXCLUSION),
    ("lateral-03.rq", LATERAL_EXCLUSION),
    ("lateral-04.rq", LATERAL_EXCLUSION),
    ("lateral-05.rq", LATERAL_EXCLUSION),
    (
        "sparql11-bindings-01.rq",
        "uses the pre-standard BINDINGS keyword instead of VALUES",
    ),
    (
        "sparql11-bindings-02.rq",
        "uses the pre-standard BINDINGS keyword instead of VALUES",
    ),
    (
        "bsbm-bi-q5.rq",
        "upstream expected result contains malformed whitespace inside an IRI",
    ),
    (
        "sparql11-bnode-01.rq",
        "upstream result binds a variable not declared in its result header",
    ),
    (
        "sparql11-bnode-02.rq",
        "upstream result binds a variable not declared in its result header",
    ),
    (
        "sparql11-sequence-04.rq",
        "uses RDF4J's nonstandard property-path cardinality syntax",
    ),
    (
        "sparql11-sequence-05.rq",
        "uses RDF4J's nonstandard property-path cardinality syntax",
    ),
    (
        "sparql11-sequence-06.rq",
        "uses RDF4J's nonstandard property-path cardinality syntax",
    ),
    (
        "sparql11-wildcard-05.rq",
        "upstream result declares a variable absent from SELECT *",
    ),
];

#[test]
fn rdf4j_query_evaluation() {
    let Some(root) = oracles::checkout("rdf4j") else {
        return;
    };
    let resources = root.join("testsuites/sparql/src/main/resources");
    let v11 = resources.join("testcases-sparql-1.1");
    let v12 = resources.join("testcases-sparql-1.2");
    let manifests = [
        v11.join("aggregates/manifest.ttl"),
        v11.join("bindings/manifest.ttl"),
        v11.join("bsbm/manifest.ttl"),
        v11.join("builtin/manifest.ttl"),
        v11.join("expressions/manifest.ttl"),
        v11.join("negation/manifest.ttl"),
        v11.join("property-paths/manifest.ttl"),
        v11.join("subquery/manifest.ttl"),
        v12.join("aggregates/manifest.ttl"),
        v12.join("lateral/manifest.ttl"),
    ];
    run_query_manifests("RDF4J query evaluation", &manifests, RDF4J_EXCLUDED);
}

#[test]
fn sparql10_query_evaluation() {
    run_query_evaluation(
        "sparql10",
        &[
            "algebra",
            "ask",
            "basic",
            "bnode-coreference",
            "boolean-effective-value",
            "bound",
            "cast",
            "construct",
            "dataset",
            "distinct",
            "expr-builtin",
            "expr-equals",
            "expr-ops",
            "graph",
            "i18n",
            "open-world",
            "optional",
            "optional-filter",
            "reduced",
            "regex",
            "solution-seq",
            "sort",
            "triple-match",
            "type-promotion",
        ],
    );
}

#[test]
fn sparql11_query_evaluation() {
    run_query_evaluation(
        "sparql11",
        &[
            "aggregates",
            "bind",
            "bindings",
            "cast",
            "construct",
            "csv-tsv-res",
            "exists",
            "functions",
            "grouping",
            "json-res",
            "negation",
            "project-expression",
            "property-path",
            "subquery",
        ],
    );
}

#[test]
fn sparql12_query_evaluation() {
    run_query_evaluation(
        "sparql12",
        &[
            "codepoint-escapes",
            "eval-triple-terms",
            "expression",
            "grouping",
            "lang-basedir",
            "rdf11",
            "version",
        ],
    );
}

// ---------------------------------------------------------------------------
// SPARQL 1.1 Update evaluation (M7 inc.3): run the request through the
// update executor, compare the final dataset against the expected one
// (per-quad rows with graph column, blank-node bijection).
// ---------------------------------------------------------------------------

const UT: &str = "http://www.w3.org/2009/sparql/tests/test-update#";
const RDFS: &str = "http://www.w3.org/2000/01/rdf-schema#";

struct UpdateTest {
    name: String,
    request: PathBuf,
    data: Vec<PathBuf>,
    graph_data: Vec<(String, PathBuf)>,
    expected_data: Vec<PathBuf>,
    expected_graphs: Vec<(String, PathBuf)>,
}

/// `ut:data` / `ut:graphData [ut:graph <f>; rdfs:label "name"]` under one
/// action/result node.
fn ut_dataset(g: &Graph, node: &[u8]) -> (Vec<PathBuf>, Vec<(String, PathBuf)>) {
    let data = g
        .objects(node, &format!("{UT}data"))
        .map(iri_path)
        .collect();
    let graphs = g
        .objects(node, &format!("{UT}graphData"))
        .map(|gd| {
            let file = g
                .object(gd, &format!("{UT}graph"))
                .expect("ut:graphData has ut:graph");
            let label = g
                .object(gd, &format!("{RDFS}label"))
                .map(|l| match concise_to_t(l) {
                    T::Lit { lex, .. } => lex,
                    other => panic!("rdfs:label {other:?}"),
                })
                .expect("ut:graphData has rdfs:label");
            (label, iri_path(file))
        })
        .collect();
    (data, graphs)
}

fn collect_update_manifest(path: &Path, out: &mut Vec<UpdateTest>) {
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
    let Some(entries_head) = g.object(&manifest_node, &format!("{MF}entries")) else {
        return;
    };
    for entry in g.list(entries_head) {
        let is_update = g
            .objects(&entry, &format!("{RDF}type"))
            .any(|o| o == format!(">{MF}UpdateEvaluationTest").as_bytes());
        if !is_update {
            continue;
        }
        let name = g
            .object(&entry, &format!("{MF}name"))
            .map(|o| String::from_utf8_lossy(o).into_owned())
            .unwrap_or_else(|| String::from_utf8_lossy(&entry).into_owned());
        let action = g
            .object(&entry, &format!("{MF}action"))
            .expect("update test has mf:action")
            .to_vec();
        let request = iri_path(
            g.object(&action, &format!("{UT}request"))
                .expect("action has ut:request"),
        );
        let (data, graph_data) = ut_dataset(&g, &action);
        let result = g
            .object(&entry, &format!("{MF}result"))
            .expect("update test has mf:result")
            .to_vec();
        let (expected_data, expected_graphs) = ut_dataset(&g, &result);
        out.push(UpdateTest {
            name,
            request,
            data,
            graph_data,
            expected_data,
            expected_graphs,
        });
    }
}

/// Sorted, deduplicated dataset rows `[graph?, s, p, o]` (datasets are
/// quad sets).
fn dataset_rows(mut rows: Vec<Vec<Option<T>>>) -> Vec<Vec<Option<T>>> {
    rows.sort();
    rows.dedup();
    rows
}

fn run_update_test(t: &UpdateTest) -> Result<(), String> {
    use graphy_algebra::translate_update;
    use graphy_sparql_syntax::parse_update;

    let src = std::fs::read_to_string(&t.request).map_err(|e| e.to_string())?;
    let dir = t.request.parent().expect("request dir");
    let based = format!("BASE <file://{}/>\n{src}", dir.display());
    let req = parse_update(&based).map_err(|e| format!("parse: {e}"))?;
    let tu = translate_update(&req).map_err(|e| format!("translate: {e}"))?;
    apply_and_check(t, &tu)?;

    // §M13c print round-trip: the printed request must parse, translate,
    // and drive a fresh store to the same expected dataset.
    let printed = graphy_sparql_syntax::print_update(&req);
    let req2 = parse_update(&printed)
        .map_err(|e| format!("printed form fails to parse: {e}\n---\n{printed}"))?;
    let tu2 = translate_update(&req2)
        .map_err(|e| format!("printed form fails to translate: {e}\n---\n{printed}"))?;
    apply_and_check(t, &tu2).map_err(|e| format!("[printed] {e}\n---\n{printed}"))
}

fn apply_and_check(t: &UpdateTest, tu: &graphy_algebra::TranslatedUpdate) -> Result<(), String> {
    use graphy_engine::execute_update;

    let store_dir = scratch();
    build_store(&store_dir, &t.data, &t.graph_data)?;
    let store = Store::open(&store_dir).map_err(|e| e.to_string())?;
    execute_update(&store, tu).map_err(|e| format!("execute: {e}"))?;

    // Final dataset → rows.
    let snap = store.snapshot();
    let mut actual: Vec<Vec<Option<T>>> = Vec::new();
    let pat = graphy_store::Pattern::default();
    let mut scan = snap
        .scan(&pat, graphy_store::Order::Spo)
        .map_err(|e| e.to_string())?;
    let mut batch = graphy_store::QuadBatch::new();
    while scan.next_batch(&mut batch).map_err(|e| e.to_string())? {
        for i in 0..batch.len() {
            let g = match batch.g[i] {
                0 => None,
                v => Some(concise_to_t(
                    &snap
                        .decode_value(v, graphy_store::TermPos::Graph)
                        .map_err(|e| e.to_string())?,
                )),
            };
            let s = snap
                .decode_value(batch.s[i], graphy_store::TermPos::Subject)
                .map_err(|e| e.to_string())?;
            let p = snap
                .decode_value(batch.p[i], graphy_store::TermPos::Predicate)
                .map_err(|e| e.to_string())?;
            let o = snap
                .decode_value(batch.o[i], graphy_store::TermPos::Object)
                .map_err(|e| e.to_string())?;
            actual.push(vec![
                g,
                Some(concise_to_t(&s)),
                Some(concise_to_t(&p)),
                Some(concise_to_t(&o)),
            ]);
        }
    }
    drop(snap);
    drop(store);
    std::fs::remove_dir_all(&store_dir).ok();

    // Expected dataset → rows.
    let mut expected: Vec<Vec<Option<T>>> = Vec::new();
    for f in &t.expected_data {
        for (g, s, p, o) in parse_dataset_file(f) {
            expected.push(vec![g, Some(s), Some(p), Some(o)]);
        }
    }
    for (label, f) in &t.expected_graphs {
        for (s, p, o) in parse_graph_file(f) {
            expected.push(vec![Some(T::Iri(label.clone())), Some(s), Some(p), Some(o)]);
        }
    }
    let expected = dataset_rows(expected);
    let actual = dataset_rows(actual);
    if !rows_match(&expected, &actual, false, &mut HashMap::new()) {
        return Err(format!(
            "datasets differ ({} expected vs {} actual quads)\nexpected: {expected:?}\nactual:   {actual:?}",
            expected.len(),
            actual.len(),
        ));
    }
    Ok(())
}

#[test]
fn sparql11_update_evaluation() {
    let Some(root) = suite_root() else {
        eprintln!("skipping: testdata/rdf-tests not present");
        return;
    };
    let dirs = [
        "add",
        "basic-update",
        "clear",
        "copy",
        "delete",
        "delete-data",
        "delete-insert",
        "delete-where",
        "drop",
        "move",
        "update-silent",
    ];
    let mut tests = Vec::new();
    for d in dirs {
        collect_update_manifest(
            &root.join("sparql11").join(d).join("manifest.ttl"),
            &mut tests,
        );
    }
    let mut ran = 0usize;
    let mut failures: Vec<String> = Vec::new();
    for t in &tests {
        ran += 1;
        let outcome = std::panic::catch_unwind(|| run_update_test(t)).unwrap_or_else(|p| {
            let msg = p
                .downcast_ref::<String>()
                .cloned()
                .or_else(|| p.downcast_ref::<&str>().map(|s| (*s).to_owned()))
                .unwrap_or_else(|| "panic".into());
            Err(format!("panicked: {msg}"))
        });
        if let Err(e) = outcome {
            failures.push(format!(
                "{} [{}]: {e}",
                t.name,
                t.request.file_name().unwrap_or_default().to_string_lossy(),
            ));
        }
    }
    assert!(
        failures.is_empty(),
        "{} of {} failed:\n{}",
        failures.len(),
        ran,
        failures.join("\n---\n")
    );
    println!("sparql11 update evaluation: {ran} tests green");
}

#[test]
fn sparql12_update_evaluation() {
    let Some(root) = suite_root() else {
        eprintln!("skipping: testdata/rdf-tests not present");
        return;
    };
    let mut tests = Vec::new();
    collect_update_manifest(
        &root
            .join("sparql12")
            .join("eval-triple-terms")
            .join("manifest.ttl"),
        &mut tests,
    );
    let mut failures = Vec::new();
    for t in &tests {
        let outcome = std::panic::catch_unwind(|| run_update_test(t)).unwrap_or_else(|p| {
            let msg = p
                .downcast_ref::<String>()
                .cloned()
                .or_else(|| p.downcast_ref::<&str>().map(|s| (*s).to_owned()))
                .unwrap_or_else(|| "panic".into());
            Err(format!("panicked: {msg}"))
        });
        if let Err(e) = outcome {
            failures.push(format!(
                "{} [{}]: {e}",
                t.name,
                t.request.file_name().unwrap_or_default().to_string_lossy(),
            ));
        }
    }
    assert!(
        failures.is_empty(),
        "{} of {} failed:\n{}",
        failures.len(),
        tests.len(),
        failures.join("\n---\n")
    );
    println!("sparql12 update evaluation: {} tests green", tests.len());
}
