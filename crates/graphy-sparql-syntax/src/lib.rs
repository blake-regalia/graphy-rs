//! SPARQL 1.1 + 1.2 syntax (doc 04): a single-pass lexer and (coming) a
//! recursive-descent parser for Query + Update, producing a span-carrying
//! AST. Engine-independent — the algebra translation lives in
//! `graphy-algebra`.

pub mod ast;
pub mod lexer;
pub mod parser;
pub mod print;
pub mod subst;
pub mod token;

pub use lexer::{tokenize, tokenize_resilient, LexError};
pub use parser::{
    parse_query, parse_query_recovering, parse_update, parse_update_recovering, ParseError,
};
pub use print::{print_expr, print_path, print_query, print_term, print_update, Printer};
pub use subst::{substitute_query, substitute_update, SubstError};
pub use token::{Dir, Kw, Span, StringForm, Token, TokenKind};
