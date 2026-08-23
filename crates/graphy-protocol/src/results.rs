//! SPARQL results serialization (doc 06 §3): SPARQL 1.1 Results JSON /
//! XML, CSV, TSV for solutions and booleans; N-Triples (a Turtle
//! subset, so it serves both media types) for CONSTRUCT / DESCRIBE
//! graphs. Terms arrive as concise bytes and classify through
//! `graphy_core::concise::decode`; N-Triples/TSV term text reuses the
//! canonical `graphy_turtle::write_term`.

use graphy_core::{concise, Dir, TermRef};
use serde_json::{json, Value};

/// Solutions / boolean / triples — mirrors `graphy_engine::Output`.
#[derive(Debug)]
pub enum Results {
    Solutions {
        vars: Vec<String>,
        rows: Vec<Vec<Option<Vec<u8>>>>,
    },
    Boolean(bool),
    Triples(Vec<(Vec<u8>, Vec<u8>, Vec<u8>)>),
}

fn term_json(bytes: &[u8]) -> Value {
    match concise::decode(bytes) {
        Ok(TermRef::Iri(i)) => json!({"type": "uri", "value": i}),
        Ok(TermRef::BlankNode(l)) => json!({"type": "bnode", "value": l}),
        Ok(TermRef::Literal(l)) => {
            let mut o = serde_json::Map::new();
            o.insert("type".into(), "literal".into());
            o.insert("value".into(), l.lexical().into());
            if let Some((tag, dir)) = l.lang() {
                o.insert("xml:lang".into(), tag.into());
                if let Some(dir) = dir {
                    // SPARQL 1.2 results: base direction.
                    o.insert(
                        "its:dir".into(),
                        match dir {
                            Dir::Ltr => "ltr",
                            Dir::Rtl => "rtl",
                        }
                        .into(),
                    );
                }
            } else if l.datatype() != graphy_core::vocab::XSD_STRING {
                o.insert("datatype".into(), l.datatype().into());
            }
            Value::Object(o)
        }
        Ok(TermRef::TripleTerm(v)) => {
            let part = |t: &TermRef<'_>| -> Value {
                let mut buf = Vec::new();
                write_concise(&mut buf, t);
                term_json(&buf)
            };
            json!({"type": "triple", "value": {
                "subject": part(&v.subject()),
                "predicate": part(&v.predicate()),
                "object": part(&v.object()),
            }})
        }
        Err(_) => json!({"type": "literal", "value": String::from_utf8_lossy(bytes)}),
    }
}

/// Re-encode a decoded component back to concise bytes (triple-term
/// recursion helper).
fn write_concise(out: &mut Vec<u8>, t: &TermRef<'_>) {
    match t {
        TermRef::Iri(i) => concise::encode_iri(out, i),
        TermRef::BlankNode(l) => concise::encode_blank(out, l),
        TermRef::Literal(l) => {
            if let Some((tag, dir)) = l.lang() {
                concise::encode_lang(out, l.lexical(), tag, dir);
            } else if l.datatype() == graphy_core::vocab::XSD_STRING {
                concise::encode_simple(out, l.lexical());
            } else {
                concise::encode_datatype(out, l.lexical(), l.datatype());
            }
        }
        TermRef::TripleTerm(v) => {
            let (mut s, mut p, mut o) = (Vec::new(), Vec::new(), Vec::new());
            write_concise(&mut s, &v.subject());
            write_concise(&mut p, &v.predicate());
            write_concise(&mut o, &v.object());
            concise::encode_triple_term(out, &s, &p, &o);
        }
    }
}

/// SPARQL 1.1 Query Results JSON.
pub fn to_json(r: &Results) -> String {
    match r {
        Results::Boolean(b) => json!({"head": {}, "boolean": b}).to_string(),
        Results::Solutions { vars, rows } => {
            let bindings: Vec<Value> = rows
                .iter()
                .map(|row| {
                    let mut o = serde_json::Map::new();
                    for (v, cell) in vars.iter().zip(row) {
                        if let Some(bytes) = cell {
                            o.insert(v.clone(), term_json(bytes));
                        }
                    }
                    Value::Object(o)
                })
                .collect();
            json!({"head": {"vars": vars}, "results": {"bindings": bindings}}).to_string()
        }
        Results::Triples(_) => unreachable!("graphs serialize as N-Triples"),
    }
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// SPARQL 1.1 Query Results XML.
pub fn to_xml(r: &Results) -> String {
    let directional = match r {
        Results::Solutions { rows, .. } => rows
            .iter()
            .flatten()
            .flatten()
            .any(|bytes| term_has_direction(bytes)),
        _ => false,
    };
    let mut out = String::from(
        "<?xml version=\"1.0\"?>\n<sparql xmlns=\"http://www.w3.org/2005/sparql-results#\"",
    );
    if directional {
        out.push_str(" xmlns:its=\"http://www.w3.org/2005/11/its\" its:version=\"2.0\"");
    }
    out.push('>');
    match r {
        Results::Boolean(b) => {
            out.push_str("<head/>");
            out.push_str(&format!("<boolean>{b}</boolean>"));
        }
        Results::Solutions { vars, rows } => {
            out.push_str("<head>");
            for v in vars {
                out.push_str(&format!("<variable name=\"{}\"/>", xml_escape(v)));
            }
            out.push_str("</head><results>");
            for row in rows {
                out.push_str("<result>");
                for (v, cell) in vars.iter().zip(row) {
                    let Some(bytes) = cell else { continue };
                    out.push_str(&format!("<binding name=\"{}\">", xml_escape(v)));
                    out.push_str(&term_xml(bytes));
                    out.push_str("</binding>");
                }
                out.push_str("</result>");
            }
            out.push_str("</results>");
        }
        Results::Triples(_) => unreachable!("graphs serialize as N-Triples"),
    }
    out.push_str("</sparql>");
    out
}

fn term_xml(bytes: &[u8]) -> String {
    match concise::decode(bytes) {
        Ok(term) => term_ref_xml(term),
        Err(_) => format!(
            "<literal>{}</literal>",
            xml_escape(&String::from_utf8_lossy(bytes))
        ),
    }
}

fn term_ref_xml(term: TermRef<'_>) -> String {
    match term {
        TermRef::Iri(i) => format!("<uri>{}</uri>", xml_escape(i)),
        TermRef::BlankNode(l) => format!("<bnode>{}</bnode>", xml_escape(l)),
        TermRef::Literal(l) => {
            if let Some((tag, dir)) = l.lang() {
                let dir = dir.map_or("", |d| match d {
                    Dir::Ltr => " its:dir=\"ltr\"",
                    Dir::Rtl => " its:dir=\"rtl\"",
                });
                format!(
                    "<literal xml:lang=\"{}\"{}>{}</literal>",
                    xml_escape(tag),
                    dir,
                    xml_escape(l.lexical())
                )
            } else if l.datatype() == graphy_core::vocab::XSD_STRING {
                format!("<literal>{}</literal>", xml_escape(l.lexical()))
            } else {
                format!(
                    "<literal datatype=\"{}\">{}</literal>",
                    xml_escape(l.datatype()),
                    xml_escape(l.lexical())
                )
            }
        }
        TermRef::TripleTerm(tt) => {
            format!(
                "<triple><subject>{}</subject><predicate>{}</predicate><object>{}</object></triple>",
                term_ref_xml(tt.subject()),
                term_ref_xml(tt.predicate()),
                term_ref_xml(tt.object()),
            )
        }
    }
}

fn term_has_direction(bytes: &[u8]) -> bool {
    fn has(term: TermRef<'_>) -> bool {
        match term {
            TermRef::Literal(l) => l.lang().is_some_and(|(_, dir)| dir.is_some()),
            TermRef::TripleTerm(tt) => has(tt.subject()) || has(tt.predicate()) || has(tt.object()),
            TermRef::Iri(_) | TermRef::BlankNode(_) => false,
        }
    }
    concise::decode(bytes).is_ok_and(has)
}

/// Term in SPARQL surface syntax (TSV cells, N-Triples components).
fn term_nt(bytes: &[u8]) -> String {
    match concise::decode(bytes) {
        Ok(t) => {
            let mut buf = Vec::new();
            let _ = graphy_turtle::write_term(&mut buf, t);
            String::from_utf8_lossy(&buf).into_owned()
        }
        Err(_) => String::from_utf8_lossy(bytes).into_owned(),
    }
}

/// SPARQL 1.1 CSV results (plain lexical projection).
pub fn to_csv(r: &Results) -> String {
    let Results::Solutions { vars, rows } = r else {
        // ASK has no CSV form; serve the lexical boolean.
        if let Results::Boolean(b) = r {
            return format!("{b}\r\n");
        }
        unreachable!("graphs serialize as N-Triples");
    };
    let esc = |s: &str| -> String {
        if s.contains([',', '"', '\n', '\r']) {
            format!("\"{}\"", s.replace('"', "\"\""))
        } else {
            s.to_owned()
        }
    };
    let mut out = String::new();
    out.push_str(&vars.iter().map(|v| esc(v)).collect::<Vec<_>>().join(","));
    out.push_str("\r\n");
    for row in rows {
        let cells: Vec<String> = row
            .iter()
            .map(|cell| {
                let Some(bytes) = cell else {
                    return String::new();
                };
                match concise::decode(bytes) {
                    Ok(TermRef::Iri(i)) => esc(i),
                    Ok(TermRef::BlankNode(l)) => format!("_:{l}"),
                    Ok(TermRef::Literal(l)) => esc(l.lexical()),
                    _ => esc(&term_nt(bytes)),
                }
            })
            .collect();
        out.push_str(&cells.join(","));
        out.push_str("\r\n");
    }
    out
}

/// SPARQL 1.1 TSV results (full term syntax).
pub fn to_tsv(r: &Results) -> String {
    let Results::Solutions { vars, rows } = r else {
        if let Results::Boolean(b) = r {
            return format!("{b}\n");
        }
        unreachable!("graphs serialize as N-Triples");
    };
    let mut out = String::new();
    out.push_str(
        &vars
            .iter()
            .map(|v| format!("?{v}"))
            .collect::<Vec<_>>()
            .join("\t"),
    );
    out.push('\n');
    for row in rows {
        let cells: Vec<String> = row
            .iter()
            .map(|c| c.as_deref().map(term_nt).unwrap_or_default())
            .collect();
        out.push_str(&cells.join("\t"));
        out.push('\n');
    }
    out
}

/// N-Triples (also valid Turtle) for CONSTRUCT / DESCRIBE graphs.
pub fn to_ntriples(r: &Results) -> String {
    let Results::Triples(triples) = r else {
        unreachable!("solutions serialize as results formats")
    };
    let mut out = String::new();
    for (s, p, o) in triples {
        out.push_str(&format!("{} {} {} .\n", term_nt(s), term_nt(p), term_nt(o)));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn xml_serializes_direction_and_triple_terms_per_sparql_12() {
        let mut s = Vec::new();
        concise::encode_iri(&mut s, "http://example/s");
        let mut p = Vec::new();
        concise::encode_iri(&mut p, "http://example/p");
        let mut o = Vec::new();
        concise::encode_lang(&mut o, "مرحبا", "ar", Some(Dir::Rtl));
        let mut triple = Vec::new();
        concise::encode_triple_term(&mut triple, &s, &p, &o);

        let xml = to_xml(&Results::Solutions {
            vars: vec!["t".into()],
            rows: vec![vec![Some(triple)]],
        });
        assert!(xml.contains("xmlns:its=\"http://www.w3.org/2005/11/its\" its:version=\"2.0\""));
        assert!(xml.contains(
            "<triple><subject><uri>http://example/s</uri></subject>\
             <predicate><uri>http://example/p</uri></predicate>\
             <object><literal xml:lang=\"ar\" its:dir=\"rtl\">مرحبا</literal></object></triple>"
        ));
        assert!(!xml.contains("&lt;&lt;"));
    }
}
