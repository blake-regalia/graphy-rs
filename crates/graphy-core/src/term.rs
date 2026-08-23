//! Borrowed and owned RDF term types (doc 01 §2).
//!
//! [`TermRef`] is a zero-copy view produced by [`concise::decode`]; [`Term`]
//! owns a validated concise byte string. Because the concise encoding is
//! canonical (one byte spelling per term), byte equality is term equality and
//! `Term`'s derived `Ord` — plain byte order — is the total term order
//! *(kind, kind-specific fields, value)*.

use crate::{concise, iri, varint, vocab, Result, TermError};

/// Base direction of a directional language-tagged string (RDF 1.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Dir {
    Ltr,
    Rtl,
}

/// A borrowed view of a decoded term. Lifetimes borrow from the concise bytes
/// (or, for constructor-made values, from the caller's strings).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TermRef<'a> {
    Iri(&'a str),
    BlankNode(&'a str),
    Literal(LiteralParts<'a>),
    TripleTerm(TripleTermRef<'a>),
}

impl<'a> TermRef<'a> {
    /// A simple literal (datatype xsd:string).
    pub fn simple(lexical: &'a str) -> Self {
        TermRef::Literal(LiteralParts {
            lexical,
            kind: LitKind::Simple,
        })
    }

    /// A language-tagged literal; `tag` must already be lowercase-normalized.
    pub fn lang(lexical: &'a str, tag: &'a str, dir: Option<Dir>) -> Self {
        TermRef::Literal(LiteralParts {
            lexical,
            kind: LitKind::Lang { tag, dir },
        })
    }

    /// A datatyped literal. `datatype` must not be one of the datatypes with a
    /// dedicated concise form (xsd:string, rdf:langString, rdf:dirLangString);
    /// the encoder/decoder enforce that invariant.
    pub fn datatyped(lexical: &'a str, datatype: &'a str) -> Self {
        TermRef::Literal(LiteralParts {
            lexical,
            kind: LitKind::Datatyped { datatype },
        })
    }
}

/// The pieces of a literal term.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LiteralParts<'a> {
    lexical: &'a str,
    kind: LitKind<'a>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LitKind<'a> {
    Simple,
    Lang { tag: &'a str, dir: Option<Dir> },
    Datatyped { datatype: &'a str },
}

impl<'a> LiteralParts<'a> {
    pub fn lexical(&self) -> &'a str {
        self.lexical
    }

    /// Language tag and optional base direction, if language-tagged.
    pub fn lang(&self) -> Option<(&'a str, Option<Dir>)> {
        match self.kind {
            LitKind::Lang { tag, dir } => Some((tag, dir)),
            _ => None,
        }
    }

    /// The datatype IRI, materializing the implicit datatypes of the dedicated
    /// concise forms (xsd:string / rdf:langString / rdf:dirLangString).
    pub fn datatype(&self) -> &'a str {
        match self.kind {
            LitKind::Simple => vocab::XSD_STRING,
            LitKind::Lang { dir: None, .. } => vocab::RDF_LANG_STRING,
            LitKind::Lang { dir: Some(_), .. } => vocab::RDF_DIR_LANG_STRING,
            LitKind::Datatyped { datatype } => datatype,
        }
    }
}

/// A borrowed triple term: a lazy view over the concise triple-term *payload*
/// (the bytes after the sigil: three components, each `varint(len) bytes`).
///
/// Since concise bytes are canonical, derived `PartialEq` (payload byte
/// equality) is triple-term equality.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TripleTermRef<'a> {
    payload: &'a [u8],
}

impl<'a> TripleTermRef<'a> {
    /// Wrap payload bytes that `concise::decode` has already fully validated.
    pub(crate) fn from_payload(payload: &'a [u8]) -> Self {
        TripleTermRef { payload }
    }

    pub fn subject(&self) -> TermRef<'a> {
        self.component(0)
    }

    pub fn predicate(&self) -> TermRef<'a> {
        self.component(1)
    }

    pub fn object(&self) -> TermRef<'a> {
        self.component(2)
    }

    /// The raw payload bytes (post-sigil).
    pub fn payload(&self) -> &'a [u8] {
        self.payload
    }

    fn component(&self, want: usize) -> TermRef<'a> {
        // The payload was fully validated (structure, UTF-8, nesting depth,
        // positional constraints) when this view was constructed, so lazy
        // component decode cannot fail; a panic here means the invariant was
        // violated by construction of the view over unvalidated bytes.
        let mut at = 0;
        for i in 0..=want {
            let (len, n) =
                varint::read(&self.payload[at..]).expect("validated triple-term payload");
            at += n;
            let end = at + len as usize;
            if i == want {
                return concise::decode_at_depth(&self.payload[at..end], 0)
                    .expect("validated triple-term payload");
            }
            at = end;
        }
        unreachable!()
    }
}

/// An owned term: a validated concise byte string.
///
/// Derived `Ord` compares the boxed bytes lexicographically, which by the
/// canonical-encoding invariant is exactly the total term order.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Term(Box<[u8]>);

impl Term {
    /// Adopt concise bytes, validating them via [`concise::decode`].
    pub fn from_concise(bytes: &[u8]) -> Result<Term> {
        concise::decode(bytes)?;
        Ok(Term(bytes.into()))
    }

    /// The concise byte encoding of this term.
    pub fn as_concise(&self) -> &[u8] {
        &self.0
    }

    /// Decode into a borrowed view. Infallible by the construction invariant
    /// (every `Term` holds bytes that passed `concise::decode`).
    pub fn as_term_ref(&self) -> TermRef<'_> {
        concise::decode(&self.0).expect("Term bytes validated at construction")
    }

    pub fn iri(iri: &str) -> Result<Term> {
        iri::validate_iri(iri)?;
        let mut buf = Vec::with_capacity(1 + iri.len());
        concise::encode_iri(&mut buf, iri);
        Ok(Term(buf.into_boxed_slice()))
    }

    pub fn blank_node(label: &str) -> Result<Term> {
        concise::validate_blank_label(label)?;
        let mut buf = Vec::with_capacity(1 + label.len());
        concise::encode_blank(&mut buf, label);
        Ok(Term(buf.into_boxed_slice()))
    }

    /// A simple literal. Infallible: every string is a valid xsd:string.
    pub fn literal_simple(lexical: &str) -> Term {
        let mut buf = Vec::with_capacity(1 + lexical.len());
        concise::encode_simple(&mut buf, lexical);
        Term(buf.into_boxed_slice())
    }

    /// A language-tagged literal; the tag is lowercase-normalized here.
    pub fn literal_lang(lexical: &str, tag: &str, dir: Option<Dir>) -> Result<Term> {
        let tag = concise::normalize_lang_tag(tag)?;
        let mut buf = Vec::with_capacity(lexical.len() + tag.len() + 8);
        concise::encode_lang(&mut buf, lexical, &tag, dir);
        Ok(Term(buf.into_boxed_slice()))
    }

    /// A datatyped literal. `xsd:string` folds into the simple form (single
    /// spelling per term); the langString datatypes require a tag and must go
    /// through [`Term::literal_lang`].
    pub fn literal_typed(lexical: &str, datatype_iri: &str) -> Result<Term> {
        if datatype_iri == vocab::XSD_STRING {
            return Ok(Term::literal_simple(lexical));
        }
        if datatype_iri == vocab::RDF_LANG_STRING || datatype_iri == vocab::RDF_DIR_LANG_STRING {
            return Err(TermError::LangStringWithoutTag);
        }
        iri::validate_iri(datatype_iri)?;
        let mut buf = Vec::with_capacity(lexical.len() + datatype_iri.len() + 3);
        concise::encode_datatype(&mut buf, lexical, datatype_iri);
        Ok(Term(buf.into_boxed_slice()))
    }

    /// A triple term. Positional constraints (subject ∈ {IRI, blank node},
    /// predicate = IRI) and nesting depth are validated by re-decoding the
    /// encoded bytes — the components themselves are already valid terms.
    pub fn triple_term(s: &Term, p: &Term, o: &Term) -> Result<Term> {
        let mut buf = Vec::with_capacity(
            s.as_concise().len() + p.as_concise().len() + o.as_concise().len() + 7,
        );
        concise::encode_triple_term(&mut buf, s.as_concise(), p.as_concise(), o.as_concise());
        concise::decode(&buf)?;
        Ok(Term(buf.into_boxed_slice()))
    }
}

/// A graph name: the default graph or a named graph (doc 01 §7).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum GraphName<T = Term> {
    Default,
    Named(T),
}

/// A generic triple; `T` is [`Term`] at the API boundary and `TermId` inside
/// the store.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Triple<T = Term> {
    pub s: T,
    pub p: T,
    pub o: T,
}

/// A generic quad (doc 01 §7).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Quad<T = Term> {
    pub s: T,
    pub p: T,
    pub o: T,
    pub g: GraphName<T>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn constructors_round_trip() {
        let t = Term::iri("http://ex.example/a").unwrap();
        assert_eq!(t.as_term_ref(), TermRef::Iri("http://ex.example/a"));

        let t = Term::blank_node("b0").unwrap();
        assert_eq!(t.as_term_ref(), TermRef::BlankNode("b0"));

        let t = Term::literal_simple("hi");
        match t.as_term_ref() {
            TermRef::Literal(l) => {
                assert_eq!(l.lexical(), "hi");
                assert_eq!(l.lang(), None);
                assert_eq!(l.datatype(), vocab::XSD_STRING);
            }
            _ => panic!(),
        }

        let t = Term::literal_lang("Hallo", "DE", Some(Dir::Ltr)).unwrap();
        match t.as_term_ref() {
            TermRef::Literal(l) => {
                // Tag normalized to lowercase at construction.
                assert_eq!(l.lang(), Some(("de", Some(Dir::Ltr))));
                assert_eq!(l.datatype(), vocab::RDF_DIR_LANG_STRING);
            }
            _ => panic!(),
        }

        let t = Term::literal_typed("42", vocab::XSD_INTEGER).unwrap();
        match t.as_term_ref() {
            TermRef::Literal(l) => assert_eq!(l.datatype(), vocab::XSD_INTEGER),
            _ => panic!(),
        }
    }

    #[test]
    fn typed_string_folds_to_simple() {
        let typed = Term::literal_typed("x", vocab::XSD_STRING).unwrap();
        assert_eq!(typed, Term::literal_simple("x"));
    }

    #[test]
    fn lang_string_datatypes_rejected_without_tag() {
        assert_eq!(
            Term::literal_typed("x", vocab::RDF_LANG_STRING),
            Err(TermError::LangStringWithoutTag)
        );
        assert_eq!(
            Term::literal_typed("x", vocab::RDF_DIR_LANG_STRING),
            Err(TermError::LangStringWithoutTag)
        );
    }

    #[test]
    fn triple_term_components() {
        let s = Term::blank_node("s").unwrap();
        let p = Term::iri("http://ex.example/p").unwrap();
        let o = Term::literal_lang("o", "en", None).unwrap();
        let tt = Term::triple_term(&s, &p, &o).unwrap();
        match tt.as_term_ref() {
            TermRef::TripleTerm(view) => {
                assert_eq!(view.subject(), s.as_term_ref());
                assert_eq!(view.predicate(), p.as_term_ref());
                assert_eq!(view.object(), o.as_term_ref());
            }
            _ => panic!(),
        }
        // Nested triple term as object.
        let nested = Term::triple_term(&s, &p, &tt).unwrap();
        match nested.as_term_ref() {
            TermRef::TripleTerm(view) => assert_eq!(view.object(), tt.as_term_ref()),
            _ => panic!(),
        }
    }

    #[test]
    fn triple_term_positional_constraints() {
        let iri = Term::iri("http://ex.example/x").unwrap();
        let lit = Term::literal_simple("nope");
        let bn = Term::blank_node("b").unwrap();
        // Literal subject rejected.
        assert!(Term::triple_term(&lit, &iri, &iri).is_err());
        // Blank-node predicate rejected.
        assert!(Term::triple_term(&iri, &bn, &iri).is_err());
        // Literal object fine.
        assert!(Term::triple_term(&iri, &iri, &lit).is_ok());
    }

    #[test]
    fn from_concise_validates() {
        let good = Term::literal_simple("ok");
        assert_eq!(Term::from_concise(good.as_concise()).unwrap(), good);
        assert!(Term::from_concise(b">not absolute").is_err());
        assert!(Term::from_concise(b"").is_err());
    }

    #[test]
    fn term_order_is_byte_order() {
        let mut terms = [
            Term::blank_node("z").unwrap(),
            Term::literal_simple("a"),
            Term::iri("http://ex.example/").unwrap(),
            Term::literal_lang("a", "en", None).unwrap(),
        ];
        terms.sort();
        let bytes: Vec<&[u8]> = terms.iter().map(Term::as_concise).collect();
        let mut sorted = bytes.clone();
        sorted.sort();
        assert_eq!(bytes, sorted);
    }
}
