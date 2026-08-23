//! The concise term encoding (doc 01 §3), adapted from graphy.js "c1".
//!
//! Each term is one byte string whose **first byte is a type sigil**; the
//! remainder is content with no closing delimiter and no escaping (the term's
//! length is always known from its container). Plain byte order over concise
//! strings is the total order *(term kind, kind-specific fields, value)*.
//!
//! ```text
//! >http://…            IRI (absolute)
//! _label               blank node
//! "lexical             simple literal (xsd:string)
//! @en"lexical          language literal
//! @ar--rtl"lexical     directional language literal (RDF 1.2)
//! ^>dt-iri"lexical     datatyped literal
//! \t …                 triple term (see below)
//! ```
//!
//! Standalone triple terms (self-contained, storage-independent form) encode
//! as `0x09` followed by the three components, each as `varint(len) bytes`.
//! (In a base segment, triple terms are instead ordinals into the
//! `TRIPLE_TERMS` section — that form never leaves `graphy-store`.)

use crate::term::{Dir, TermRef, TripleTermRef};
use crate::{iri, varint, vocab, Result, TermError};

/// Type sigils, in byte-order position. Ordering consequence:
/// `\t` triple terms < `"` simple literals < `>` IRIs < `@` lang literals
/// < `^` datatyped literals < `_` blank nodes.
pub mod sigil {
    pub const TRIPLE_TERM: u8 = 0x09;
    pub const SIMPLE: u8 = b'"'; // 0x22
    pub const IRI: u8 = b'>'; // 0x3E
    pub const LANG: u8 = b'@'; // 0x40
    pub const DATATYPE: u8 = b'^'; // 0x5E
    pub const BLANK: u8 = b'_'; // 0x5F
}

/// Maximum triple-term nesting depth accepted by decode/encode.
pub const MAX_TRIPLE_TERM_DEPTH: usize = 32;

// ---------------------------------------------------------------- encoding

pub fn encode_iri(out: &mut Vec<u8>, iri: &str) {
    out.push(sigil::IRI);
    out.extend_from_slice(iri.as_bytes());
}

pub fn encode_blank(out: &mut Vec<u8>, label: &str) {
    out.push(sigil::BLANK);
    out.extend_from_slice(label.as_bytes());
}

pub fn encode_simple(out: &mut Vec<u8>, lexical: &str) {
    out.push(sigil::SIMPLE);
    out.extend_from_slice(lexical.as_bytes());
}

/// `tag` must already be lowercase-normalized (see [`validate_lang_tag`]).
pub fn encode_lang(out: &mut Vec<u8>, lexical: &str, tag: &str, dir: Option<Dir>) {
    out.push(sigil::LANG);
    out.extend_from_slice(tag.as_bytes());
    if let Some(d) = dir {
        out.extend_from_slice(match d {
            Dir::Ltr => b"--ltr",
            Dir::Rtl => b"--rtl",
        });
    }
    out.push(sigil::SIMPLE);
    out.extend_from_slice(lexical.as_bytes());
}

pub fn encode_datatype(out: &mut Vec<u8>, lexical: &str, datatype_iri: &str) {
    out.push(sigil::DATATYPE);
    out.push(sigil::IRI);
    out.extend_from_slice(datatype_iri.as_bytes());
    out.push(sigil::SIMPLE);
    out.extend_from_slice(lexical.as_bytes());
}

/// Encode a standalone triple term from already-encoded component terms.
pub fn encode_triple_term(out: &mut Vec<u8>, s: &[u8], p: &[u8], o: &[u8]) {
    out.push(sigil::TRIPLE_TERM);
    for part in [s, p, o] {
        varint::write(out, part.len() as u64);
        out.extend_from_slice(part);
    }
}

// ---------------------------------------------------------------- decoding

/// Decode and validate a concise byte string into a borrowed term view.
///
/// This is the trust boundary: bytes accepted here are valid UTF-8 with
/// structurally valid components. Bytes produced by this crate's encoders
/// from validated inputs always decode successfully.
pub fn decode(bytes: &[u8]) -> Result<TermRef<'_>> {
    decode_at_depth(bytes, 0)
}

pub(crate) fn decode_at_depth(bytes: &[u8], depth: usize) -> Result<TermRef<'_>> {
    if depth > MAX_TRIPLE_TERM_DEPTH {
        return Err(TermError::TripleTermDepth(MAX_TRIPLE_TERM_DEPTH));
    }
    let (&sig, rest) = bytes
        .split_first()
        .ok_or(TermError::InvalidConcise("empty"))?;
    match sig {
        sigil::IRI => {
            let s = utf8(rest)?;
            iri::validate_iri(s)?;
            Ok(TermRef::Iri(s))
        }
        sigil::BLANK => {
            let label = utf8(rest)?;
            validate_blank_label(label)?;
            Ok(TermRef::BlankNode(label))
        }
        sigil::SIMPLE => Ok(TermRef::simple(utf8(rest)?)),
        sigil::LANG => {
            let quote = rest
                .iter()
                .position(|&b| b == sigil::SIMPLE)
                .ok_or(TermError::InvalidConcise("language literal missing '\"'"))?;
            let (head, lex) = (&rest[..quote], &rest[quote + 1..]);
            let head = utf8(head)?;
            let (tag, dir) = match head.rfind("--") {
                Some(i) => {
                    let dir = match &head[i + 2..] {
                        "ltr" => Dir::Ltr,
                        "rtl" => Dir::Rtl,
                        _ => return Err(TermError::InvalidConcise("bad base direction")),
                    };
                    (&head[..i], Some(dir))
                }
                None => (head, None),
            };
            validate_lang_tag_normalized(tag)?;
            Ok(TermRef::lang(utf8(lex)?, tag, dir))
        }
        sigil::DATATYPE => {
            let (&mark, rest) = rest
                .split_first()
                .ok_or(TermError::InvalidConcise("datatyped literal missing '>'"))?;
            if mark != sigil::IRI {
                return Err(TermError::InvalidConcise("datatyped literal missing '>'"));
            }
            let quote = rest
                .iter()
                .position(|&b| b == sigil::SIMPLE)
                .ok_or(TermError::InvalidConcise("datatyped literal missing '\"'"))?;
            let dt = utf8(&rest[..quote])?;
            iri::validate_iri(dt)?;
            if dt == vocab::XSD_STRING
                || dt == vocab::RDF_LANG_STRING
                || dt == vocab::RDF_DIR_LANG_STRING
            {
                // These have dedicated concise forms; a `^` encoding of them
                // would create a second byte representation of the same term
                // and break bytes-equality ⇔ term-equality.
                return Err(TermError::InvalidConcise(
                    "datatype with a dedicated concise form",
                ));
            }
            Ok(TermRef::datatyped(utf8(&rest[quote + 1..])?, dt))
        }
        sigil::TRIPLE_TERM => {
            // Validate all three components now; the view decodes lazily later.
            let mut at = 0;
            for pos in 0..3 {
                let (len, n) = varint::read(&rest[at..])
                    .ok_or(TermError::InvalidConcise("truncated triple term"))?;
                at += n;
                let end = at
                    .checked_add(len as usize)
                    .filter(|&e| e <= rest.len())
                    .ok_or(TermError::InvalidConcise("truncated triple term"))?;
                let component = decode_at_depth(&rest[at..end], depth + 1)?;
                match (pos, &component) {
                    // Subject: IRI or blank node only (RDF 1.2).
                    (0, TermRef::Iri(_) | TermRef::BlankNode(_)) => {}
                    (0, _) => return Err(TermError::InvalidPosition("triple term subject")),
                    // Predicate: IRI only.
                    (1, TermRef::Iri(_)) => {}
                    (1, _) => return Err(TermError::InvalidPosition("triple term predicate")),
                    // Object: any term.
                    _ => {}
                }
                at = end;
            }
            if at != rest.len() {
                return Err(TermError::InvalidConcise("trailing bytes in triple term"));
            }
            Ok(TermRef::TripleTerm(TripleTermRef::from_payload(rest)))
        }
        _ => Err(TermError::InvalidConcise("unknown sigil")),
    }
}

fn utf8(bytes: &[u8]) -> Result<&str> {
    std::str::from_utf8(bytes).map_err(|_| TermError::InvalidUtf8)
}

// -------------------------------------------------------------- validation

/// BCP47-shaped language tag: `alpha{1,8} ("-" alphanum{1,8})*`.
/// Returns the lowercase-normalized tag (RDF compares tags case-insensitively;
/// we normalize once at construction so byte equality is term equality).
pub fn normalize_lang_tag(tag: &str) -> Result<String> {
    validate_lang_tag_shape(tag)?;
    Ok(tag.to_ascii_lowercase())
}

fn validate_lang_tag_shape(tag: &str) -> Result<()> {
    let mut subtags = tag.split('-');
    let primary = subtags.next().unwrap_or("");
    if primary.is_empty() || primary.len() > 8 || !primary.bytes().all(|b| b.is_ascii_alphabetic())
    {
        return Err(TermError::InvalidLangTag(tag.to_owned()));
    }
    for sub in subtags {
        if sub.is_empty() || sub.len() > 8 || !sub.bytes().all(|b| b.is_ascii_alphanumeric()) {
            return Err(TermError::InvalidLangTag(tag.to_owned()));
        }
    }
    Ok(())
}

/// Stored tags must additionally already be lowercase.
fn validate_lang_tag_normalized(tag: &str) -> Result<()> {
    validate_lang_tag_shape(tag)?;
    if tag.bytes().any(|b| b.is_ascii_uppercase()) {
        return Err(TermError::InvalidLangTag(tag.to_owned()));
    }
    Ok(())
}

/// Blank-node labels use the RDF text `BLANK_NODE_LABEL` grammar. Keeping
/// the owned term boundary serialization-safe guarantees that every
/// successfully constructed/decoded blank node can be emitted by the
/// N-Triples and N-Quads writers without inventing a second identity map.
/// Parsers already map surface labels to labels in this subset.
pub fn validate_blank_label(label: &str) -> Result<()> {
    let mut chars = label.chars();
    let Some(first) = chars.next() else {
        return Err(TermError::InvalidBlankNodeLabel(label.to_owned()));
    };
    if !(crate::text::is_pn_chars_u(first) || first.is_ascii_digit()) {
        return Err(TermError::InvalidBlankNodeLabel(label.to_owned()));
    }
    let mut last = first;
    for c in chars {
        if !(crate::text::is_pn_chars(c) || c == '.') {
            return Err(TermError::InvalidBlankNodeLabel(label.to_owned()));
        }
        last = c;
    }
    if last == '.' {
        return Err(TermError::InvalidBlankNodeLabel(label.to_owned()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sigil_order_groups_kinds() {
        // Byte order clusters by sigil: triple < simple < iri < lang < datatyped < blank.
        let mut v: Vec<Vec<u8>> = Vec::new();
        let mut b = Vec::new();
        encode_simple(&mut b, "zzz");
        v.push(std::mem::take(&mut b));
        encode_iri(&mut b, "http://a.example/");
        v.push(std::mem::take(&mut b));
        encode_lang(&mut b, "Banana", "en", None);
        v.push(std::mem::take(&mut b));
        encode_datatype(&mut b, "42", crate::vocab::XSD_INTEGER);
        v.push(std::mem::take(&mut b));
        encode_blank(&mut b, "aaa");
        v.push(std::mem::take(&mut b));
        let mut sorted = v.clone();
        sorted.sort();
        assert_eq!(v, sorted);
    }

    #[test]
    fn lang_literals_group_by_tag_then_value() {
        let mut a = Vec::new();
        encode_lang(&mut a, "Banane", "fr", None);
        let mut b = Vec::new();
        encode_lang(&mut b, "Banana", "en", None);
        let mut c = Vec::new();
        encode_lang(&mut c, "Apple", "en", None);
        assert!(c < b && b < a);
    }

    #[test]
    fn blank_labels_are_safe_for_rdf_text_writers() {
        for valid in ["a", "0", "_x", "a.b", "é"] {
            assert!(validate_blank_label(valid).is_ok(), "{valid:?}");
        }
        for invalid in ["", "a/b", "-a", ".a", "a.", "a:b", "a b"] {
            assert!(validate_blank_label(invalid).is_err(), "{invalid:?}");
        }
    }

    #[test]
    fn decode_rejects_garbage() {
        assert!(decode(b"").is_err());
        assert!(decode(b"!nope").is_err());
        assert!(decode(b">relative").is_err());
        assert!(decode(b"@EN\"upper tag stored").is_err());
        assert!(decode(b"@en-\"trailing hyphen").is_err());
        assert!(decode(b"^>http://x/\xff\"bad utf8").is_err());
        // xsd:string via `^` is a forbidden second spelling of a simple literal.
        let mut b = Vec::new();
        encode_datatype(&mut b, "x", crate::vocab::XSD_STRING);
        assert!(decode(&b).is_err());
    }

    #[test]
    fn literal_contents_need_no_escaping() {
        let evil = "she said \"hi\"\nnew ^line @and >more";
        let mut b = Vec::new();
        encode_simple(&mut b, evil);
        match decode(&b).unwrap() {
            TermRef::Literal(l) => assert_eq!(l.lexical(), evil),
            _ => panic!(),
        }
        let mut b = Vec::new();
        encode_lang(&mut b, evil, "en", Some(Dir::Ltr));
        match decode(&b).unwrap() {
            TermRef::Literal(l) => {
                assert_eq!(l.lexical(), evil);
                assert_eq!(l.lang(), Some(("en", Some(Dir::Ltr))));
            }
            _ => panic!(),
        }
    }
}
