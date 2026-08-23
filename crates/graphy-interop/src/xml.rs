//! A minimal, self-contained XML 1.0 pull reader: elements, attributes, character data,
//! predefined/numeric entity references, comments, PIs, CDATA, and bounded internal
//! general entities. External and parameter entities are rejected, preventing XXE while
//! supporting the internal namespace abbreviations common in RDF/XML documents.

use std::collections::HashMap;

const MAX_ENTITY_DECLARATIONS: usize = 64;
const MAX_ENTITY_DEPTH: usize = 16;
const MAX_EXPANDED_TEXT: usize = 1 << 20;

#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    /// `<name a="v" …>` — `empty` marks `<name …/>`.
    Start {
        element: Element,
        empty: bool,
    },
    /// `</name>`
    End(String),
    /// Character data with entities decoded (CDATA included verbatim).
    Text(String),
    Eof,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Element {
    /// Raw qualified name as written (`rdf:Description`).
    pub qname: String,
    /// (raw attribute qname, entity-decoded value) in document order.
    pub attrs: Vec<(String, String)>,
}

pub struct XmlError(pub String);

impl std::fmt::Display for XmlError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "XML parse error: {}", self.0)
    }
}
impl std::fmt::Debug for XmlError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "XmlError({})", self.0)
    }
}
impl std::error::Error for XmlError {}

pub struct XmlReader<'a> {
    src: &'a str,
    pos: usize,
    /// Names of currently open elements (for well-formedness).
    open: Vec<String>,
    entities: HashMap<String, String>,
}

impl<'a> XmlReader<'a> {
    pub fn new(src: &'a str) -> Self {
        Self {
            src,
            pos: 0,
            open: Vec::new(),
            entities: HashMap::new(),
        }
    }

    fn rest(&self) -> &'a str {
        &self.src[self.pos..]
    }

    fn error(&self, message: impl Into<String>) -> XmlError {
        XmlError(format!("{} at byte {}", message.into(), self.pos))
    }

    /// The next structural event.
    pub fn next(&mut self) -> Result<Event, XmlError> {
        loop {
            if self.rest().is_empty() {
                if !self.open.is_empty() {
                    return Err(
                        self.error(format!("unclosed element <{}>", self.open.last().unwrap()))
                    );
                }
                return Ok(Event::Eof);
            }
            if self.rest().starts_with("<?") {
                self.skip_until("?>")?;
                continue;
            }
            if self.rest().starts_with("<!--") {
                self.pos += 4;
                let Some(end) = self.rest().find("-->") else {
                    return Err(self.error("unterminated comment"));
                };
                if self.rest()[..end].contains("--") {
                    return Err(self.error("'--' inside comment"));
                }
                self.pos += end + 3;
                continue;
            }
            if self.rest().starts_with("<!DOCTYPE") {
                self.skip_doctype()?;
                continue;
            }
            if self.rest().starts_with("<![CDATA[") {
                self.pos += 9;
                let Some(end) = self.rest().find("]]>") else {
                    return Err(self.error("unterminated CDATA section"));
                };
                let text = self.rest()[..end].to_string();
                self.pos += end + 3;
                return Ok(Event::Text(text));
            }
            if self.rest().starts_with("</") {
                self.pos += 2;
                let name = self.read_name()?;
                self.skip_ws();
                if !self.rest().starts_with('>') {
                    return Err(self.error("malformed end tag"));
                }
                self.pos += 1;
                match self.open.pop() {
                    Some(open) if open == name => {}
                    Some(open) => {
                        return Err(self.error(format!("mismatched end tag </{name}> for <{open}>")))
                    }
                    None => return Err(self.error(format!("stray end tag </{name}>"))),
                }
                return Ok(Event::End(name));
            }
            if self.rest().starts_with('<') {
                self.pos += 1;
                return self.read_start_tag();
            }
            // character data up to the next markup
            let end = self.rest().find('<').unwrap_or(self.rest().len());
            let raw = &self.rest()[..end];
            if raw.contains("]]>") {
                return Err(self.error("']]>' in character data"));
            }
            let decoded = decode_entities_with(raw, &self.entities).map_err(|e| self.error(e))?;
            self.pos += end;
            if self.open.is_empty() {
                if !decoded.trim().is_empty() {
                    return Err(self.error("character data outside the document element"));
                }
                continue;
            }
            return Ok(Event::Text(decoded));
        }
    }

    fn read_start_tag(&mut self) -> Result<Event, XmlError> {
        let qname = self.read_name()?;
        let mut attrs = Vec::new();
        loop {
            self.skip_ws();
            if self.rest().starts_with("/>") {
                self.pos += 2;
                return Ok(Event::Start {
                    element: Element { qname, attrs },
                    empty: true,
                });
            }
            if self.rest().starts_with('>') {
                self.pos += 1;
                self.open.push(qname.clone());
                return Ok(Event::Start {
                    element: Element { qname, attrs },
                    empty: false,
                });
            }
            let attr_name = self.read_name()?;
            self.skip_ws();
            if !self.rest().starts_with('=') {
                return Err(self.error(format!("attribute `{attr_name}` missing '='")));
            }
            self.pos += 1;
            self.skip_ws();
            let quote = match self.rest().chars().next() {
                Some(q @ ('"' | '\'')) => q,
                _ => return Err(self.error("attribute value must be quoted")),
            };
            self.pos += 1;
            let Some(end) = self.rest().find(quote) else {
                return Err(self.error("unterminated attribute value"));
            };
            let raw = &self.rest()[..end];
            if raw.contains('<') {
                return Err(self.error("'<' in attribute value"));
            }
            let value = decode_entities_with(raw, &self.entities).map_err(|e| self.error(e))?;
            self.pos += end + 1;
            if attrs.iter().any(|(existing, _)| *existing == attr_name) {
                return Err(self.error(format!("duplicate attribute `{attr_name}`")));
            }
            attrs.push((attr_name, value));
        }
    }

    fn read_name(&mut self) -> Result<String, XmlError> {
        let rest = self.rest();
        let mut end = 0;
        for (index, ch) in rest.char_indices() {
            let ok = if index == 0 {
                ch.is_alphabetic() || ch == '_' || ch == ':'
            } else {
                ch.is_alphanumeric() || matches!(ch, '_' | ':' | '-' | '.') || ch as u32 > 0xB7
            };
            if !ok {
                end = index;
                break;
            }
            end = index + ch.len_utf8();
        }
        if end == 0 {
            return Err(self.error("expected a name"));
        }
        let name = rest[..end].to_string();
        self.pos += end;
        Ok(name)
    }

    fn skip_ws(&mut self) {
        let trimmed = self
            .rest()
            .trim_start_matches(['\u{20}', '\u{9}', '\u{D}', '\u{A}']);
        self.pos = self.src.len() - trimmed.len();
    }

    fn skip_until(&mut self, needle: &str) -> Result<(), XmlError> {
        match self.rest().find(needle) {
            Some(index) => {
                self.pos += index + needle.len();
                Ok(())
            }
            None => Err(self.error(format!("unterminated construct (expected `{needle}`)"))),
        }
    }

    fn skip_doctype(&mut self) -> Result<(), XmlError> {
        // Scan to the matching '>', honoring quotes and an internal subset.
        // Only internal general entities are accepted; no resolver is ever
        // invoked for external identifiers.
        self.pos += "<!DOCTYPE".len();
        let mut in_subset = false;
        let mut quote = None;
        let mut subset_start = None;
        let mut subset_end = None;
        let decl_start = self.pos;
        loop {
            let Some(ch) = self.rest().chars().next() else {
                return Err(self.error("unterminated DOCTYPE"));
            };
            let at = self.pos;
            self.pos += ch.len_utf8();
            if let Some(q) = quote {
                if ch == q {
                    quote = None;
                }
                continue;
            }
            match ch {
                '\'' | '"' => quote = Some(ch),
                '[' if !in_subset => {
                    in_subset = true;
                    subset_start = Some(self.pos);
                }
                ']' if in_subset => {
                    in_subset = false;
                    subset_end = Some(at);
                }
                '>' if !in_subset => {
                    let header_end = subset_start.map_or(at, |start| start - 1);
                    let header = &self.src[decl_start..header_end];
                    if header
                        .split_whitespace()
                        .any(|w| matches!(w, "SYSTEM" | "PUBLIC"))
                    {
                        return Err(self.error("external DOCTYPE identifiers are not supported"));
                    }
                    if let (Some(start), Some(end)) = (subset_start, subset_end) {
                        self.parse_internal_subset(&self.src[start..end])?;
                    }
                    return Ok(());
                }
                _ => {}
            }
        }
    }

    fn parse_internal_subset(&mut self, subset: &str) -> Result<(), XmlError> {
        let mut rest = subset;
        while let Some(at) = rest.find("<!ENTITY") {
            rest = &rest[at + "<!ENTITY".len()..];
            rest = rest.trim_start_matches(char::is_whitespace);
            if rest.starts_with('%') {
                return Err(self.error("parameter entities are not supported"));
            }
            let name_end = rest
                .find(char::is_whitespace)
                .ok_or_else(|| self.error("malformed entity declaration"))?;
            let name = &rest[..name_end];
            if name.is_empty()
                || !name.chars().enumerate().all(|(i, c)| {
                    if i == 0 {
                        c.is_alphabetic() || matches!(c, '_' | ':')
                    } else {
                        c.is_alphanumeric() || matches!(c, '_' | ':' | '-' | '.')
                    }
                })
            {
                return Err(self.error("invalid entity name"));
            }
            rest = rest[name_end..].trim_start_matches(char::is_whitespace);
            if rest.starts_with("SYSTEM") || rest.starts_with("PUBLIC") {
                return Err(self.error("external entities are not supported"));
            }
            let Some(q @ ('\'' | '"')) = rest.chars().next() else {
                return Err(self.error("entity value must be quoted"));
            };
            rest = &rest[q.len_utf8()..];
            let Some(end) = rest.find(q) else {
                return Err(self.error("unterminated entity value"));
            };
            if self.entities.len() >= MAX_ENTITY_DECLARATIONS {
                return Err(self.error("too many entity declarations"));
            }
            self.entities
                .insert(name.to_owned(), rest[..end].to_owned());
            rest = rest[end + q.len_utf8()..].trim_start_matches(char::is_whitespace);
            let Some(after) = rest.strip_prefix('>') else {
                return Err(self.error("malformed entity declaration"));
            };
            rest = after;
        }
        Ok(())
    }
}

fn decode_entities_with(raw: &str, entities: &HashMap<String, String>) -> Result<String, String> {
    let mut out = String::with_capacity(raw.len());
    expand_entities(raw, entities, &mut Vec::new(), &mut out)?;
    Ok(out)
}

fn expand_entities(
    raw: &str,
    entities: &HashMap<String, String>,
    stack: &mut Vec<String>,
    out: &mut String,
) -> Result<(), String> {
    if !raw.contains('&') {
        out.push_str(raw);
        if out.len() > MAX_EXPANDED_TEXT {
            return Err("entity expansion exceeds size limit".to_owned());
        }
        return Ok(());
    }
    let mut rest = raw;
    while let Some(amp) = rest.find('&') {
        out.push_str(&rest[..amp]);
        rest = &rest[amp + 1..];
        let Some(semi) = rest.find(';') else {
            return Err("unterminated entity reference".to_string());
        };
        let entity = &rest[..semi];
        match entity {
            "amp" => out.push('&'),
            "lt" => out.push('<'),
            "gt" => out.push('>'),
            "quot" => out.push('"'),
            "apos" => out.push('\''),
            _ if entity.starts_with("#x") || entity.starts_with("#X") => {
                let code = u32::from_str_radix(&entity[2..], 16)
                    .map_err(|_| format!("bad character reference &{entity};"))?;
                out.push(
                    char::from_u32(code).ok_or_else(|| format!("invalid codepoint &{entity};"))?,
                );
            }
            _ if entity.starts_with('#') => {
                let code = entity[1..]
                    .parse::<u32>()
                    .map_err(|_| format!("bad character reference &{entity};"))?;
                out.push(
                    char::from_u32(code).ok_or_else(|| format!("invalid codepoint &{entity};"))?,
                );
            }
            _ => {
                let value = entities
                    .get(entity)
                    .ok_or_else(|| format!("undeclared entity &{entity};"))?;
                if stack.len() >= MAX_ENTITY_DEPTH || stack.iter().any(|e| e == entity) {
                    return Err(format!("recursive or too-deep entity &{entity};"));
                }
                stack.push(entity.to_owned());
                expand_entities(value, entities, stack, out)?;
                stack.pop();
            }
        }
        if out.len() > MAX_EXPANDED_TEXT {
            return Err("entity expansion exceeds size limit".to_owned());
        }
        rest = &rest[semi + 1..];
    }
    out.push_str(rest);
    if out.len() > MAX_EXPANDED_TEXT {
        return Err("entity expansion exceeds size limit".to_owned());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{Event, XmlReader};

    #[test]
    fn expands_nested_internal_entities() {
        let xml = r#"<!DOCTYPE x [
            <!ENTITY root "http://example.com">
            <!ENTITY ns "&root;/ns#">
        ]><x a="&ns;value"/>"#;
        let Event::Start { element, .. } = XmlReader::new(xml).next().unwrap() else {
            panic!("expected start element");
        };
        assert_eq!(element.attrs[0].1, "http://example.com/ns#value");
    }

    #[test]
    fn rejects_external_and_recursive_entities() {
        let external = r#"<!DOCTYPE x [<!ENTITY leak SYSTEM "file:///etc/passwd">]><x/>"#;
        assert!(XmlReader::new(external).next().is_err());

        let recursive = r#"<!DOCTYPE x [<!ENTITY loop "&loop;">]><x a="&loop;"/>"#;
        assert!(XmlReader::new(recursive).next().is_err());
    }
}
