/// Errors constructing or decoding terms.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TermError {
    #[error("invalid IRI ({reason}) at byte {pos}: {iri:?}")]
    InvalidIri {
        iri: String,
        pos: usize,
        reason: &'static str,
    },

    #[error("cannot resolve relative IRI reference against non-absolute base {base:?}")]
    RelativeBase { base: String },

    #[error("invalid language tag: {0:?}")]
    InvalidLangTag(String),

    #[error("invalid blank node label: {0:?}")]
    InvalidBlankNodeLabel(String),

    #[error("rdf:langString / rdf:dirLangString literals require a language tag; use a language-literal constructor")]
    LangStringWithoutTag,

    #[error("invalid concise term encoding: {0}")]
    InvalidConcise(&'static str),

    #[error("term is not valid UTF-8")]
    InvalidUtf8,

    #[error("triple term nesting exceeds the maximum depth of {0}")]
    TripleTermDepth(usize),

    #[error("{0} is not a valid term kind for this position")]
    InvalidPosition(&'static str),
}
