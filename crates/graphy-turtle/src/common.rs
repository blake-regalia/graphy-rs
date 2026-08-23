//! Term assembly shared by the format drivers: IRI resolution, blank-node
//! relabeling, prefix expansion, and concise encoding into the quad arena.
//! Functions return `Err(String)` messages; drivers attach positions.

use std::collections::HashMap;

use graphy_core::{concise, iri, vocab, Dir};

use crate::quad::{Arena, H};

/// Statement-independent parser context.
#[derive(Debug, Default)]
pub(crate) struct TermCtx {
    /// Current base IRI (absolute), if any.
    base: Option<String>,
    /// Prefix map (Turtle/TriG only).
    prefixes: HashMap<Vec<u8>, String>,
    /// Surface blank-node label → internal label ordinal. Internal labels
    /// are `s{n}` in first-seen order (chunking-independent), drawing from
    /// the same counter as minted `b{n}` labels but in a disjoint namespace:
    /// only syntax-minted nodes may carry `b{n}`, which is what lets the
    /// pretty writer inline them (a surface label can be referenced any
    /// number of times; a minted one at most once by grammar).
    labels: HashMap<Vec<u8>, u64>,
    next_label: u64,
    /// Trusted-input mode (`Options::trusted`): scheme-shaped references are
    /// taken as valid absolute IRIs without the percent-encoding/DEL sweep
    /// or the full RFC 3986 re-validation.
    trusted: bool,
    /// Content-derived blank labels (data-parallel NT/NQ, doc 03 §4.1):
    /// internal label = `s` + surface label instead of first-seen `s{n}`,
    /// making output independent of discovery order across parser instances.
    /// Only valid for drivers that never mint fresh labels (NT/NQ).
    pub(crate) content_labels: bool,
    /// Blank-label namespace (`Options::label_ns`): prefixes every emitted
    /// label with `f{ns}` so labels from different documents never unify.
    label_ns: Option<u128>,
}

impl TermCtx {
    pub fn new(
        base: Option<&str>,
        trusted: bool,
        label_ns: Option<u128>,
    ) -> Result<TermCtx, String> {
        let mut ctx = TermCtx {
            trusted,
            label_ns,
            ..TermCtx::default()
        };
        if let Some(b) = base {
            ctx.set_base(b)?;
        }
        Ok(ctx)
    }

    /// Replace the base; a relative new base resolves against the old one.
    pub fn set_base(&mut self, new: &str) -> Result<(), String> {
        let resolved = match &self.base {
            Some(cur) => iri::resolve(cur, new).map_err(|e| e.to_string())?,
            None => {
                iri::validate_iri(new).map_err(|e| e.to_string())?;
                new.to_owned()
            }
        };
        self.base = Some(resolved);
        Ok(())
    }

    pub fn set_prefix(&mut self, prefix: &[u8], iri: String) {
        self.prefixes.insert(prefix.to_vec(), iri);
    }

    pub fn expand_prefix(&self, prefix: &[u8]) -> Option<&str> {
        self.prefixes.get(prefix).map(String::as_str)
    }

    /// Iterate the declared prefixes (unordered; latest declaration wins).
    pub fn prefixes(&self) -> impl Iterator<Item = (&[u8], &str)> {
        self.prefixes
            .iter()
            .map(|(k, v)| (k.as_slice(), v.as_str()))
    }

    /// Map a surface label to its internal label ordinal.
    pub fn internal_label(&mut self, surface: &[u8]) -> u64 {
        if let Some(&n) = self.labels.get(surface) {
            return n;
        }
        let n = self.next_label;
        self.next_label += 1;
        self.labels.insert(surface.to_vec(), n);
        n
    }

    /// Fresh internal label (anonymous nodes, collections, reifiers).
    pub fn fresh_label(&mut self) -> u64 {
        let n = self.next_label;
        self.next_label += 1;
        n
    }

    /// Absolutize an IRI reference: absolute input passes through borrowed;
    /// relative input resolves against the base (RFC 3986 §5).
    ///
    /// Precondition: `iri_text` came out of the lexer, which already rejected
    /// the raw-forbidden characters — the fast path only re-checks what the
    /// lexer does not (percent-encoding completeness, DEL, scheme shape).
    pub fn resolve_iri<'a>(&self, iri_text: &'a str) -> Result<std::borrow::Cow<'a, str>, String> {
        if self.trusted {
            // Trusted input: a scheme-shaped reference IS an absolute IRI —
            // skip the %-completeness/DEL sweep and any RFC re-validation.
            // Anything else is a relative reference; resolve it if a base
            // exists, else pass it through (valid trusted input never gets
            // here without a base — garbage in, garbage out, safely).
            if scheme_shaped(iri_text) {
                return Ok(std::borrow::Cow::Borrowed(iri_text));
            }
            return match &self.base {
                Some(base) => iri::resolve(base, iri_text)
                    .map(std::borrow::Cow::Owned)
                    .map_err(|e| e.to_string()),
                None => Ok(std::borrow::Cow::Borrowed(iri_text)),
            };
        }
        if fast_absolute_lexed(iri_text) {
            return Ok(std::borrow::Cow::Borrowed(iri_text));
        }
        match iri::validate_iri(iri_text) {
            Ok(()) => Ok(std::borrow::Cow::Borrowed(iri_text)),
            Err(_) => match &self.base {
                Some(base) => iri::resolve(base, iri_text)
                    .map(std::borrow::Cow::Owned)
                    .map_err(|e| e.to_string()),
                None => {
                    // Re-derive the precise error (shape vs. relative).
                    iri::validate_reference(iri_text).map_err(|e| e.to_string())?;
                    Err(format!("relative IRI {iri_text:?} without a base"))
                }
            },
        }
    }

    /// Emit an IRI term, resolving relative references against the base.
    pub fn emit_iri(&self, buf: &mut Arena, iri_text: &str) -> Result<H, String> {
        let abs = self.resolve_iri(iri_text)?;
        let mark = buf.mark();
        concise::encode_iri(&mut buf.bytes, &abs);
        Ok(buf.handle_from(mark))
    }

    pub fn emit_blank(&mut self, buf: &mut Arena, surface: &[u8]) -> H {
        if self.content_labels {
            // `s{surface}` cannot collide with minted `b{n}` labels within a
            // run, and needs no shared state between parser instances.
            let mark = buf.mark();
            buf.bytes.push(b'_');
            push_ns(buf, self.label_ns);
            buf.bytes.push(b's');
            buf.bytes.extend_from_slice(surface);
            return buf.handle_from(mark);
        }
        let n = self.internal_label(surface);
        emit_blank_ordinal(buf, self.label_ns, b's', n)
    }

    /// Mint and emit a fresh internal blank node (anonymous nodes,
    /// collections, reifiers). Only these carry the `b{n}` shape the pretty
    /// writer may reconstruct as `( … )`/`[ … ]` syntax.
    pub fn emit_fresh_blank(&mut self, buf: &mut Arena) -> H {
        let n = self.fresh_label();
        emit_blank_ordinal(buf, self.label_ns, b'b', n)
    }
}

/// Emit `_:{f{ns}}{kind}{n}` for an internal ordinal: `kind` is `b` for
/// syntax-minted fresh nodes, `s` for surface-derived labels.
pub(crate) fn emit_blank_ordinal(buf: &mut Arena, ns: Option<u128>, kind: u8, n: u64) -> H {
    use std::io::Write as _;
    let mark = buf.mark();
    buf.bytes.push(b'_');
    push_ns(buf, ns);
    buf.bytes.push(kind);
    write!(buf.bytes, "{n}").expect("Vec write is infallible");
    buf.handle_from(mark)
}

/// Label-namespace prefix (`f{ns}`); prefixed languages are disjoint across
/// namespaces because what follows is always `b` or `s`, never a digit.
fn push_ns(buf: &mut Arena, ns: Option<u128>) {
    use std::io::Write as _;
    if let Some(ns) = ns {
        buf.bytes.push(b'f');
        write!(buf.bytes, "{ns}").expect("Vec write is infallible");
    }
}

pub(crate) fn emit_simple(buf: &mut Arena, lexical: &[u8]) -> H {
    let mark = buf.mark();
    buf.bytes.push(b'"');
    buf.bytes.extend_from_slice(lexical);
    buf.handle_from(mark)
}

/// `tag` as written (any case); lowercased here per the concise invariant.
pub(crate) fn emit_lang(buf: &mut Arena, lexical: &[u8], tag: &str, dir: Option<Dir>) -> H {
    let mark = buf.mark();
    buf.bytes.push(b'@');
    buf.bytes
        .extend(tag.bytes().map(|b| b.to_ascii_lowercase()));
    if let Some(d) = dir {
        buf.bytes.extend_from_slice(match d {
            Dir::Ltr => b"--ltr",
            Dir::Rtl => b"--rtl",
        });
    }
    buf.bytes.push(b'"');
    buf.bytes.extend_from_slice(lexical);
    buf.handle_from(mark)
}

/// Datatyped literal; folds `xsd:string` into the simple form and rejects
/// the langString datatypes (they require a language tag).
pub(crate) fn emit_typed(buf: &mut Arena, lexical: &[u8], dt: &str) -> Result<H, String> {
    if dt == vocab::XSD_STRING {
        return Ok(emit_simple(buf, lexical));
    }
    if dt == vocab::RDF_LANG_STRING || dt == vocab::RDF_DIR_LANG_STRING {
        return Err(format!(
            "literal with datatype {dt} requires a language tag"
        ));
    }
    let mark = buf.mark();
    buf.bytes.push(b'^');
    buf.bytes.push(b'>');
    buf.bytes.extend_from_slice(dt.as_bytes());
    buf.bytes.push(b'"');
    buf.bytes.extend_from_slice(lexical);
    Ok(buf.handle_from(mark))
}

/// Combine three already-emitted component handles into a triple term.
/// `scratch` avoids aliasing the arena during the copy.
pub(crate) fn emit_triple_term(buf: &mut Arena, s: H, p: H, o: H, scratch: &mut Vec<u8>) -> H {
    scratch.clear();
    {
        let (sb, pb, ob) = (buf.get(s), buf.get(p), buf.get(o));
        concise::encode_triple_term(scratch, sb, pb, ob);
    }
    let mark = buf.mark();
    buf.bytes.extend_from_slice(scratch);
    buf.handle_from(mark)
}

/// The concise datatyped-literal spelling for a Turtle numeric/boolean
/// shorthand token (grammar-validated lexical, implied datatype).
pub(crate) fn emit_shorthand(buf: &mut Arena, lexical: &[u8], dt: &'static str) -> H {
    let mark = buf.mark();
    buf.bytes.push(b'^');
    buf.bytes.push(b'>');
    buf.bytes.extend_from_slice(dt.as_bytes());
    buf.bytes.push(b'"');
    buf.bytes.extend_from_slice(lexical);
    buf.handle_from(mark)
}

/// Fast validity check for lexer-validated, common ASCII absolute IRIs.
/// `false` means "take the complete RFC 3987 path", never "invalid".
pub(crate) fn fast_absolute_lexed(s: &str) -> bool {
    let b = s.as_bytes();
    if !scheme_shaped(s) {
        return false;
    }
    if !s.is_ascii()
        || memchr::memchr2(b'%', 0x7F, b).is_some()
        || memchr::memchr2(b'[', b']', b).is_some()
    {
        return false;
    }
    if let Some(hash) = memchr::memchr(b'#', b) {
        if memchr::memchr(b'#', &b[hash + 1..]).is_some() {
            return false;
        }
    }
    let colon = memchr::memchr(b':', b).expect("scheme_shaped found colon");
    let rest = &b[colon + 1..];
    if let Some(after) = rest.strip_prefix(b"//") {
        let end = memchr::memchr3(b'/', b'?', b'#', after).unwrap_or(after.len());
        let authority = &after[..end];
        // Bracketed IP literals and userinfo are uncommon enough to use the
        // complete validator. For the common reg-name form, validate an
        // optional decimal port without allocating.
        if authority.contains(&b'@') {
            return false;
        }
        if let Some(i) = authority.iter().rposition(|&c| c == b':') {
            if !authority[i + 1..].iter().all(u8::is_ascii_digit) {
                return false;
            }
        }
    }
    true
}

/// Whether the reference begins with a well-formed scheme — the RFC 3986
/// absolute-vs-relative discriminator (a relative reference's first segment
/// cannot contain ':').
pub(crate) fn scheme_shaped(s: &str) -> bool {
    let b = s.as_bytes();
    let Some(colon) = memchr::memchr(b':', b) else {
        return false;
    };
    colon > 0
        && b[0].is_ascii_alphabetic()
        && b[1..colon]
            .iter()
            .all(|&c| c.is_ascii_alphanumeric() || matches!(c, b'+' | b'-' | b'.'))
}
