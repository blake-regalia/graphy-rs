//! HDT dictionary string ↔ concise-term conversion. HDT stores terms as
//! raw strings: IRIs bare (no angle brackets), blank nodes as `_:label`,
//! literals with their quotes and `@lang` / `^^<datatype>` suffix, lexical
//! forms unescaped (raw UTF-8).

use graphy_core::{concise, Dir, Term, TermRef};

use crate::HdtError;

/// HDT string → concise term bytes (validating; import trust boundary).
pub(crate) fn hdt_to_concise(s: &str) -> Result<Vec<u8>, HdtError> {
    let bad = |m: String| HdtError::Format(m);
    let term = if let Some(rest) = s.strip_prefix("\"") {
        // Literal: lexical runs to the LAST quote (datatype IRIs and lang
        // tags cannot contain one); suffix is empty, @lang, or ^^<dt>.
        let close = rest
            .rfind('"')
            .ok_or_else(|| bad(format!("unterminated literal {s:?}")))?;
        let lexical = &rest[..close];
        let suffix = &rest[close + 1..];
        if suffix.is_empty() {
            Term::literal_simple(lexical)
        } else if let Some(tag) = suffix.strip_prefix('@') {
            // Accept our own `@lang--dir` round-trip spelling (RDF 1.2
            // directional literals are not representable in standard HDT).
            let (tag, dir) = match tag.rsplit_once("--") {
                Some((t, "ltr")) => (t, Some(Dir::Ltr)),
                Some((t, "rtl")) => (t, Some(Dir::Rtl)),
                _ => (tag, None),
            };
            Term::literal_lang(lexical, tag, dir)
                .map_err(|e| bad(format!("bad language literal {s:?}: {e}")))?
        } else if let Some(dt) = suffix.strip_prefix("^^<").and_then(|d| d.strip_suffix('>')) {
            Term::literal_typed(lexical, dt)
                .map_err(|e| bad(format!("bad typed literal {s:?}: {e}")))?
        } else {
            return Err(bad(format!("bad literal suffix {suffix:?} in {s:?}")));
        }
    } else if let Some(label) = s.strip_prefix("_:") {
        Term::blank_node(label).map_err(|e| bad(format!("bad blank node {s:?}: {e}")))?
    } else {
        Term::iri(s).map_err(|e| bad(format!("bad IRI {s:?}: {e}")))?
    };
    Ok(term.as_concise().to_vec())
}

/// Concise term bytes → HDT string. Triple terms are not representable in
/// HDT and error out.
pub(crate) fn concise_to_hdt(bytes: &[u8]) -> Result<String, HdtError> {
    let term =
        concise::decode(bytes).map_err(|e| HdtError::Format(format!("bad concise term: {e}")))?;
    Ok(match term {
        TermRef::Iri(i) => i.to_owned(),
        TermRef::BlankNode(b) => format!("_:{b}"),
        TermRef::Literal(l) => {
            if let Some((tag, dir)) = l.lang() {
                match dir {
                    None => format!("\"{}\"@{tag}", l.lexical()),
                    // Our own lossless spelling; standard HDT has no
                    // directional literals.
                    Some(Dir::Ltr) => format!("\"{}\"@{tag}--ltr", l.lexical()),
                    Some(Dir::Rtl) => format!("\"{}\"@{tag}--rtl", l.lexical()),
                }
            } else if l.datatype() == graphy_core::vocab::XSD_STRING {
                format!("\"{}\"", l.lexical())
            } else {
                format!("\"{}\"^^<{}>", l.lexical(), l.datatype())
            }
        }
        TermRef::TripleTerm(_) => {
            return Err(HdtError::Format(
                "RDF 1.2 triple terms are not representable in HDT".into(),
            ))
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn conversions_round_trip() {
        for s in [
            "http://example.org/x",
            "_:b0",
            "\"plain\"",
            "\"hello \\\" world\"",
            "\"chat\"@fr",
            "\"5\"^^<http://www.w3.org/2001/XMLSchema#integer>",
            "\"x\"@ar--rtl",
        ] {
            let concise = hdt_to_concise(s).unwrap();
            assert_eq!(concise_to_hdt(&concise).unwrap(), s, "{s}");
        }
    }
}
