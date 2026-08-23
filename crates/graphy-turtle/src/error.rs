//! Parse errors carrying position and a recovery-friendly message (doc 03 §2).

/// A syntax or well-formedness error at a specific input position.
///
/// `offset` is the absolute byte offset from the start of the stream; `line`
/// and `column` are 1-based (column counts bytes, not characters).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{message} at {line}:{column} (byte {offset})")]
pub struct ParseError {
    pub message: String,
    pub offset: u64,
    pub line: u64,
    pub column: u64,
}

/// Parse or I/O failure from the reader adapters.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Parse(#[from] ParseError),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}
