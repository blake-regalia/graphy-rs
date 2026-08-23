//! The semantic-token legend (docs/10 §7.1): a single set of types and
//! modifiers shared across every language, so the encoder and the LSP
//! `SemanticTokensLegend` capability stay in one place.

/// Semantic-token *type*. The discriminant is the legend index used in the
/// wire encoding; the names are standard LSP `SemanticTokenTypes` so stock
/// editor themes colour them without custom configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum SemKind {
    Namespace = 0,
    Type,
    Class,
    EnumMember,
    Variable,
    Property,
    String,
    Number,
    Keyword,
    Operator,
    Comment,
    Macro,
    Decorator,
}

impl SemKind {
    /// The legend, indexed by [`SemKind`] discriminant. Advertised verbatim in
    /// the server's `SemanticTokensLegend.tokenTypes`.
    pub const LEGEND: &'static [&'static str] = &[
        "namespace",
        "type",
        "class",
        "enumMember",
        "variable",
        "property",
        "string",
        "number",
        "keyword",
        "operator",
        "comment",
        "macro",
        "decorator",
    ];

    /// Index into [`LEGEND`](Self::LEGEND).
    pub fn index(self) -> u32 {
        self as u32
    }
}

/// Semantic-token *modifier*. The discriminant is the bit position in the
/// modifier bitset (docs/10 §7.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum SemMod {
    Declaration = 0,
    Readonly,
    Deprecated,
    DefaultLibrary,
}

impl SemMod {
    /// The modifier legend, indexed by [`SemMod`] discriminant. Advertised as
    /// the server's `SemanticTokensLegend.tokenModifiers`.
    pub const LEGEND: &'static [&'static str] =
        &["declaration", "readonly", "deprecated", "defaultLibrary"];

    /// This modifier's bit in the per-token modifier bitset.
    pub fn bit(self) -> u32 {
        1 << (self as u32)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discriminants_index_the_legend() {
        assert_eq!(SemKind::LEGEND.len(), 13);
        assert_eq!(
            SemKind::LEGEND[SemKind::Namespace.index() as usize],
            "namespace"
        );
        assert_eq!(
            SemKind::LEGEND[SemKind::Decorator.index() as usize],
            "decorator"
        );
        assert_eq!(
            SemMod::LEGEND[SemMod::DefaultLibrary as usize],
            "defaultLibrary"
        );
        assert_eq!(SemMod::DefaultLibrary.bit(), 0b1000);
    }
}
