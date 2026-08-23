//! Unicode character classes from the Turtle/SPARQL grammars — shared
//! definitions live in `graphy_core::text` (the SPARQL lexer uses the
//! same terminals).

pub(crate) use graphy_core::text::{
    is_forbidden_iri_byte, is_pn_chars, is_pn_chars_base, is_pn_chars_u, is_pn_local_esc,
};
