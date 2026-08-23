//! Token model (doc 04 §2): kinds carry classification only — the text is
//! always recovered through the [`Span`], so tokens stay `Copy` and the
//! stream is cheap to buffer for parser lookahead.

/// Byte range into the source string.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Span {
    pub start: u32,
    pub end: u32,
}

impl Span {
    pub fn text(self, src: &str) -> &str {
        &src[self.start as usize..self.end as usize]
    }

    pub fn len(self) -> u32 {
        self.end - self.start
    }

    pub fn is_empty(self) -> bool {
        self.start == self.end
    }
}

/// Initial text direction of a directional language tag (RDF 1.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dir {
    Ltr,
    Rtl,
}

/// Which quoting form a string literal used (needed to strip delimiters
/// and to know the escape dialect when decoding).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StringForm {
    /// `"…"`
    Quote,
    /// `'…'`
    Apos,
    /// `"""…"""`
    LongQuote,
    /// `'''…'''`
    LongApos,
}

impl StringForm {
    /// Delimiter length on each side.
    pub fn delim(self) -> u32 {
        match self {
            StringForm::Quote | StringForm::Apos => 1,
            StringForm::LongQuote | StringForm::LongApos => 3,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Token {
    pub kind: TokenKind,
    pub span: Span,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenKind {
    /// `<…>` including the angle brackets (SPARQL IRIREFs contain no
    /// escapes, unlike N-Triples).
    IriRef,
    /// `prefix:` (the span includes the colon; the prefix may be empty).
    PNameNs,
    /// `prefix:local` (local part may contain `%XX` and `\`-escapes).
    PNameLn,
    /// `_:label`
    BlankNode,
    /// `?name` or `$name` (span includes the sigil).
    Var,
    /// `@tag`, `@tag-sub`, `@tag--ltr` (direction carried out of band).
    LangTag(Option<Dir>),
    /// Unsigned or signed per the grammar's `INTEGER[_POSITIVE|_NEGATIVE]`
    /// (a directly attached sign is part of the token).
    Integer,
    Decimal,
    Double,
    True,
    False,
    /// String literal; the form tells delimiters and escape handling.
    String(StringForm),
    /// `( )` with only whitespace inside (the empty collection).
    Nil,
    /// `[ ]` with only whitespace inside (fresh blank node).
    Anon,
    /// Bare `a` (rdf:type shorthand in verb position).
    A,
    /// Case-insensitive reserved word.
    Keyword(Kw),

    LBrace,
    RBrace,
    LParen,
    RParen,
    LBracket,
    RBracket,
    Semicolon,
    Comma,
    Dot,
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    AndAnd,
    OrOr,
    Bang,
    Plus,
    Minus,
    Star,
    Slash,
    /// Path alternative `|`.
    Pipe,
    /// Path inverse `^`.
    Caret,
    /// Datatype marker `^^`.
    CaretCaret,
    /// Path `?` (a `?` not followed by a variable name).
    Question,
    /// `<<` (reified triple pattern open, SPARQL 1.2).
    LtLt,
    /// `>>`
    GtGt,
    /// `<<(` (triple term open, SPARQL 1.2 — one terminal, no interior
    /// whitespace).
    LtLtParen,
    /// `)>>`
    RParenGtGt,
    /// Reifier marker `~` (SPARQL 1.2).
    Tilde,
    /// Annotation block open `{|` (SPARQL 1.2).
    LBraceBar,
    /// Annotation block close `|}`.
    RBarBrace,
    /// An unlexable run. Only produced by [`tokenize_resilient`](crate::tokenize_resilient)
    /// for editor highlighting (docs/10 §3.2); the strict [`tokenize`](crate::tokenize)
    /// never emits it, so the parser never sees it.
    Error,
}

macro_rules! keywords {
    ($($variant:ident => $text:literal),+ $(,)?) => {
        /// Reserved words (SPARQL 1.1 Query + Update, and the 1.2
        /// additions), matched case-insensitively. Prefixed-name
        /// collisions cannot happen: a word followed by `:` lexes as a
        /// PNAME, never a keyword.
        #[derive(Debug, Clone, Copy, PartialEq, Eq)]
        pub enum Kw {
            $($variant),+
        }

        impl Kw {
            /// Look up an already-uppercased word.
            pub(crate) fn from_upper(word: &str) -> Option<Kw> {
                Some(match word {
                    $($text => Kw::$variant,)+
                    _ => return None,
                })
            }

            /// Canonical (uppercase) spelling.
            pub fn as_str(self) -> &'static str {
                match self {
                    $(Kw::$variant => $text),+
                }
            }

            /// Every keyword's canonical spelling (for editor completion).
            pub const ALL: &'static [&'static str] = &[$($text),+];
        }
    };
}

keywords! {
    // Prologue + query forms.
    Base => "BASE",
    Prefix => "PREFIX",
    Version => "VERSION",
    Select => "SELECT",
    Construct => "CONSTRUCT",
    Describe => "DESCRIBE",
    Ask => "ASK",
    // Dataset + patterns.
    From => "FROM",
    Named => "NAMED",
    Where => "WHERE",
    Graph => "GRAPH",
    Optional => "OPTIONAL",
    Union => "UNION",
    Filter => "FILTER",
    Minus => "MINUS",
    Bind => "BIND",
    Values => "VALUES",
    Undef => "UNDEF",
    Service => "SERVICE",
    Silent => "SILENT",
    // Solution modifiers.
    Distinct => "DISTINCT",
    Reduced => "REDUCED",
    As => "AS",
    Group => "GROUP",
    By => "BY",
    Having => "HAVING",
    Order => "ORDER",
    Asc => "ASC",
    Desc => "DESC",
    Limit => "LIMIT",
    Offset => "OFFSET",
    // Boolean-adjacent operators.
    Exists => "EXISTS",
    Not => "NOT",
    In => "IN",
    // Update.
    Insert => "INSERT",
    Delete => "DELETE",
    Data => "DATA",
    Load => "LOAD",
    Into => "INTO",
    Clear => "CLEAR",
    Create => "CREATE",
    Drop => "DROP",
    Copy => "COPY",
    Move => "MOVE",
    Add => "ADD",
    With => "WITH",
    Using => "USING",
    Default => "DEFAULT",
    All => "ALL",
    To => "TO",
    // Builtin functions (§17.4).
    Str => "STR",
    Lang => "LANG",
    LangMatches => "LANGMATCHES",
    Datatype => "DATATYPE",
    Bound => "BOUND",
    Iri => "IRI",
    Uri => "URI",
    BNode => "BNODE",
    Rand => "RAND",
    Abs => "ABS",
    Ceil => "CEIL",
    Floor => "FLOOR",
    Round => "ROUND",
    Concat => "CONCAT",
    StrLen => "STRLEN",
    UCase => "UCASE",
    LCase => "LCASE",
    EncodeForUri => "ENCODE_FOR_URI",
    Contains => "CONTAINS",
    StrStarts => "STRSTARTS",
    StrEnds => "STRENDS",
    StrBefore => "STRBEFORE",
    StrAfter => "STRAFTER",
    Year => "YEAR",
    Month => "MONTH",
    Day => "DAY",
    Hours => "HOURS",
    Minutes => "MINUTES",
    Seconds => "SECONDS",
    Timezone => "TIMEZONE",
    Tz => "TZ",
    Now => "NOW",
    Uuid => "UUID",
    StrUuid => "STRUUID",
    Md5 => "MD5",
    Sha1 => "SHA1",
    Sha256 => "SHA256",
    Sha384 => "SHA384",
    Sha512 => "SHA512",
    Coalesce => "COALESCE",
    If => "IF",
    StrLang => "STRLANG",
    StrDt => "STRDT",
    SameTerm => "SAMETERM",
    IsIri => "ISIRI",
    IsUri => "ISURI",
    IsBlank => "ISBLANK",
    IsLiteral => "ISLITERAL",
    IsNumeric => "ISNUMERIC",
    Regex => "REGEX",
    Substr => "SUBSTR",
    Replace => "REPLACE",
    // Aggregates.
    Count => "COUNT",
    Sum => "SUM",
    Min => "MIN",
    Max => "MAX",
    Avg => "AVG",
    Sample => "SAMPLE",
    GroupConcat => "GROUP_CONCAT",
    Separator => "SEPARATOR",
    // SPARQL 1.2 builtins.
    Triple => "TRIPLE",
    Subject => "SUBJECT",
    Predicate => "PREDICATE",
    Object => "OBJECT",
    IsTriple => "ISTRIPLE",
    LangDir => "LANGDIR",
    HasLang => "HASLANG",
    HasLangDir => "HASLANGDIR",
    StrLangDir => "STRLANGDIR",
}
