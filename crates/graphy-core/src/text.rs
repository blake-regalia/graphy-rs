//! Unicode character classes shared by the RDF text grammars (`PN_CHARS`
//! et al. — Turtle, TriG, and SPARQL use the same terminal definitions).
//! ASCII is special-cased on the lexers' hot paths; these full-range
//! checks run only for multibyte characters.

pub fn is_pn_chars_base(c: char) -> bool {
    matches!(c,
        'A'..='Z'
        | 'a'..='z'
        | '\u{00C0}'..='\u{00D6}'
        | '\u{00D8}'..='\u{00F6}'
        | '\u{00F8}'..='\u{02FF}'
        | '\u{0370}'..='\u{037D}'
        | '\u{037F}'..='\u{1FFF}'
        | '\u{200C}'..='\u{200D}'
        | '\u{2070}'..='\u{218F}'
        | '\u{2C00}'..='\u{2FEF}'
        | '\u{3001}'..='\u{D7FF}'
        | '\u{F900}'..='\u{FDCF}'
        | '\u{FDF0}'..='\u{FFFD}'
        | '\u{10000}'..='\u{EFFFF}')
}

pub fn is_pn_chars_u(c: char) -> bool {
    c == '_' || is_pn_chars_base(c)
}

pub fn is_pn_chars(c: char) -> bool {
    matches!(c, '-' | '0'..='9' | '\u{00B7}' | '\u{0300}'..='\u{036F}' | '\u{203F}'..='\u{2040}')
        || is_pn_chars_u(c)
}

/// `VARNAME` continuation characters (SPARQL): `PN_CHARS` minus `-`.
pub fn is_varname_char(c: char) -> bool {
    matches!(c, '0'..='9' | '\u{00B7}' | '\u{0300}'..='\u{036F}' | '\u{203F}'..='\u{2040}')
        || is_pn_chars_u(c)
}

/// Characters legal in a PN_LOCAL escape (`PN_LOCAL_ESC`).
pub fn is_pn_local_esc(b: u8) -> bool {
    matches!(
        b,
        b'_' | b'~'
            | b'.'
            | b'-'
            | b'!'
            | b'$'
            | b'&'
            | b'\''
            | b'('
            | b')'
            | b'*'
            | b'+'
            | b','
            | b';'
            | b'='
            | b'/'
            | b'?'
            | b'#'
            | b'@'
            | b'%'
    )
}

/// Bytes forbidden inside an IRIREF: controls/space and `<>"{}|^\``\`.
/// Raw scanning never sees `>` (it terminates the token), but escape
/// decoding can produce one, so it must be in the set.
pub fn is_forbidden_iri_byte(b: u8) -> bool {
    b <= 0x20
        || matches!(
            b,
            b'<' | b'>' | b'"' | b'{' | b'}' | b'|' | b'^' | b'`' | b'\\'
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pn_classes() {
        assert!(is_pn_chars_base('A') && is_pn_chars_base('é') && is_pn_chars_base('中'));
        assert!(!is_pn_chars_base('_') && !is_pn_chars_base('0') && !is_pn_chars_base('-'));
        assert!(is_pn_chars_u('_') && !is_pn_chars_u('-'));
        assert!(is_pn_chars('-') && is_pn_chars('5') && is_pn_chars('\u{00B7}'));
        assert!(!is_pn_chars('.') && !is_pn_chars(':'));
        assert!(is_varname_char('_') && is_varname_char('5') && !is_varname_char('-'));
    }
}
