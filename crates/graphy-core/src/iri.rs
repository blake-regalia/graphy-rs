//! IRI validation and RFC 3986/3987 reference resolution.
//!
//! Validation is pragmatic (doc 01 §3 "validation boundary"): we enforce the
//! structural grammar of RFC 3986 generalized to IRIs (RFC 3987 `ucschar` /
//! `iprivate` ranges admitted wherever `unreserved` is), require a scheme for
//! absolute IRIs, and reject the characters that can never appear raw in an
//! IRI reference (controls, space, `<>"{}|\^` and backtick). We do **not**
//! chase scheme-specific rules. Terms are stored as-given (codepoint
//! comparison); optional NFC normalization is a store-level concern upstack.

use crate::{Result, TermError};

/// Validate an absolute IRI (scheme required).
pub fn validate_iri(iri: &str) -> Result<()> {
    let parts = parse_reference(iri)?;
    if parts.scheme.is_none() {
        return Err(TermError::InvalidIri {
            iri: iri.to_owned(),
            pos: 0,
            reason: "relative IRI where an absolute IRI is required",
        });
    }
    Ok(())
}

/// Validate an IRI reference (may be relative).
pub fn validate_reference(iri_ref: &str) -> Result<()> {
    parse_reference(iri_ref).map(|_| ())
}

/// Resolve `reference` against absolute `base` per RFC 3986 §5.3.
pub fn resolve(base: &str, reference: &str) -> Result<String> {
    let b = parse_reference(base)?;
    if b.scheme.is_none() {
        return Err(TermError::RelativeBase {
            base: base.to_owned(),
        });
    }
    let r = parse_reference(reference)?;

    // RFC 3986 §5.3 transform-references algorithm.
    let (scheme, authority, path, query);
    if r.scheme.is_some() {
        scheme = r.scheme;
        authority = r.authority;
        path = remove_dot_segments(r.path);
        query = r.query;
    } else {
        scheme = b.scheme;
        if r.authority.is_some() {
            authority = r.authority;
            path = remove_dot_segments(r.path);
            query = r.query;
        } else {
            authority = b.authority;
            if r.path.is_empty() {
                path = b.path.to_owned();
                query = r.query.or(b.query);
            } else {
                query = r.query;
                if r.path.starts_with('/') {
                    path = remove_dot_segments(r.path);
                } else {
                    path = remove_dot_segments(&merge_paths(&b, r.path));
                }
            }
        }
    }

    let mut out = String::with_capacity(base.len() + reference.len());
    if let Some(s) = scheme {
        out.push_str(s);
        out.push(':');
    }
    if let Some(a) = authority {
        out.push_str("//");
        out.push_str(a);
    }
    out.push_str(&path);
    if let Some(q) = query {
        out.push('?');
        out.push_str(q);
    }
    if let Some(f) = r.fragment {
        out.push('#');
        out.push_str(f);
    }
    Ok(out)
}

/// Component view of an IRI reference. Slices borrow the input.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IriParts<'a> {
    pub scheme: Option<&'a str>,
    pub authority: Option<&'a str>,
    pub path: &'a str,
    pub query: Option<&'a str>,
    pub fragment: Option<&'a str>,
}

/// Parse an IRI reference into components, validating character content.
pub fn parse_reference(s: &str) -> Result<IriParts<'_>> {
    // Reject characters never allowed raw anywhere in an IRI reference.
    if let Some(pos) = s
        .char_indices()
        .find(|&(_, c)| {
            c <= ' '
                || c == '<'
                || c == '>'
                || c == '"'
                || c == '{'
                || c == '}'
                || c == '|'
                || c == '\\'
                || c == '^'
                || c == '`'
                || c == '\u{7f}'
        })
        .map(|(i, _)| i)
    {
        return Err(err(s, pos, "forbidden character"));
    }
    // Percent-encodings must be complete.
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            if i + 2 >= bytes.len() {
                return Err(err(s, i, "truncated percent-encoding"));
            }
            if !bytes[i + 1].is_ascii_hexdigit() || !bytes[i + 2].is_ascii_hexdigit() {
                return Err(err(s, i, "invalid percent-encoding"));
            }
            i += 3;
        } else {
            i += 1;
        }
    }

    let mut rest = s;
    let mut parts = IriParts {
        scheme: None,
        authority: None,
        path: "",
        query: None,
        fragment: None,
    };

    // Fragment first. A second '#' is not data: it violates the IRI
    // reference grammar.
    if let Some(h) = rest.find('#') {
        if rest[h + 1..].contains('#') {
            return Err(err(s, h + 1, "multiple fragment delimiters"));
        }
        parts.fragment = Some(&rest[h + 1..]);
        rest = &rest[..h];
    }
    // Query.
    if let Some(q) = rest.find('?') {
        parts.query = Some(&rest[q + 1..]);
        rest = &rest[..q];
    }
    // Scheme: ALPHA (ALPHA / DIGIT / '+' / '-' / '.')* ':', and it must occur
    // before any '/' for the colon to denote a scheme.
    if let Some(c) = rest.find(':') {
        let candidate = &rest[..c];
        let is_scheme = !candidate.is_empty()
            && candidate.as_bytes()[0].is_ascii_alphabetic()
            && candidate
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'+' || b == b'-' || b == b'.')
            && !candidate.contains('/');
        if is_scheme {
            parts.scheme = Some(candidate);
            rest = &rest[c + 1..];
        }
    }
    // Authority.
    if let Some(after) = rest.strip_prefix("//") {
        let end = after.find(['/', '?', '#']).unwrap_or(after.len());
        parts.authority = Some(&after[..end]);
        rest = &after[end..];
    }
    parts.path = rest;

    if let Some(authority) = parts.authority {
        validate_authority(s, authority)?;
        if !parts.path.is_empty() && !parts.path.starts_with('/') {
            return Err(err(s, 0, "authority path must be empty or begin with '/'"));
        }
    } else if parts.scheme.is_none() {
        let first = parts.path.split('/').next().unwrap_or("");
        if first.contains(':') {
            return Err(err(
                s,
                first.find(':').unwrap_or(0),
                "relative first path segment contains ':'",
            ));
        }
    }
    validate_component(s, parts.path, Component::Path)?;
    if let Some(query) = parts.query {
        validate_component(s, query, Component::QueryOrFragment)?;
    }
    if let Some(fragment) = parts.fragment {
        validate_component(s, fragment, Component::QueryOrFragment)?;
    }
    Ok(parts)
}

#[derive(Clone, Copy)]
enum Component {
    Path,
    QueryOrFragment,
}

fn validate_component(full: &str, value: &str, component: Component) -> Result<()> {
    let bytes = value.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            // Completeness was checked once above.
            i += 3;
            continue;
        }
        let c = value[i..].chars().next().expect("character boundary");
        let allowed = is_ipchar(c)
            || c == '/'
            || matches!(component, Component::QueryOrFragment) && (c == '?' || is_iprivate(c));
        if !allowed {
            return Err(err(
                full,
                full.len().saturating_sub(value.len()) + i,
                "character not allowed in IRI component",
            ));
        }
        i += c.len_utf8();
    }
    Ok(())
}

fn validate_authority(full: &str, authority: &str) -> Result<()> {
    let host_port = match authority.rsplit_once('@') {
        Some((userinfo, host_port)) => {
            validate_ascii_component(full, userinfo, |c| {
                is_iunreserved(c) || is_sub_delim(c) || c == ':'
            })?;
            host_port
        }
        None => authority,
    };

    if let Some(bracketed) = host_port.strip_prefix('[') {
        let Some(close) = bracketed.find(']') else {
            return Err(err(full, 0, "unterminated IP literal"));
        };
        let address = &bracketed[..close];
        let tail = &bracketed[close + 1..];
        if tail.contains(['[', ']']) {
            return Err(err(full, 0, "invalid bracket in authority"));
        }
        if let Some(port) = tail.strip_prefix(':') {
            if !port.bytes().all(|b| b.is_ascii_digit()) {
                return Err(err(full, 0, "invalid port"));
            }
        } else if !tail.is_empty() {
            return Err(err(full, 0, "characters after IP literal"));
        }
        let ipv_future = address
            .strip_prefix('v')
            .or_else(|| address.strip_prefix('V'))
            .and_then(|r| r.split_once('.'))
            .is_some_and(|(version, body)| {
                !version.is_empty()
                    && version.bytes().all(|b| b.is_ascii_hexdigit())
                    && !body.is_empty()
                    && body
                        .chars()
                        .all(|c| is_unreserved_ascii(c) || is_sub_delim(c) || c == ':')
            });
        if address.parse::<std::net::Ipv6Addr>().is_err() && !ipv_future {
            return Err(err(full, 0, "invalid IP literal"));
        }
        return Ok(());
    }
    if host_port.contains(['[', ']']) {
        return Err(err(full, 0, "invalid bracket in authority"));
    }

    let (host, port) = match host_port.rsplit_once(':') {
        Some((host, port)) if !host.contains(':') => (host, Some(port)),
        Some(_) => return Err(err(full, 0, "IPv6 address must be bracketed")),
        None => (host_port, None),
    };
    if let Some(port) = port {
        if !port.bytes().all(|b| b.is_ascii_digit()) {
            return Err(err(full, 0, "invalid port"));
        }
    }
    validate_ascii_component(full, host, |c| is_iunreserved(c) || is_sub_delim(c))
}

fn validate_ascii_component(full: &str, value: &str, allowed: impl Fn(char) -> bool) -> Result<()> {
    let bytes = value.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            i += 3;
            continue;
        }
        let c = value[i..].chars().next().expect("character boundary");
        if !allowed(c) {
            return Err(err(full, 0, "character not allowed in authority"));
        }
        i += c.len_utf8();
    }
    Ok(())
}

fn is_ipchar(c: char) -> bool {
    is_iunreserved(c) || is_sub_delim(c) || matches!(c, ':' | '@')
}

fn is_iunreserved(c: char) -> bool {
    is_unreserved_ascii(c) || is_ucschar(c)
}

fn is_unreserved_ascii(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '-' | '.' | '_' | '~')
}

fn is_sub_delim(c: char) -> bool {
    matches!(
        c,
        '!' | '$' | '&' | '\'' | '(' | ')' | '*' | '+' | ',' | ';' | '='
    )
}

fn is_ucschar(c: char) -> bool {
    matches!(
        c as u32,
        0xA0..=0xD7FF
            | 0xF900..=0xFDCF
            | 0xFDF0..=0xFFEF
            | 0x10000..=0x1FFFD
            | 0x20000..=0x2FFFD
            | 0x30000..=0x3FFFD
            | 0x40000..=0x4FFFD
            | 0x50000..=0x5FFFD
            | 0x60000..=0x6FFFD
            | 0x70000..=0x7FFFD
            | 0x80000..=0x8FFFD
            | 0x90000..=0x9FFFD
            | 0xA0000..=0xAFFFD
            | 0xB0000..=0xBFFFD
            | 0xC0000..=0xCFFFD
            | 0xD0000..=0xDFFFD
            | 0xE1000..=0xEFFFD
    )
}

fn is_iprivate(c: char) -> bool {
    matches!(c as u32, 0xE000..=0xF8FF | 0xF0000..=0xFFFFD | 0x100000..=0x10FFFD)
}

fn err(iri: &str, pos: usize, reason: &'static str) -> TermError {
    TermError::InvalidIri {
        iri: iri.to_owned(),
        pos,
        reason,
    }
}

/// RFC 3986 §5.3.3.
fn merge_paths(base: &IriParts, ref_path: &str) -> String {
    if base.authority.is_some() && base.path.is_empty() {
        let mut s = String::with_capacity(ref_path.len() + 1);
        s.push('/');
        s.push_str(ref_path);
        s
    } else {
        match base.path.rfind('/') {
            Some(i) => {
                let mut s = String::with_capacity(i + 1 + ref_path.len());
                s.push_str(&base.path[..=i]);
                s.push_str(ref_path);
                s
            }
            None => ref_path.to_owned(),
        }
    }
}

/// RFC 3986 §5.2.4. The "replace prefix with '/'" steps are implemented by
/// reslicing so the retained '/' is the original one (no allocation).
fn remove_dot_segments(path: &str) -> String {
    let mut input = path;
    let mut output = String::with_capacity(path.len());
    while !input.is_empty() {
        if let Some(rest) = input.strip_prefix("../") {
            input = rest;
        } else if let Some(rest) = input.strip_prefix("./") {
            input = rest;
        } else if input.starts_with("/./") {
            input = &input[2..]; // "/./x" → "/x"
        } else if input == "/." {
            input = "/";
        } else if input.starts_with("/../") {
            input = &input[3..]; // "/../x" → "/x"
            pop_segment(&mut output);
        } else if input == "/.." {
            input = "/";
            pop_segment(&mut output);
        } else if input == "." || input == ".." {
            input = "";
        } else {
            // Move the first segment (including a leading '/', up to but not
            // including the next '/') from input to output.
            let start = usize::from(input.starts_with('/'));
            let end = input[start..]
                .find('/')
                .map(|i| i + start)
                .unwrap_or(input.len());
            output.push_str(&input[..end]);
            input = &input[end..];
        }
    }
    output
}

fn pop_segment(output: &mut String) {
    if let Some(i) = output.rfind('/') {
        output.truncate(i);
    } else {
        output.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_rfc3986_normal_examples() {
        let base = "http://a/b/c/d;p?q";
        let cases = [
            ("g:h", "g:h"),
            ("g", "http://a/b/c/g"),
            ("./g", "http://a/b/c/g"),
            ("g/", "http://a/b/c/g/"),
            ("/g", "http://a/g"),
            ("//g", "http://g"),
            ("?y", "http://a/b/c/d;p?y"),
            ("g?y", "http://a/b/c/g?y"),
            ("#s", "http://a/b/c/d;p?q#s"),
            ("g#s", "http://a/b/c/g#s"),
            ("g?y#s", "http://a/b/c/g?y#s"),
            (";x", "http://a/b/c/;x"),
            ("g;x", "http://a/b/c/g;x"),
            ("g;x?y#s", "http://a/b/c/g;x?y#s"),
            ("", "http://a/b/c/d;p?q"),
            (".", "http://a/b/c/"),
            ("./", "http://a/b/c/"),
            ("..", "http://a/b/"),
            ("../", "http://a/b/"),
            ("../g", "http://a/b/g"),
            ("../..", "http://a/"),
            ("../../", "http://a/"),
            ("../../g", "http://a/g"),
        ];
        for (r, expected) in cases {
            assert_eq!(resolve(base, r).unwrap(), expected, "ref {r:?}");
        }
    }

    #[test]
    fn resolve_rfc3986_abnormal_examples() {
        let base = "http://a/b/c/d;p?q";
        let cases = [
            ("../../../g", "http://a/g"),
            ("../../../../g", "http://a/g"),
            ("/./g", "http://a/g"),
            ("/../g", "http://a/g"),
            ("g.", "http://a/b/c/g."),
            (".g", "http://a/b/c/.g"),
            ("g..", "http://a/b/c/g.."),
            ("..g", "http://a/b/c/..g"),
            ("./../g", "http://a/b/g"),
            ("./g/.", "http://a/b/c/g/"),
            ("g/./h", "http://a/b/c/g/h"),
            ("g/../h", "http://a/b/c/h"),
            ("g;x=1/./y", "http://a/b/c/g;x=1/y"),
            ("g;x=1/../y", "http://a/b/c/y"),
        ];
        for (r, expected) in cases {
            assert_eq!(resolve(base, r).unwrap(), expected, "ref {r:?}");
        }
    }

    #[test]
    fn validates() {
        assert!(validate_iri("http://example.org/x").is_ok());
        assert!(validate_iri("urn:uuid:1234").is_ok());
        assert!(validate_iri("relative/path").is_err());
        assert!(validate_iri("http://example.org/sp ace").is_err());
        assert!(validate_iri("http://example.org/<x>").is_err());
        assert!(validate_iri("http://example.org/%2").is_err());
        assert!(validate_iri("http://example.org/%GG").is_err());
        assert!(validate_iri("http://example.org/ok%20fine").is_ok());
        assert!(validate_iri("http://example.org/emoji/\u{1F600}").is_ok());
        assert!(validate_iri("http://[2001:db8::1]/x").is_ok());
        assert!(validate_iri("http://[bad").is_err());
        assert!(validate_iri("http://2001:db8::1/x").is_err());
        assert!(validate_iri("http://example.org:abc/x").is_err());
        assert!(validate_iri("http://example.org/x#one#two").is_err());
        assert!(validate_reference("a:b").is_ok(), "a:b is an absolute IRI");
        assert!(validate_reference("1a:b").is_err());
    }
}
