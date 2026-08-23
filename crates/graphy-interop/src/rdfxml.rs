//! RDF/XML parser and serializer (RDF 1.1 XML Syntax). The parser walks the XML event
//! stream with the grammar's node-element / property-element alternation, handling
//! rdf:about/ID/nodeID, typed nodes, property attributes, rdf:resource/nodeID/datatype,
//! parseType Resource/Collection/Literal, rdf:li membership, xml:base/xml:lang scoping,
//! and rdf:ID statement reification. Emits concise-encoded triples.

use std::collections::{BTreeMap, HashSet};

use graphy_core::{concise, iri};

use crate::xml::{Element, Event, XmlError, XmlReader};
use crate::{document_label_ns, ParseError, ParseOptions, Triple};

pub const RDF_NS: &str = "http://www.w3.org/1999/02/22-rdf-syntax-ns#";
const XML_NS: &str = "http://www.w3.org/XML/1998/namespace";
const XML_LITERAL: &str = "http://www.w3.org/1999/02/22-rdf-syntax-ns#XMLLiteral";

/// Node-element names from the rdf: namespace that the grammar forbids.
const FORBIDDEN_NODE_NAMES: [&str; 10] = [
    "RDF",
    "ID",
    "about",
    "parseType",
    "resource",
    "nodeID",
    "datatype",
    "aboutEach",
    "aboutEachPrefix",
    "bagID",
];
/// Property-element names from the rdf: namespace that the grammar forbids.
const FORBIDDEN_PROPERTY_NAMES: [&str; 10] = [
    "Description",
    "RDF",
    "ID",
    "about",
    "parseType",
    "resource",
    "nodeID",
    "aboutEach",
    "aboutEachPrefix",
    "bagID",
];
/// Old-RDF attributes that are outright errors.
const REJECTED_ATTRS: [&str; 3] = ["aboutEach", "aboutEachPrefix", "bagID"];

#[derive(Clone)]
struct Scope {
    /// prefix → namespace; "" is the default namespace (elements only).
    namespaces: BTreeMap<String, String>,
    base: Option<String>,
    lang: Option<String>,
}

pub struct RdfXmlParser<'a> {
    reader: XmlReader<'a>,
    triples: Vec<Triple>,
    blank_counter: usize,
    label_ns: u128,
    /// rdf:ID values seen per base (duplicates are errors).
    seen_ids: HashSet<String>,
}

fn error(message: impl Into<String>) -> ParseError {
    ParseError(message.into())
}

impl From<XmlError> for ParseError {
    fn from(e: XmlError) -> Self {
        ParseError(e.to_string())
    }
}

/// Parses an RDF/XML document into concise triples.
pub fn parse_rdfxml(source: &str, base: Option<&str>) -> Result<Vec<Triple>, ParseError> {
    parse_rdfxml_with_options(source, base, ParseOptions::default())
}

/// Parses an RDF/XML document with explicit parser options.
pub fn parse_rdfxml_with_options(
    source: &str,
    base: Option<&str>,
    options: ParseOptions,
) -> Result<Vec<Triple>, ParseError> {
    let mut parser = RdfXmlParser {
        reader: XmlReader::new(source),
        triples: Vec::new(),
        blank_counter: 0,
        label_ns: document_label_ns(options),
        seen_ids: HashSet::new(),
    };
    let mut scope = Scope {
        namespaces: BTreeMap::new(),
        base: base.map(str::to_string),
        lang: None,
    };
    scope
        .namespaces
        .insert("xml".to_string(), XML_NS.to_string());

    // document element: rdf:RDF wrapping node elements, or a single node element
    loop {
        match parser.reader.next()? {
            Event::Eof => return Err(error("empty document")),
            Event::Text(text) if text.trim().is_empty() => continue,
            Event::Text(_) => return Err(error("unexpected character data at document root")),
            Event::Start { element, empty } => {
                let scope = parser.element_scope(&scope, &element)?;
                let (namespace, local) = parser.expand_element_name(&scope, &element)?;
                if namespace == RDF_NS && local == "RDF" {
                    parser.check_rdf_attrs(&scope, &element, &["about", "ID", "nodeID"])?;
                    if !empty {
                        parser.parse_node_element_list(&scope)?;
                    }
                } else {
                    parser.parse_node_element(&scope, element, empty)?;
                }
                break;
            }
            Event::End(_) => return Err(error("unexpected end tag")),
        }
    }
    // trailing content
    loop {
        match parser.reader.next()? {
            Event::Eof => break,
            Event::Text(text) if text.trim().is_empty() => continue,
            _ => return Err(error("content after the document element")),
        }
    }
    Ok(parser.triples)
}

impl<'a> RdfXmlParser<'a> {
    // ------------------------------------------------------------- scoping

    /// Applies an element's xmlns/xml:base/xml:lang declarations to the inherited scope.
    fn element_scope(&self, inherited: &Scope, element: &Element) -> Result<Scope, ParseError> {
        let mut scope = inherited.clone();
        for (name, value) in &element.attrs {
            if name == "xmlns" {
                scope.namespaces.insert(String::new(), value.clone());
            } else if let Some(prefix) = name.strip_prefix("xmlns:") {
                if value.is_empty() {
                    return Err(error(format!("empty namespace for prefix `{prefix}`")));
                }
                scope.namespaces.insert(prefix.to_string(), value.clone());
            } else if name == "xml:base" {
                let resolved = match &inherited.base {
                    Some(base) => iri::resolve(base, value)
                        .map_err(|e| error(format!("bad xml:base: {e:?}")))?,
                    None => value.clone(),
                };
                // xml:base fragments are dropped
                let resolved = match resolved.split_once('#') {
                    Some((head, _)) => head.to_string(),
                    None => resolved,
                };
                scope.base = Some(resolved);
            } else if name == "xml:lang" {
                scope.lang = if value.is_empty() {
                    None
                } else {
                    Some(value.to_lowercase())
                };
            }
        }
        Ok(scope)
    }

    fn expand_element_name(
        &self,
        scope: &Scope,
        element: &Element,
    ) -> Result<(String, String), ParseError> {
        match element.qname.split_once(':') {
            Some((prefix, local)) => {
                let namespace = scope
                    .namespaces
                    .get(prefix)
                    .ok_or_else(|| error(format!("undeclared prefix `{prefix}`")))?;
                Ok((namespace.clone(), local.to_string()))
            }
            None => {
                let namespace = scope.namespaces.get("").cloned().unwrap_or_default();
                if namespace.is_empty() {
                    return Err(error(format!(
                        "element `{}` has no namespace",
                        element.qname
                    )));
                }
                Ok((namespace, element.qname.clone()))
            }
        }
    }

    /// Expands a non-xmlns attribute name; unprefixed attributes have no namespace.
    fn expand_attr_name(&self, scope: &Scope, qname: &str) -> Result<(String, String), ParseError> {
        match qname.split_once(':') {
            Some((prefix, local)) => {
                let namespace = scope
                    .namespaces
                    .get(prefix)
                    .ok_or_else(|| error(format!("undeclared prefix `{prefix}`")))?;
                Ok((namespace.clone(), local.to_string()))
            }
            None => Ok((String::new(), qname.to_string())),
        }
    }

    // ------------------------------------------------------------ helpers

    fn fresh_blank(&mut self) -> Vec<u8> {
        let label = format!("g{:032x}b{}", self.label_ns, self.blank_counter);
        self.blank_counter += 1;
        let mut out = Vec::new();
        concise::encode_blank(&mut out, &label);
        out
    }

    fn node_id_blank(&self, label: &str) -> Result<Vec<u8>, ParseError> {
        if !is_ncname(label) {
            return Err(error(format!("rdf:nodeID `{label}` is not an XML name")));
        }
        let mut out = Vec::new();
        // Same document-scoped `i` surface-label domain as JSON-LD.
        concise::encode_blank(&mut out, &format!("i{:032x}s{label}", self.label_ns));
        Ok(out)
    }

    fn resolve(&self, scope: &Scope, reference: &str) -> Result<Vec<u8>, ParseError> {
        let absolute = match &scope.base {
            Some(base) => iri::resolve(base, reference)
                .map_err(|e| error(format!("cannot resolve `{reference}`: {e:?}")))?,
            None => {
                if reference.is_empty() || reference.starts_with('#') {
                    return Err(error("relative IRI with no base"));
                }
                reference.to_string()
            }
        };
        let mut out = Vec::new();
        concise::encode_iri(&mut out, &absolute);
        Ok(out)
    }

    fn id_iri(&mut self, scope: &Scope, id: &str) -> Result<Vec<u8>, ParseError> {
        if !is_ncname(id) {
            return Err(error(format!("rdf:ID `{id}` is not an XML name")));
        }
        let key = format!("{}#{id}", scope.base.as_deref().unwrap_or(""));
        if !self.seen_ids.insert(key) {
            return Err(error(format!("duplicate rdf:ID `{id}`")));
        }
        self.resolve(scope, &format!("#{id}"))
    }

    fn rdf_iri(&self, local: &str) -> Vec<u8> {
        let mut out = Vec::new();
        concise::encode_iri(&mut out, &format!("{RDF_NS}{local}"));
        out
    }

    fn emit(&mut self, s: Vec<u8>, p: Vec<u8>, o: Vec<u8>) {
        self.triples.push(Triple { s, p, o });
    }

    fn literal(&self, lexical: &str, lang: Option<&str>, datatype: Option<&str>) -> Vec<u8> {
        let mut out = Vec::new();
        match (datatype, lang) {
            (Some(datatype), _) if datatype == graphy_core::vocab::XSD_STRING => {
                concise::encode_simple(&mut out, lexical)
            }
            (Some(datatype), _) => concise::encode_datatype(&mut out, lexical, datatype),
            (None, Some(lang)) => concise::encode_lang(&mut out, lexical, lang, None),
            (None, None) => concise::encode_simple(&mut out, lexical),
        }
        out
    }

    /// Guards against grammar-invalid rdf: attributes on rdf:RDF / node elements.
    fn check_rdf_attrs(
        &self,
        scope: &Scope,
        element: &Element,
        forbidden: &[&str],
    ) -> Result<(), ParseError> {
        for (qname, _) in &element.attrs {
            if qname.starts_with("xmlns") || qname.starts_with("xml:") {
                continue;
            }
            let (namespace, local) = self.expand_attr_name(scope, qname)?;
            if namespace == RDF_NS
                && (forbidden.contains(&local.as_str()) || REJECTED_ATTRS.contains(&local.as_str()))
            {
                return Err(error(format!("attribute rdf:{local} not allowed here")));
            }
        }
        Ok(())
    }

    // ------------------------------------------------------ node elements

    fn parse_node_element_list(&mut self, scope: &Scope) -> Result<(), ParseError> {
        loop {
            match self.reader.next()? {
                Event::End(_) => return Ok(()),
                Event::Text(text) if text.trim().is_empty() => continue,
                Event::Text(_) => return Err(error("character data between node elements")),
                Event::Start { element, empty } => {
                    let scope = self.element_scope(scope, &element)?;
                    self.parse_node_element(&scope, element, empty)?;
                }
                Event::Eof => return Err(error("unexpected end of document")),
            }
        }
    }

    /// Parses one node element (scope already extended); returns its subject term.
    fn parse_node_element(
        &mut self,
        scope: &Scope,
        element: Element,
        empty: bool,
    ) -> Result<Vec<u8>, ParseError> {
        let (namespace, local) = self.expand_element_name(scope, &element)?;
        if namespace == RDF_NS && (FORBIDDEN_NODE_NAMES.contains(&local.as_str()) || local == "li")
        {
            return Err(error(format!("rdf:{local} is not a valid node element")));
        }

        // subject from rdf:about / rdf:ID / rdf:nodeID (mutually exclusive)
        let mut subject: Option<Vec<u8>> = None;
        let mut exclusive = 0;
        for (qname, value) in &element.attrs {
            if qname.starts_with("xmlns") || qname.starts_with("xml:") {
                continue;
            }
            let (attr_ns, attr_local) = self.expand_attr_name(scope, qname)?;
            if attr_ns == RDF_NS {
                match attr_local.as_str() {
                    "about" => {
                        subject = Some(self.resolve(scope, value)?);
                        exclusive += 1;
                    }
                    "ID" => {
                        subject = Some(self.id_iri(scope, value)?);
                        exclusive += 1;
                    }
                    "nodeID" => {
                        subject = Some(self.node_id_blank(value)?);
                        exclusive += 1;
                    }
                    _ => {}
                }
            }
        }
        if exclusive > 1 {
            return Err(error(
                "rdf:about, rdf:ID and rdf:nodeID are mutually exclusive",
            ));
        }
        let subject = match subject {
            Some(subject) => subject,
            None => self.fresh_blank(),
        };

        // typed node element
        if !(namespace == RDF_NS && local == "Description") {
            let mut type_iri = Vec::new();
            concise::encode_iri(&mut type_iri, &format!("{namespace}{local}"));
            self.emit(subject.clone(), self.rdf_iri("type"), type_iri);
        }

        // property attributes
        for (qname, value) in &element.attrs {
            if qname.starts_with("xmlns") || qname.starts_with("xml:") || qname == "xmlns" {
                continue;
            }
            let (attr_ns, attr_local) = self.expand_attr_name(scope, qname)?;
            if attr_ns == RDF_NS {
                match attr_local.as_str() {
                    "about" | "ID" | "nodeID" => continue,
                    "type" => {
                        let object = self.resolve(scope, value)?;
                        self.emit(subject.clone(), self.rdf_iri("type"), object);
                        continue;
                    }
                    "li" => return Err(error("rdf:li is not allowed as an attribute")),
                    other if REJECTED_ATTRS.contains(&other) => {
                        return Err(error(format!("attribute rdf:{other} not allowed")))
                    }
                    other if FORBIDDEN_PROPERTY_NAMES.contains(&other) => {
                        return Err(error(format!("attribute rdf:{other} not allowed here")))
                    }
                    // remaining rdf-namespace attributes are property attributes (e.g.
                    // rdf:value); fall through
                    _ => {}
                }
            } else if attr_ns.is_empty() {
                // names beginning with "xml" are reserved by XML: ignore, don't error
                if attr_local.to_ascii_lowercase().starts_with("xml") {
                    continue;
                }
                return Err(error(format!(
                    "unqualified attribute `{qname}` is not allowed"
                )));
            }
            let mut predicate = Vec::new();
            concise::encode_iri(&mut predicate, &format!("{attr_ns}{attr_local}"));
            let object = self.literal(value, scope.lang.as_deref(), None);
            self.emit(subject.clone(), predicate, object);
        }

        // property element children
        if !empty {
            let mut li_counter = 0usize;
            loop {
                match self.reader.next()? {
                    Event::End(_) => break,
                    Event::Text(text) if text.trim().is_empty() => continue,
                    Event::Text(_) => return Err(error("character data inside a node element")),
                    Event::Start { element, empty } => {
                        let scope = self.element_scope(scope, &element)?;
                        self.parse_property_element(
                            &scope,
                            element,
                            empty,
                            &subject,
                            &mut li_counter,
                        )?;
                    }
                    Event::Eof => return Err(error("unexpected end of document")),
                }
            }
        }
        Ok(subject)
    }

    // -------------------------------------------------- property elements

    fn parse_property_element(
        &mut self,
        scope: &Scope,
        element: Element,
        empty: bool,
        subject: &[u8],
        li_counter: &mut usize,
    ) -> Result<(), ParseError> {
        let (namespace, local) = self.expand_element_name(scope, &element)?;
        if namespace == RDF_NS && FORBIDDEN_PROPERTY_NAMES.contains(&local.as_str()) {
            return Err(error(format!(
                "rdf:{local} is not a valid property element"
            )));
        }
        let predicate_iri = if namespace == RDF_NS && local == "li" {
            *li_counter += 1;
            format!("{RDF_NS}_{li_counter}")
        } else {
            format!("{namespace}{local}")
        };
        let mut predicate = Vec::new();
        concise::encode_iri(&mut predicate, &predicate_iri);

        // attribute roles
        let mut reify_id: Option<Vec<u8>> = None;
        let mut resource: Option<Vec<u8>> = None;
        let mut node_id: Option<Vec<u8>> = None;
        let mut datatype: Option<String> = None;
        let mut parse_type: Option<String> = None;
        let mut property_attrs: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();

        for (qname, value) in &element.attrs {
            if qname.starts_with("xmlns") || qname.starts_with("xml:") {
                continue;
            }
            let (attr_ns, attr_local) = self.expand_attr_name(scope, qname)?;
            if attr_ns == RDF_NS {
                match attr_local.as_str() {
                    "ID" => {
                        reify_id = Some(self.id_iri(scope, value)?);
                        continue;
                    }
                    "resource" => {
                        resource = Some(self.resolve(scope, value)?);
                        continue;
                    }
                    "nodeID" => {
                        node_id = Some(self.node_id_blank(value)?);
                        continue;
                    }
                    "datatype" => {
                        let mut resolved = Vec::new();
                        // datatype IRIs resolve against the base too
                        let absolute = match &scope.base {
                            Some(base) => iri::resolve(base, value)
                                .map_err(|e| error(format!("bad rdf:datatype: {e:?}")))?,
                            None => value.clone(),
                        };
                        concise::encode_iri(&mut resolved, &absolute);
                        datatype = Some(absolute);
                        let _ = resolved;
                        continue;
                    }
                    "parseType" => {
                        parse_type = Some(value.clone());
                        continue;
                    }
                    "type" => {
                        let object = self.resolve(scope, value)?;
                        property_attrs.push((self.rdf_iri("type"), object));
                        continue;
                    }
                    "li" => return Err(error("rdf:li is not allowed as an attribute")),
                    other if REJECTED_ATTRS.contains(&other) => {
                        return Err(error(format!("attribute rdf:{other} not allowed")))
                    }
                    other if FORBIDDEN_PROPERTY_NAMES.contains(&other) => {
                        return Err(error(format!("attribute rdf:{other} not allowed here")))
                    }
                    _ => {}
                }
            } else if attr_ns.is_empty() {
                if attr_local.to_ascii_lowercase().starts_with("xml") {
                    continue;
                }
                return Err(error(format!(
                    "unqualified attribute `{qname}` is not allowed"
                )));
            }
            let mut attr_predicate = Vec::new();
            concise::encode_iri(&mut attr_predicate, &format!("{attr_ns}{attr_local}"));
            let object = self.literal(value, scope.lang.as_deref(), None);
            property_attrs.push((attr_predicate, object));
        }

        if resource.is_some() && node_id.is_some() {
            return Err(error("rdf:resource and rdf:nodeID are mutually exclusive"));
        }
        if parse_type.is_some()
            && (resource.is_some()
                || node_id.is_some()
                || datatype.is_some()
                || !property_attrs.is_empty())
        {
            return Err(error("rdf:parseType excludes other object attributes"));
        }

        let object: Vec<u8> = match parse_type.as_deref() {
            Some("Resource") => {
                let object = self.fresh_blank();
                if !empty {
                    let mut inner_li = 0usize;
                    loop {
                        match self.reader.next()? {
                            Event::End(_) => break,
                            Event::Text(text) if text.trim().is_empty() => continue,
                            Event::Text(_) => {
                                return Err(error("character data in parseType=\"Resource\""))
                            }
                            Event::Start { element, empty } => {
                                let scope = self.element_scope(scope, &element)?;
                                self.parse_property_element(
                                    &scope,
                                    element,
                                    empty,
                                    &object,
                                    &mut inner_li,
                                )?;
                            }
                            Event::Eof => return Err(error("unexpected end of document")),
                        }
                    }
                }
                object
            }
            Some("Collection") => {
                let mut items = Vec::new();
                if !empty {
                    loop {
                        match self.reader.next()? {
                            Event::End(_) => break,
                            Event::Text(text) if text.trim().is_empty() => continue,
                            Event::Text(_) => {
                                return Err(error("character data in parseType=\"Collection\""))
                            }
                            Event::Start { element, empty } => {
                                let scope = self.element_scope(scope, &element)?;
                                items.push(self.parse_node_element(&scope, element, empty)?);
                            }
                            Event::Eof => return Err(error("unexpected end of document")),
                        }
                    }
                }
                // build the rdf list
                let mut head = self.rdf_iri("nil");
                let mut cells: Vec<Vec<u8>> = Vec::new();
                for _ in &items {
                    cells.push(self.fresh_blank());
                }
                for (index, item) in items.into_iter().enumerate() {
                    self.emit(cells[index].clone(), self.rdf_iri("first"), item);
                    let rest = if index + 1 < cells.len() {
                        cells[index + 1].clone()
                    } else {
                        self.rdf_iri("nil")
                    };
                    self.emit(cells[index].clone(), self.rdf_iri("rest"), rest);
                }
                if let Some(first_cell) = cells.first() {
                    head = first_cell.clone();
                }
                head
            }
            Some(_) => {
                // any other parseType is treated as Literal
                let raw = if empty {
                    String::new()
                } else {
                    self.capture_canonical_literal(scope)?
                };
                self.literal(&raw, None, Some(XML_LITERAL))
            }
            None => {
                if let Some(resource) = resource {
                    self.expect_empty(empty)?;
                    self.finish_object_attrs(&resource, property_attrs);
                    resource
                } else if let Some(node_id) = node_id {
                    self.expect_empty(empty)?;
                    self.finish_object_attrs(&node_id, property_attrs);
                    node_id
                } else if !property_attrs.is_empty() {
                    // empty property element with property attributes → blank object
                    self.expect_empty(empty)?;
                    let object = self.fresh_blank();
                    self.finish_object_attrs(&object, property_attrs);
                    object
                } else if empty {
                    self.literal("", scope.lang.as_deref(), datatype.as_deref())
                } else {
                    self.parse_property_content(scope, subject, &predicate, datatype.as_deref())?
                }
            }
        };

        self.emit(subject.to_vec(), predicate.clone(), object.clone());
        if let Some(statement) = reify_id {
            self.emit(
                statement.clone(),
                self.rdf_iri("type"),
                self.rdf_iri("Statement"),
            );
            self.emit(statement.clone(), self.rdf_iri("subject"), subject.to_vec());
            self.emit(statement.clone(), self.rdf_iri("predicate"), predicate);
            self.emit(statement, self.rdf_iri("object"), object);
        }
        Ok(())
    }

    /// Content of a plain property element: either character data (literal) or exactly
    /// one nested node element (resource object).
    fn parse_property_content(
        &mut self,
        scope: &Scope,
        _subject: &[u8],
        _predicate: &[u8],
        datatype: Option<&str>,
    ) -> Result<Vec<u8>, ParseError> {
        let mut text = String::new();
        let mut node: Option<Vec<u8>> = None;
        loop {
            match self.reader.next()? {
                Event::End(_) => break,
                Event::Text(chunk) => text.push_str(&chunk),
                Event::Start { element, empty } => {
                    if node.is_some() {
                        return Err(error("more than one node element in property content"));
                    }
                    if !text.trim().is_empty() {
                        return Err(error("mixed text and element content"));
                    }
                    let scope = self.element_scope(scope, &element)?;
                    node = Some(self.parse_node_element(&scope, element, empty)?);
                }
                Event::Eof => return Err(error("unexpected end of document")),
            }
        }
        match node {
            Some(node) => {
                if !text.trim().is_empty() {
                    return Err(error("mixed text and element content"));
                }
                Ok(node)
            }
            None => Ok(self.literal(&text, scope.lang.as_deref(), datatype)),
        }
    }

    /// Emits property-attribute triples about an object node.
    fn finish_object_attrs(&mut self, object: &[u8], attrs: Vec<(Vec<u8>, Vec<u8>)>) {
        for (predicate, value) in attrs {
            self.emit(object.to_vec(), predicate, value);
        }
    }

    fn expect_empty(&mut self, empty: bool) -> Result<(), ParseError> {
        if empty {
            return Ok(());
        }
        loop {
            match self.reader.next()? {
                Event::End(_) => return Ok(()),
                Event::Text(text) if text.trim().is_empty() => continue,
                _ => return Err(error("property element must be empty")),
            }
        }
    }

    /// Canonicalize the XML infoset inside `parseType="Literal"`. The root
    /// of each captured subtree inherits the in-scope namespace nodes.
    fn capture_canonical_literal(&mut self, scope: &Scope) -> Result<String, ParseError> {
        let mut out = String::new();
        loop {
            match self.reader.next()? {
                Event::Start { element, empty } => {
                    self.write_canonical_element(&mut out, scope, element, empty, true)?
                }
                Event::End(_) => break,
                Event::Text(text) => push_xml_text(&mut out, &text),
                Event::Eof => return Err(error("unterminated XML literal")),
            }
        }
        Ok(out)
    }

    fn write_canonical_element(
        &mut self,
        out: &mut String,
        inherited: &Scope,
        element: Element,
        empty: bool,
        subtree_root: bool,
    ) -> Result<(), ParseError> {
        let scope = self.element_scope(inherited, &element)?;
        out.push('<');
        out.push_str(&element.qname);

        if subtree_root {
            // RDF/XML canonicalization carries the namespace axis from the
            // containing document into an XML-literal subtree. Keep rdf
            // first (the RDF test-suite canonical form), then lexical order.
            if let Some(ns) = scope.namespaces.get("rdf") {
                push_xml_attr(out, "xmlns:rdf", ns);
            }
            for (prefix, ns) in &scope.namespaces {
                if prefix == "rdf" || prefix == "xml" {
                    continue;
                }
                if prefix.is_empty() {
                    push_xml_attr(out, "xmlns", ns);
                } else {
                    push_xml_attr(out, &format!("xmlns:{prefix}"), ns);
                }
            }
        } else {
            let mut declarations: Vec<_> = element
                .attrs
                .iter()
                .filter(|(name, _)| name == "xmlns" || name.starts_with("xmlns:"))
                .collect();
            declarations.sort_by(|a, b| a.0.cmp(&b.0));
            for (name, value) in declarations {
                push_xml_attr(out, name, value);
            }
        }

        let mut attrs: Vec<_> = element
            .attrs
            .iter()
            .filter(|(name, _)| name != "xmlns" && !name.starts_with("xmlns:"))
            .collect();
        attrs.sort_by(|a, b| a.0.cmp(&b.0));
        for (name, value) in attrs {
            push_xml_attr(out, name, value);
        }
        out.push('>');

        if !empty {
            loop {
                match self.reader.next()? {
                    Event::Start { element, empty } => {
                        self.write_canonical_element(out, &scope, element, empty, false)?
                    }
                    Event::End(_) => break,
                    Event::Text(text) => push_xml_text(out, &text),
                    Event::Eof => return Err(error("unterminated XML literal element")),
                }
            }
        }
        out.push_str("</");
        out.push_str(&element.qname);
        out.push('>');
        Ok(())
    }
}

fn push_xml_attr(out: &mut String, name: &str, value: &str) {
    out.push(' ');
    out.push_str(name);
    out.push_str("=\"");
    for ch in value.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '"' => out.push_str("&quot;"),
            '\t' => out.push_str("&#x9;"),
            '\n' => out.push_str("&#xA;"),
            '\r' => out.push_str("&#xD;"),
            c => out.push(c),
        }
    }
    out.push('"');
}

fn push_xml_text(out: &mut String, value: &str) {
    for ch in value.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '\r' => out.push_str("&#xD;"),
            c => out.push(c),
        }
    }
}

fn is_ncname(value: &str) -> bool {
    let mut chars = value.chars();
    match chars.next() {
        Some(first) if first.is_alphabetic() || first == '_' => {}
        _ => return false,
    }
    chars.all(|ch| ch.is_alphanumeric() || matches!(ch, '_' | '-' | '.') || ch as u32 == 0xB7)
}

// ---------------------------------------------------------------- serializer

/// Serializes concise triples as RDF/XML: one rdf:Description per subject, properties as
/// rdf:resource/rdf:nodeID/literal elements with generated namespace prefixes.
pub fn write_rdfxml(triples: &[Triple]) -> String {
    // predicate namespaces → generated prefixes
    let mut prefixes: BTreeMap<String, String> = BTreeMap::new();
    let mut split_cache: Vec<(String, String, String)> = Vec::new(); // (iri, ns, local)
    for triple in triples {
        let predicate = String::from_utf8_lossy(&triple.p[1..]).into_owned();
        let (namespace, local) = split_iri(&predicate);
        if !prefixes.contains_key(&namespace) {
            let prefix = if namespace == RDF_NS {
                "rdf".to_string()
            } else {
                format!("ns{}", prefixes.len())
            };
            prefixes.insert(namespace.clone(), prefix);
        }
        split_cache.push((predicate, namespace, local));
    }
    prefixes
        .entry(RDF_NS.to_string())
        .or_insert_with(|| "rdf".to_string());

    let mut out = String::new();
    out.push_str("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<rdf:RDF");
    for (namespace, prefix) in &prefixes {
        out.push_str(&format!(
            "\n    xmlns:{prefix}=\"{}\"",
            escape_attr(namespace)
        ));
    }
    out.push_str(">\n");

    // group triples by subject, preserving first-seen order
    let mut order: Vec<&[u8]> = Vec::new();
    for triple in triples {
        if !order.contains(&triple.s.as_slice()) {
            order.push(&triple.s);
        }
    }
    for subject in order {
        out.push_str("  <rdf:Description");
        match subject.first() {
            Some(b'>') => out.push_str(&format!(
                " rdf:about=\"{}\"",
                escape_attr(&String::from_utf8_lossy(&subject[1..]))
            )),
            Some(b'_') => out.push_str(&format!(
                " rdf:nodeID=\"{}\"",
                escape_attr(&String::from_utf8_lossy(&subject[1..]))
            )),
            _ => {}
        }
        out.push_str(">\n");
        for (index, triple) in triples.iter().enumerate() {
            if triple.s.as_slice() != subject {
                continue;
            }
            let (_, namespace, local) = &split_cache[index];
            let prefix = &prefixes[namespace];
            let tag = format!("{prefix}:{local}");
            match triple.o.first() {
                Some(b'>') => out.push_str(&format!(
                    "    <{tag} rdf:resource=\"{}\"/>\n",
                    escape_attr(&String::from_utf8_lossy(&triple.o[1..]))
                )),
                Some(b'_') => out.push_str(&format!(
                    "    <{tag} rdf:nodeID=\"{}\"/>\n",
                    escape_attr(&String::from_utf8_lossy(&triple.o[1..]))
                )),
                Some(b'"') => out.push_str(&format!(
                    "    <{tag}>{}</{tag}>\n",
                    escape_text(&String::from_utf8_lossy(&triple.o[1..]))
                )),
                Some(b'@') => {
                    let bytes = &triple.o;
                    let quote = bytes.iter().position(|&b| b == b'"').unwrap_or(bytes.len());
                    let lang = String::from_utf8_lossy(&bytes[1..quote]);
                    let lang = lang.split("--").next().unwrap_or(&lang).to_string();
                    let lexical = String::from_utf8_lossy(&bytes[quote + 1..]);
                    out.push_str(&format!(
                        "    <{tag} xml:lang=\"{}\">{}</{tag}>\n",
                        escape_attr(&lang),
                        escape_text(&lexical)
                    ));
                }
                Some(b'^') => {
                    let bytes = &triple.o;
                    let quote = bytes.iter().position(|&b| b == b'"').unwrap_or(bytes.len());
                    let datatype = String::from_utf8_lossy(&bytes[2..quote]);
                    let lexical = String::from_utf8_lossy(&bytes[quote + 1..]);
                    out.push_str(&format!(
                        "    <{tag} rdf:datatype=\"{}\">{}</{tag}>\n",
                        escape_attr(&datatype),
                        escape_text(&lexical)
                    ));
                }
                _ => {}
            }
        }
        out.push_str("  </rdf:Description>\n");
    }
    out.push_str("</rdf:RDF>\n");
    out
}

/// Splits a predicate IRI into (namespace, NCName local part) at the last viable point.
fn split_iri(iri: &str) -> (String, String) {
    let split_at = iri
        .char_indices()
        .rev()
        .take_while(|(_, ch)| ch.is_alphanumeric() || matches!(ch, '_' | '-' | '.'))
        .last()
        .map(|(index, _)| index)
        .unwrap_or(iri.len());
    // the local part must start with a name-start character
    let mut split_at = split_at;
    while split_at < iri.len() {
        let ch = iri[split_at..].chars().next().unwrap();
        if ch.is_alphabetic() || ch == '_' {
            break;
        }
        split_at += ch.len_utf8();
    }
    (iri[..split_at].to_string(), iri[split_at..].to_string())
}

fn escape_text(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn escape_attr(value: &str) -> String {
    escape_text(value).replace('"', "&quot;")
}
