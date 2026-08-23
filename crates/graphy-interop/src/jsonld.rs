//! JSON-LD parser and serializer. The parser covers the widely-used core of JSON-LD 1.0:
//! inline `@context` (term → IRI, term definitions with `@id`/`@type`, prefixes,
//! `@vocab`, `@base`, `@language`), node objects with `@id`/`@type`, value objects
//! (`@value`/`@language`/`@type`), `@list`, `@graph`, arrays, nested node objects, and
//! JSON native literals. Remote contexts, `@reverse`, framing, and `@container` reshaping
//! are rejected. The serializer emits an expanded-form document.

use std::collections::BTreeMap;

use graphy_core::{concise, iri};
use serde_json::{Map, Value};

use crate::{document_label_ns, ParseError, ParseOptions, Triple};

const RDF_NS: &str = "http://www.w3.org/1999/02/22-rdf-syntax-ns#";
const XSD_NS: &str = "http://www.w3.org/2001/XMLSchema#";

fn error(message: impl Into<String>) -> ParseError {
    ParseError(message.into())
}

#[derive(Clone, Default)]
struct Context {
    /// term → definition.
    terms: BTreeMap<String, TermDefinition>,
    vocab: Option<String>,
    base: Option<String>,
    language: Option<String>,
}

#[derive(Clone)]
struct TermDefinition {
    iri: String,
    /// `@type` coercion: an IRI, or the keyword `@id`.
    type_coercion: Option<String>,
    /// `@container: @list`.
    list_container: bool,
    language: Option<String>,
}

pub struct JsonLdParser {
    triples: Vec<Triple>,
    blank_counter: usize,
    label_ns: u128,
}

/// Parses a JSON-LD document into concise triples.
pub fn parse_jsonld(source: &str, base: Option<&str>) -> Result<Vec<Triple>, ParseError> {
    parse_jsonld_with_options(source, base, ParseOptions::default())
}

/// Parses a JSON-LD document with explicit parser options.
pub fn parse_jsonld_with_options(
    source: &str,
    base: Option<&str>,
    options: ParseOptions,
) -> Result<Vec<Triple>, ParseError> {
    let document: Value =
        serde_json::from_str(source).map_err(|e| error(format!("invalid JSON: {e}")))?;
    let mut parser = JsonLdParser {
        triples: Vec::new(),
        blank_counter: 0,
        label_ns: document_label_ns(options),
    };
    let context = Context {
        base: base.map(str::to_string),
        ..Context::default()
    };
    match &document {
        Value::Array(items) => {
            for item in items {
                parser.parse_node(item, &context)?;
            }
        }
        Value::Object(_) => {
            parser.parse_node(&document, &context)?;
        }
        _ => return Err(error("document must be an object or array")),
    }
    Ok(parser.triples)
}

impl JsonLdParser {
    fn fresh_blank(&mut self) -> Vec<u8> {
        let label = format!("g{:032x}b{}", self.label_ns, self.blank_counter);
        self.blank_counter += 1;
        let mut out = Vec::new();
        concise::encode_blank(&mut out, &label);
        out
    }

    // ------------------------------------------------------------ context

    fn merge_context(&self, base: &Context, value: &Value) -> Result<Context, ParseError> {
        let mut context = base.clone();
        match value {
            Value::Array(parts) => {
                for part in parts {
                    context = self.merge_context(&context, part)?;
                }
            }
            Value::Null => context = Context::default(),
            Value::String(_) => return Err(error("remote @context documents are not supported")),
            Value::Object(map) => {
                for (key, definition) in map {
                    match key.as_str() {
                        "@vocab" => {
                            context.vocab = definition.as_str().map(str::to_string);
                        }
                        "@base" => {
                            context.base = definition.as_str().map(str::to_string);
                        }
                        "@language" => {
                            context.language = definition.as_str().map(|s| s.to_lowercase());
                        }
                        "@version" => {}
                        term => {
                            let definition = match definition {
                                Value::String(iri) => TermDefinition {
                                    iri: iri.clone(),
                                    type_coercion: None,
                                    list_container: false,
                                    language: None,
                                },
                                Value::Object(body) => {
                                    let iri = body
                                        .get("@id")
                                        .and_then(Value::as_str)
                                        .map(str::to_string)
                                        .unwrap_or_else(|| term.to_string());
                                    let type_coercion = body
                                        .get("@type")
                                        .and_then(Value::as_str)
                                        .map(str::to_string);
                                    let list_container =
                                        body.get("@container").and_then(Value::as_str)
                                            == Some("@list");
                                    if let Some(container) =
                                        body.get("@container").and_then(Value::as_str)
                                    {
                                        if container != "@list" && container != "@set" {
                                            return Err(error(format!(
                                                "@container {container} is not supported"
                                            )));
                                        }
                                    }
                                    if body.contains_key("@reverse") {
                                        return Err(error("@reverse is not supported"));
                                    }
                                    let language = body
                                        .get("@language")
                                        .and_then(Value::as_str)
                                        .map(|s| s.to_lowercase());
                                    TermDefinition {
                                        iri,
                                        type_coercion,
                                        list_container,
                                        language,
                                    }
                                }
                                Value::Null => continue,
                                _ => {
                                    return Err(error(format!("bad definition for term `{term}`")))
                                }
                            };
                            context.terms.insert(term.to_string(), definition);
                        }
                    }
                }
            }
            _ => return Err(error("@context must be an object, array or null")),
        }
        // second pass: term IRIs may themselves be prefixed
        let snapshot = context.clone();
        for definition in context.terms.values_mut() {
            if let Some(expanded) = expand_prefixed(&snapshot, &definition.iri) {
                definition.iri = expanded;
            }
        }
        Ok(context)
    }

    /// Expands a key or `@type` value to an IRI (term, prefix:suffix, @vocab, absolute).
    fn expand_iri(
        &self,
        context: &Context,
        value: &str,
        vocab_position: bool,
    ) -> Result<Option<String>, ParseError> {
        if value.starts_with('@') {
            return Ok(None);
        }
        if let Some(definition) = context.terms.get(value) {
            return Ok(Some(definition.iri.clone()));
        }
        if let Some(expanded) = expand_prefixed(context, value) {
            return Ok(Some(expanded));
        }
        if is_absolute_iri(value) {
            return Ok(Some(value.to_string()));
        }
        if vocab_position {
            if let Some(vocab) = &context.vocab {
                return Ok(Some(format!("{vocab}{value}")));
            }
            // keys that expand to nothing are dropped, per spec
            return Ok(None);
        }
        // document-relative reference
        match &context.base {
            Some(base) => Ok(Some(
                iri::resolve(base, value).map_err(|e| error(format!("bad IRI: {e:?}")))?,
            )),
            None if value.is_empty() => Err(error("relative @id with no base")),
            None => Ok(Some(value.to_string())),
        }
    }

    fn node_term(&mut self, context: &Context, id: &str) -> Result<Vec<u8>, ParseError> {
        if let Some(label) = id.strip_prefix("_:") {
            let mut out = Vec::new();
            // `i` keeps surface labels disjoint from every other minting
            // domain; the random document namespace keeps identical source
            // labels from separate documents distinct.
            concise::encode_blank(&mut out, &format!("i{:032x}s{label}", self.label_ns));
            return Ok(out);
        }
        let expanded = self
            .expand_iri(context, id, false)?
            .ok_or_else(|| error(format!("cannot expand @id `{id}`")))?;
        let mut out = Vec::new();
        concise::encode_iri(&mut out, &expanded);
        Ok(out)
    }

    // -------------------------------------------------------------- nodes

    /// Parses a node object (or `@graph` wrapper); returns its subject term.
    fn parse_node(&mut self, value: &Value, inherited: &Context) -> Result<Vec<u8>, ParseError> {
        let Value::Object(map) = value else {
            return Err(error("node must be an object"));
        };
        let context = match map.get("@context") {
            Some(context_value) => self.merge_context(inherited, context_value)?,
            None => inherited.clone(),
        };
        if map.contains_key("@reverse") {
            return Err(error("@reverse is not supported"));
        }

        // subject
        let subject = match map.get("@id") {
            Some(Value::String(id)) => self.node_term(&context, id)?,
            Some(_) => return Err(error("@id must be a string")),
            None => self.fresh_blank(),
        };

        for (key, entry) in map {
            match key.as_str() {
                "@context" | "@id" => continue,
                "@type" => {
                    let types: Vec<&Value> = match entry {
                        Value::Array(items) => items.iter().collect(),
                        single => vec![single],
                    };
                    for type_value in types {
                        let Some(type_name) = type_value.as_str() else {
                            return Err(error("@type entries must be strings"));
                        };
                        let expanded = self
                            .expand_iri(&context, type_name, true)?
                            .ok_or_else(|| error(format!("cannot expand @type `{type_name}`")))?;
                        let mut object = Vec::new();
                        concise::encode_iri(&mut object, &expanded);
                        self.emit(subject.clone(), rdf_iri("type"), object);
                    }
                }
                "@graph" => {
                    let items: Vec<&Value> = match entry {
                        Value::Array(items) => items.iter().collect(),
                        single => vec![single],
                    };
                    for item in items {
                        self.parse_node(item, &context)?;
                    }
                }
                _ if key.starts_with('@') => {
                    return Err(error(format!("keyword {key} is not supported here")))
                }
                _ => {
                    let Some(predicate_iri) = self.expand_iri(&context, key, true)? else {
                        continue;
                    };
                    let definition = context.terms.get(key).cloned();
                    let mut predicate = Vec::new();
                    concise::encode_iri(&mut predicate, &predicate_iri);
                    self.parse_property_values(&context, &subject, &predicate, definition, entry)?;
                }
            }
        }
        Ok(subject)
    }

    fn parse_property_values(
        &mut self,
        context: &Context,
        subject: &[u8],
        predicate: &[u8],
        definition: Option<TermDefinition>,
        entry: &Value,
    ) -> Result<(), ParseError> {
        // @container: @list treats a bare array as a list
        if definition.as_ref().is_some_and(|d| d.list_container) {
            let items: Vec<&Value> = match entry {
                Value::Array(items) => items.iter().collect(),
                single => vec![single],
            };
            let list = self.build_list(context, &definition, items)?;
            self.emit(subject.to_vec(), predicate.to_vec(), list);
            return Ok(());
        }
        match entry {
            Value::Array(items) => {
                for item in items {
                    self.parse_property_values(
                        context,
                        subject,
                        predicate,
                        definition.clone(),
                        item,
                    )?;
                }
                Ok(())
            }
            single => {
                let object = self.parse_object(context, &definition, single)?;
                self.emit(subject.to_vec(), predicate.to_vec(), object);
                Ok(())
            }
        }
    }

    fn build_list(
        &mut self,
        context: &Context,
        definition: &Option<TermDefinition>,
        items: Vec<&Value>,
    ) -> Result<Vec<u8>, ParseError> {
        let mut cells = Vec::new();
        for _ in &items {
            cells.push(self.fresh_blank());
        }
        for (index, item) in items.into_iter().enumerate() {
            let object = self.parse_object(context, definition, item)?;
            self.emit(cells[index].clone(), rdf_iri("first"), object);
            let rest = if index + 1 < cells.len() {
                cells[index + 1].clone()
            } else {
                rdf_iri("nil")
            };
            self.emit(cells[index].clone(), rdf_iri("rest"), rest);
        }
        Ok(cells.into_iter().next().unwrap_or_else(|| rdf_iri("nil")))
    }

    /// One property value: value object, list object, node object/reference, or JSON
    /// native.
    fn parse_object(
        &mut self,
        context: &Context,
        definition: &Option<TermDefinition>,
        value: &Value,
    ) -> Result<Vec<u8>, ParseError> {
        match value {
            Value::Object(map) if map.contains_key("@value") => {
                self.parse_value_object(context, definition, map)
            }
            Value::Object(map) if map.contains_key("@list") => {
                let items: Vec<&Value> = match map.get("@list").unwrap() {
                    Value::Array(items) => items.iter().collect(),
                    single => vec![single],
                };
                self.build_list(context, definition, items)
            }
            Value::Object(_) => self.parse_node(value, context),
            Value::String(text) => {
                // @type: @id coerces strings to IRIs
                if let Some(definition) = definition {
                    if definition.type_coercion.as_deref() == Some("@id") {
                        return self.node_term(context, text);
                    }
                    if let Some(coercion) = &definition.type_coercion {
                        let datatype = self
                            .expand_iri(context, coercion, true)?
                            .unwrap_or_else(|| coercion.clone());
                        let mut out = Vec::new();
                        concise::encode_datatype(&mut out, text, &datatype);
                        return Ok(out);
                    }
                    if let Some(language) = &definition.language {
                        let mut out = Vec::new();
                        concise::encode_lang(&mut out, text, language, None);
                        return Ok(out);
                    }
                }
                let mut out = Vec::new();
                match &context.language {
                    Some(language) => concise::encode_lang(&mut out, text, language, None),
                    None => concise::encode_simple(&mut out, text),
                }
                Ok(out)
            }
            Value::Bool(flag) => {
                let mut out = Vec::new();
                concise::encode_datatype(
                    &mut out,
                    if *flag { "true" } else { "false" },
                    &format!("{XSD_NS}boolean"),
                );
                Ok(out)
            }
            Value::Number(number) => {
                let mut out = Vec::new();
                if number.is_i64() || number.is_u64() {
                    concise::encode_datatype(
                        &mut out,
                        &number.to_string(),
                        &format!("{XSD_NS}integer"),
                    );
                } else {
                    let lexical = format!("{:E}", number.as_f64().unwrap_or(0.0));
                    concise::encode_datatype(&mut out, &lexical, &format!("{XSD_NS}double"));
                }
                Ok(out)
            }
            Value::Null => Err(error("null property values are not supported")),
            Value::Array(_) => Err(error("nested arrays are not supported")),
        }
    }

    fn parse_value_object(
        &mut self,
        context: &Context,
        definition: &Option<TermDefinition>,
        map: &Map<String, Value>,
    ) -> Result<Vec<u8>, ParseError> {
        let value = map.get("@value").unwrap();
        let language = map
            .get("@language")
            .and_then(Value::as_str)
            .map(|s| s.to_lowercase());
        let datatype = match map.get("@type") {
            Some(Value::String(datatype)) => Some(
                self.expand_iri(context, datatype, true)?
                    .unwrap_or_else(|| datatype.clone()),
            ),
            Some(_) => return Err(error("@type in a value object must be a string")),
            None => None,
        };
        for key in map.keys() {
            if !matches!(key.as_str(), "@value" | "@language" | "@type" | "@index") {
                return Err(error(format!("unexpected key `{key}` in value object")));
            }
        }
        if language.is_some() && datatype.is_some() {
            return Err(error("@language and @type are mutually exclusive"));
        }

        let mut out = Vec::new();
        match value {
            Value::String(text) => match (&datatype, &language) {
                (Some(datatype), _) => concise::encode_datatype(&mut out, text, datatype),
                (None, Some(language)) => concise::encode_lang(&mut out, text, language, None),
                (None, None) => {
                    // an explicit @value ignores context/term default language? No —
                    // context @language applies to plain strings in value objects too
                    let language = definition
                        .as_ref()
                        .and_then(|d| d.language.clone())
                        .or_else(|| context.language.clone());
                    match language {
                        Some(language) => concise::encode_lang(&mut out, text, &language, None),
                        None => concise::encode_simple(&mut out, text),
                    }
                }
            },
            Value::Bool(flag) => concise::encode_datatype(
                &mut out,
                if *flag { "true" } else { "false" },
                datatype.as_deref().unwrap_or(&format!("{XSD_NS}boolean")),
            ),
            Value::Number(number) => {
                let default = if number.is_i64() || number.is_u64() {
                    format!("{XSD_NS}integer")
                } else {
                    format!("{XSD_NS}double")
                };
                let lexical = if number.is_i64() || number.is_u64() {
                    number.to_string()
                } else {
                    format!("{:E}", number.as_f64().unwrap_or(0.0))
                };
                concise::encode_datatype(
                    &mut out,
                    &lexical,
                    datatype.as_deref().unwrap_or(&default),
                )
            }
            Value::Null => return Err(error("@value must not be null")),
            _ => return Err(error("@value must be a scalar")),
        }
        Ok(out)
    }

    fn emit(&mut self, s: Vec<u8>, p: Vec<u8>, o: Vec<u8>) {
        self.triples.push(Triple { s, p, o });
    }
}

fn rdf_iri(local: &str) -> Vec<u8> {
    let mut out = Vec::new();
    concise::encode_iri(&mut out, &format!("{RDF_NS}{local}"));
    out
}

fn expand_prefixed(context: &Context, value: &str) -> Option<String> {
    let (prefix, suffix) = value.split_once(':')?;
    if suffix.starts_with("//") {
        return None;
    }
    let definition = context.terms.get(prefix)?;
    Some(format!("{}{suffix}", definition.iri))
}

fn is_absolute_iri(value: &str) -> bool {
    match value.split_once(':') {
        Some((scheme, _)) => {
            !scheme.is_empty()
                && scheme
                    .chars()
                    .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '+' | '-' | '.'))
                && scheme
                    .chars()
                    .next()
                    .is_some_and(|ch| ch.is_ascii_alphabetic())
        }
        None => false,
    }
}

// ---------------------------------------------------------------- serializer

/// Serializes concise triples as an expanded-form JSON-LD document (`@graph` of node
/// objects keyed by full IRIs).
pub fn write_jsonld(triples: &[Triple]) -> String {
    let mut order: Vec<&[u8]> = Vec::new();
    for triple in triples {
        if !order.contains(&triple.s.as_slice()) {
            order.push(&triple.s);
        }
    }
    let mut nodes = Vec::new();
    for subject in order {
        let mut node = Map::new();
        let id = match subject.first() {
            Some(b'>') => String::from_utf8_lossy(&subject[1..]).into_owned(),
            Some(b'_') => format!("_:{}", String::from_utf8_lossy(&subject[1..])),
            _ => continue,
        };
        node.insert("@id".to_string(), Value::String(id));
        for triple in triples {
            if triple.s.as_slice() != subject {
                continue;
            }
            let predicate = String::from_utf8_lossy(&triple.p[1..]).into_owned();
            let object = concise_to_jsonld(&triple.o);
            let (key, value) = if predicate == format!("{RDF_NS}type") {
                match object {
                    Value::Object(map) => (
                        "@type".to_string(),
                        map.get("@id").cloned().unwrap_or(Value::Null),
                    ),
                    other => ("@type".to_string(), other),
                }
            } else {
                (predicate, object)
            };
            match node.get_mut(&key) {
                Some(Value::Array(items)) => items.push(value),
                Some(existing) => {
                    let first = existing.take();
                    *existing = Value::Array(vec![first, value]);
                }
                None => {
                    node.insert(key, value);
                }
            }
        }
        nodes.push(Value::Object(node));
    }
    serde_json::to_string_pretty(&serde_json::json!({ "@graph": nodes }))
        .unwrap_or_else(|_| "{}".to_string())
}

fn concise_to_jsonld(term: &[u8]) -> Value {
    match term.first() {
        Some(b'>') => serde_json::json!({
            "@id": String::from_utf8_lossy(&term[1..]).into_owned()
        }),
        Some(b'_') => serde_json::json!({
            "@id": format!("_:{}", String::from_utf8_lossy(&term[1..]))
        }),
        Some(b'"') => Value::String(String::from_utf8_lossy(&term[1..]).into_owned()),
        Some(b'@') => {
            let quote = term.iter().position(|&b| b == b'"').unwrap_or(term.len());
            let tag = String::from_utf8_lossy(&term[1..quote]).into_owned();
            let tag = tag.split("--").next().unwrap_or(&tag).to_string();
            serde_json::json!({
                "@value": String::from_utf8_lossy(&term[quote + 1..]).into_owned(),
                "@language": tag,
            })
        }
        Some(b'^') => {
            let quote = term.iter().position(|&b| b == b'"').unwrap_or(term.len());
            serde_json::json!({
                "@value": String::from_utf8_lossy(&term[quote + 1..]).into_owned(),
                "@type": String::from_utf8_lossy(&term[2..quote]).into_owned(),
            })
        }
        _ => Value::Null,
    }
}
