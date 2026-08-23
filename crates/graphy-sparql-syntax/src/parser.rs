//! Recursive-descent parser for SPARQL Query (doc 04 §2): mirrors the
//! grammar productions, Pratt-style precedence for expressions, and a
//! depth guard on every recursive production (queries are hostile input;
//! stack overflow is a DoS vector). Prefixed names and relative IRIs
//! resolve against the prologue during the parse; collections, blank-node
//! property lists, and SPARQL 1.2 reification sugar expand to plain
//! triples here so the algebra sees only ordinary patterns.

use std::collections::HashMap;

use graphy_core::vocab::{
    RDF_FIRST, RDF_NIL, RDF_REIFIES, RDF_REST, RDF_TYPE, XSD_BOOLEAN, XSD_DECIMAL, XSD_DOUBLE,
    XSD_INTEGER,
};

use crate::ast::*;
use crate::lexer::{tokenize, LexError};
use crate::token::{Kw, Span, StringForm, Token, TokenKind};

/// Maximum nesting depth across groups, paths, expressions, and triple
/// nodes (configurable later if a legitimate corpus needs more).
const MAX_DEPTH: u32 = 128;

/// A span-carrying parse error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseError {
    pub span: Span,
    pub message: String,
}

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} at byte {}", self.message, self.span.start)
    }
}

impl std::error::Error for ParseError {}

impl From<LexError> for ParseError {
    fn from(e: LexError) -> ParseError {
        ParseError {
            span: e.span,
            message: e.message,
        }
    }
}

/// Parse a complete SPARQL update request (possibly empty — a bare
/// prologue is a valid no-op request).
pub fn parse_update(src: &str) -> Result<UpdateRequest, ParseError> {
    let tokens = tokenize(src)?;
    let mut p = Parser::new(src, tokens);
    let u = p.update()?;
    if let Some(t) = p.peek() {
        return Err(p.err(t.span, "unexpected trailing input"));
    }
    Ok(u)
}

/// Parse a complete SPARQL query string.
pub fn parse_query(src: &str) -> Result<Query, ParseError> {
    let tokens = tokenize(src)?;
    let mut p = Parser::new(src, tokens);
    let q = p.query()?;
    if let Some(t) = p.peek() {
        return Err(p.err(t.span, "unexpected trailing input"));
    }
    Ok(q)
}

/// Recovering parse of a query (docs/10 §3.2, for the LSP diagnostics tier):
/// never fails. Lexes resiliently — unlexable runs become errors and are
/// dropped from the token stream — and the parser resynchronizes at group
/// anchors, so several broken elements report several localized errors. The
/// tree, when returned, omits the failed elements but keeps every span.
pub fn parse_query_recovering(src: &str) -> (Option<Query>, Vec<ParseError>) {
    let (tokens, mut errors) = resilient_tokens(src);
    let mut p = Parser::new(src, tokens);
    p.recovering = true;
    let result = p.query();
    let trailing = match &result {
        Ok(_) => p.peek().map(|t| p.err(t.span, "unexpected trailing input")),
        Err(_) => None,
    };
    errors.append(&mut p.errors);
    errors.extend(trailing);
    match result {
        Ok(q) => (Some(q), errors),
        Err(e) => {
            errors.push(e);
            (None, errors)
        }
    }
}

/// Recovering parse of an update request; see [`parse_query_recovering`].
/// Recovery additionally resynchronizes at top-level `;` operation
/// boundaries, so a broken operation doesn't hide the ones after it.
pub fn parse_update_recovering(src: &str) -> (Option<UpdateRequest>, Vec<ParseError>) {
    let (tokens, mut errors) = resilient_tokens(src);
    let mut p = Parser::new(src, tokens);
    p.recovering = true;
    let result = p.update();
    let trailing = match &result {
        Ok(_) => p.peek().map(|t| p.err(t.span, "unexpected trailing input")),
        Err(_) => None,
    };
    errors.append(&mut p.errors);
    errors.extend(trailing);
    match result {
        Ok(u) => (Some(u), errors),
        Err(e) => {
            errors.push(e);
            (None, errors)
        }
    }
}

/// Resilient lex for the recovering parsers: `Error` runs become
/// `ParseError`s and are filtered out of the stream the parser sees.
fn resilient_tokens(src: &str) -> (Vec<Token>, Vec<ParseError>) {
    let mut tokens = crate::lexer::tokenize_resilient(src);
    let errors = tokens
        .iter()
        .filter(|t| t.kind == TokenKind::Error)
        .map(|t| ParseError {
            span: t.span,
            message: "unrecognized input".to_string(),
        })
        .collect();
    tokens.retain(|t| t.kind != TokenKind::Error);
    (tokens, errors)
}

struct Parser<'a> {
    src: &'a str,
    tokens: Vec<Token>,
    at: usize,
    base: Option<String>,
    prefixes: HashMap<String, String>,
    /// `PREFIX` declarations in source order (for `Query::prefixes` —
    /// the resolution map above is last-wins, this preserves the shape).
    prefix_order: Vec<(String, String)>,
    fresh: u32,
    depth: u32,
    /// Current BGP scope id: bumped whenever a run of triples is
    /// interrupted, so blank-node label reuse across basic graph patterns
    /// can be rejected (a syntax-level rule with W3C tests).
    bgp_epoch: u32,
    bnode_epochs: HashMap<String, u32>,
    /// Update-request state: operation index, and which operation each
    /// template blank-node label first appeared in (reuse across
    /// operations is illegal; reuse against WHERE patterns is fine).
    update_op: u32,
    template_labels: HashMap<String, u32>,
    /// Recovering mode (docs/10 §3.2): instead of failing, the group-element
    /// and update-operation loops record the error in `errors` and
    /// resynchronize at the next anchor. Strict entry points leave this off,
    /// so their behavior is untouched.
    recovering: bool,
    errors: Vec<ParseError>,
}

impl<'a> Parser<'a> {
    fn new(src: &'a str, tokens: Vec<Token>) -> Parser<'a> {
        Parser {
            src,
            tokens,
            at: 0,
            base: None,
            prefixes: HashMap::new(),
            prefix_order: Vec::new(),
            fresh: 0,
            depth: 0,
            bgp_epoch: 0,
            bnode_epochs: HashMap::new(),
            update_op: 0,
            template_labels: HashMap::new(),
            recovering: false,
            errors: Vec::new(),
        }
    }

    // ------------------------------------------------------------ cursor

    fn peek(&self) -> Option<&Token> {
        self.tokens.get(self.at)
    }

    fn kind(&self) -> Option<TokenKind> {
        self.tokens.get(self.at).map(|t| t.kind)
    }

    fn kind_at(&self, n: usize) -> Option<TokenKind> {
        self.tokens.get(self.at + n).map(|t| t.kind)
    }

    fn bump(&mut self) -> Token {
        let t = self.tokens[self.at];
        self.at += 1;
        t
    }

    fn take(&mut self, k: TokenKind) -> Option<Token> {
        if self.kind() == Some(k) {
            Some(self.bump())
        } else {
            None
        }
    }

    fn take_kw(&mut self, kw: Kw) -> Option<Token> {
        self.take(TokenKind::Keyword(kw))
    }

    fn expect(&mut self, k: TokenKind, what: &str) -> Result<Token, ParseError> {
        self.take(k)
            .ok_or_else(|| self.err_here(format!("expected {what}")))
    }

    fn expect_kw(&mut self, kw: Kw) -> Result<Token, ParseError> {
        self.take_kw(kw)
            .ok_or_else(|| self.err_here(format!("expected {}", kw.as_str())))
    }

    fn err(&self, span: Span, message: impl Into<String>) -> ParseError {
        ParseError {
            span,
            message: message.into(),
        }
    }

    fn err_here(&self, message: impl Into<String>) -> ParseError {
        let span = self.peek().map(|t| t.span).unwrap_or(Span {
            start: self.src.len() as u32,
            end: self.src.len() as u32,
        });
        let mut message = message.into();
        match self.peek() {
            Some(t) => {
                message.push_str(&format!(", found `{}`", t.span.text(self.src)));
            }
            None => message.push_str(", found end of input"),
        }
        ParseError { span, message }
    }

    fn descend(&mut self) -> Result<(), ParseError> {
        self.depth += 1;
        if self.depth > MAX_DEPTH {
            return Err(self.err_here("query too deeply nested"));
        }
        Ok(())
    }

    fn ascend(&mut self) {
        self.depth -= 1;
    }

    fn text(&self, t: Token) -> &'a str {
        t.span.text(self.src)
    }

    // ------------------------------------------------------------ query

    fn query(&mut self) -> Result<Query, ParseError> {
        let version = self.prologue()?;
        let (form, dataset, pattern, modifiers) = match self.kind() {
            Some(TokenKind::Keyword(Kw::Select)) => {
                let select = self.select_clause()?;
                let dataset = self.dataset_clauses()?;
                let pattern = self.where_clause()?;
                let modifiers = self.solution_modifiers()?;
                self.validate_select(&select, &pattern, &modifiers, pattern.span)?;
                (QueryForm::Select(select), dataset, pattern, modifiers)
            }
            Some(TokenKind::Keyword(Kw::Construct)) => {
                self.bump();
                if self.kind() == Some(TokenKind::LBrace) {
                    let template = self.construct_template()?;
                    let dataset = self.dataset_clauses()?;
                    let pattern = self.where_clause()?;
                    let modifiers = self.solution_modifiers()?;
                    (QueryForm::Construct(template), dataset, pattern, modifiers)
                } else {
                    // Short form: CONSTRUCT WHERE { triples } — the
                    // template is the pattern itself (no paths allowed).
                    let dataset = self.dataset_clauses()?;
                    self.expect_kw(Kw::Where)?;
                    let open = self.expect(TokenKind::LBrace, "`{`")?;
                    self.next_bgp_scope();
                    let mut triples = Vec::new();
                    while self.kind() != Some(TokenKind::RBrace) {
                        self.triples_same_subject(&mut triples, false)?;
                        if self.take(TokenKind::Dot).is_none() {
                            break;
                        }
                    }
                    let close = self.expect(TokenKind::RBrace, "`}`")?;
                    let span = Span {
                        start: open.span.start,
                        end: close.span.end,
                    };
                    let pattern = GroupPattern {
                        elements: vec![GroupElement::Triples(triples.clone())],
                        span,
                    };
                    let modifiers = self.solution_modifiers()?;
                    (QueryForm::Construct(triples), dataset, pattern, modifiers)
                }
            }
            Some(TokenKind::Keyword(Kw::Describe)) => {
                self.bump();
                let mut targets = Vec::new();
                let mut star = false;
                if self.take(TokenKind::Star).is_some() {
                    star = true;
                } else {
                    loop {
                        match self.kind() {
                            Some(TokenKind::Var) => {
                                let t = self.bump();
                                targets.push(self.var_term(t));
                            }
                            Some(TokenKind::IriRef | TokenKind::PNameLn | TokenKind::PNameNs) => {
                                let term = self.iri_term()?;
                                targets.push(term);
                            }
                            _ if targets.is_empty() => {
                                return Err(self.err_here("expected `*`, a variable, or an IRI"));
                            }
                            _ => break,
                        }
                    }
                }
                let dataset = self.dataset_clauses()?;
                let pattern = if self.kind() == Some(TokenKind::Keyword(Kw::Where))
                    || self.kind() == Some(TokenKind::LBrace)
                {
                    self.where_clause()?
                } else {
                    GroupPattern::default()
                };
                let modifiers = self.solution_modifiers()?;
                (
                    QueryForm::Describe { targets, star },
                    dataset,
                    pattern,
                    modifiers,
                )
            }
            Some(TokenKind::Keyword(Kw::Ask)) => {
                self.bump();
                let dataset = self.dataset_clauses()?;
                let pattern = self.where_clause()?;
                let modifiers = self.solution_modifiers()?;
                (QueryForm::Ask, dataset, pattern, modifiers)
            }
            _ => {
                return Err(self.err_here("expected SELECT, CONSTRUCT, DESCRIBE, or ASK"));
            }
        };
        let values = if self.take_kw(Kw::Values).is_some() {
            Some(self.data_block()?)
        } else {
            None
        };
        Ok(Query {
            version,
            base: self.base.clone(),
            prefixes: self.prefix_order.clone(),
            form,
            dataset,
            pattern,
            modifiers,
            values,
        })
    }

    /// BASE / PREFIX / VERSION declarations (1.2 allows VERSION).
    fn prologue(&mut self) -> Result<Option<String>, ParseError> {
        let mut version = None;
        loop {
            match self.kind() {
                Some(TokenKind::Keyword(Kw::Base)) => {
                    self.bump();
                    let t = self.expect(TokenKind::IriRef, "an IRI after BASE")?;
                    let iri = self.resolve_iri(t)?;
                    self.base = Some(iri);
                }
                Some(TokenKind::Keyword(Kw::Prefix)) => {
                    self.bump();
                    let ns = self.expect(TokenKind::PNameNs, "a prefix name after PREFIX")?;
                    let name = self.text(ns);
                    let name = name[..name.len() - 1].to_owned(); // drop ':'
                    let t = self.expect(TokenKind::IriRef, "an IRI after the prefix name")?;
                    let iri = self.resolve_iri(t)?;
                    self.prefix_order.push((name.clone(), iri.clone()));
                    self.prefixes.insert(name, iri);
                }
                Some(TokenKind::Keyword(Kw::Version)) => {
                    self.bump();
                    let t = match self.kind() {
                        Some(TokenKind::String(f @ (StringForm::Quote | StringForm::Apos))) => {
                            let t = self.bump();
                            self.decode_string(t, f)?
                        }
                        _ => return Err(self.err_here("expected a version string")),
                    };
                    version = Some(t);
                }
                _ => return Ok(version),
            }
        }
    }

    fn select_clause(&mut self) -> Result<SelectClause, ParseError> {
        self.expect_kw(Kw::Select)?;
        let distinct = self.take_kw(Kw::Distinct).is_some();
        let reduced = !distinct && self.take_kw(Kw::Reduced).is_some();
        let mut projection = Vec::new();
        if self.take(TokenKind::Star).is_none() {
            loop {
                match self.kind() {
                    Some(TokenKind::Var) => {
                        let t = self.bump();
                        projection.push(Projection::Var(self.var_name(t)));
                    }
                    Some(TokenKind::LParen) => {
                        self.bump();
                        let expr = self.expression()?;
                        self.expect_kw(Kw::As)?;
                        let v = self.expect(TokenKind::Var, "a variable after AS")?;
                        self.expect(TokenKind::RParen, "`)`")?;
                        projection.push(Projection::Expr(expr, self.var_name(v)));
                    }
                    _ if projection.is_empty() => {
                        return Err(self.err_here("expected `*`, a variable, or `(expr AS ?var)`"));
                    }
                    _ => break,
                }
            }
        }
        Ok(SelectClause {
            distinct,
            reduced,
            projection,
        })
    }

    fn dataset_clauses(&mut self) -> Result<Vec<DatasetClause>, ParseError> {
        let mut out = Vec::new();
        while self.take_kw(Kw::From).is_some() {
            if self.take_kw(Kw::Named).is_some() {
                out.push(DatasetClause::Named(self.iri_string()?));
            } else {
                out.push(DatasetClause::Default(self.iri_string()?));
            }
        }
        Ok(out)
    }

    fn where_clause(&mut self) -> Result<GroupPattern, ParseError> {
        self.take_kw(Kw::Where);
        self.group_graph_pattern()
    }

    fn solution_modifiers(&mut self) -> Result<SolutionModifiers, ParseError> {
        let mut m = SolutionModifiers::default();
        if self.take_kw(Kw::Group).is_some() {
            self.expect_kw(Kw::By)?;
            loop {
                match self.kind() {
                    Some(TokenKind::Var) => {
                        let t = self.bump();
                        m.group_by.push(GroupCondition::Var(self.var_name(t)));
                    }
                    Some(TokenKind::LParen) => {
                        self.bump();
                        let expr = self.expression()?;
                        let alias = if self.take_kw(Kw::As).is_some() {
                            let v = self.expect(TokenKind::Var, "a variable after AS")?;
                            Some(self.var_name(v))
                        } else {
                            None
                        };
                        self.expect(TokenKind::RParen, "`)`")?;
                        m.group_by.push(GroupCondition::Expr(expr, alias));
                    }
                    Some(TokenKind::Keyword(kw)) if builtin_of(kw).is_some() => {
                        let expr = self.primary_expression()?;
                        m.group_by.push(GroupCondition::Expr(expr, None));
                    }
                    Some(TokenKind::IriRef | TokenKind::PNameLn | TokenKind::PNameNs) => {
                        let expr = self.primary_expression()?;
                        m.group_by.push(GroupCondition::Expr(expr, None));
                    }
                    _ if m.group_by.is_empty() => {
                        return Err(self.err_here("expected a GROUP BY condition"));
                    }
                    _ => break,
                }
            }
        }
        if self.take_kw(Kw::Having).is_some() {
            loop {
                match self.kind() {
                    Some(TokenKind::LParen)
                    | Some(TokenKind::IriRef | TokenKind::PNameLn | TokenKind::PNameNs) => {
                        m.having.push(self.constraint()?);
                    }
                    Some(TokenKind::Keyword(kw)) if builtin_of(kw).is_some() => {
                        m.having.push(self.constraint()?);
                    }
                    _ if m.having.is_empty() => {
                        return Err(self.err_here("expected a HAVING constraint"));
                    }
                    _ => break,
                }
            }
        }
        if self.take_kw(Kw::Order).is_some() {
            self.expect_kw(Kw::By)?;
            loop {
                let cond = match self.kind() {
                    Some(TokenKind::Keyword(Kw::Asc)) => {
                        self.bump();
                        self.expect(TokenKind::LParen, "`(`")?;
                        let e = self.expression()?;
                        self.expect(TokenKind::RParen, "`)`")?;
                        Some(OrderCondition {
                            descending: false,
                            expr: e,
                        })
                    }
                    Some(TokenKind::Keyword(Kw::Desc)) => {
                        self.bump();
                        self.expect(TokenKind::LParen, "`(`")?;
                        let e = self.expression()?;
                        self.expect(TokenKind::RParen, "`)`")?;
                        Some(OrderCondition {
                            descending: true,
                            expr: e,
                        })
                    }
                    Some(TokenKind::Var) => {
                        let t = self.bump();
                        Some(OrderCondition {
                            descending: false,
                            expr: Expr {
                                span: t.span,
                                kind: Box::new(ExprKind::Term(self.var_term(t))),
                            },
                        })
                    }
                    Some(TokenKind::LParen) => Some(OrderCondition {
                        descending: false,
                        expr: self.constraint()?,
                    }),
                    Some(TokenKind::Keyword(kw)) if builtin_of(kw).is_some() => {
                        Some(OrderCondition {
                            descending: false,
                            expr: self.constraint()?,
                        })
                    }
                    Some(TokenKind::IriRef | TokenKind::PNameLn | TokenKind::PNameNs) => {
                        Some(OrderCondition {
                            descending: false,
                            expr: self.constraint()?,
                        })
                    }
                    _ => None,
                };
                match cond {
                    Some(c) => m.order_by.push(c),
                    None if m.order_by.is_empty() => {
                        return Err(self.err_here("expected an ORDER BY condition"));
                    }
                    None => break,
                }
            }
        }
        // LIMIT and OFFSET in either order.
        loop {
            if self.take_kw(Kw::Limit).is_some() {
                if m.limit.is_some() {
                    return Err(self.err_here("duplicate LIMIT"));
                }
                m.limit = Some(self.integer_value()?);
            } else if self.take_kw(Kw::Offset).is_some() {
                if m.offset.is_some() {
                    return Err(self.err_here("duplicate OFFSET"));
                }
                m.offset = Some(self.integer_value()?);
            } else {
                break;
            }
        }
        Ok(m)
    }

    fn integer_value(&mut self) -> Result<u64, ParseError> {
        let t = self.expect(TokenKind::Integer, "an integer")?;
        self.text(t)
            .parse()
            .map_err(|_| self.err(t.span, "integer out of range"))
    }

    // ------------------------------------------------------- group patterns

    fn group_graph_pattern(&mut self) -> Result<GroupPattern, ParseError> {
        self.descend()?;
        let open = self.expect(TokenKind::LBrace, "`{`")?;
        let mut elements = Vec::new();
        if self.kind() == Some(TokenKind::Keyword(Kw::Select)) {
            let select = self.select_clause()?;
            let pattern = self.where_clause()?;
            let modifiers = self.solution_modifiers()?;
            self.validate_select(&select, &pattern, &modifiers, pattern.span)?;
            let values = if self.take_kw(Kw::Values).is_some() {
                Some(self.data_block()?)
            } else {
                None
            };
            elements.push(GroupElement::SubSelect(Box::new(SubSelect {
                select,
                pattern,
                modifiers,
                values,
            })));
        } else {
            self.group_graph_pattern_sub(&mut elements)?;
        }
        let close = self.expect(TokenKind::RBrace, "`}`")?;
        self.ascend();
        Ok(GroupPattern {
            elements,
            span: Span {
                start: open.span.start,
                end: close.span.end,
            },
        })
    }

    fn group_graph_pattern_sub(
        &mut self,
        elements: &mut Vec<GroupElement>,
    ) -> Result<(), ParseError> {
        loop {
            if matches!(self.kind(), None | Some(TokenKind::RBrace)) {
                return Ok(());
            }
            let before = self.at;
            let depth_before = self.depth;
            match self.group_element(elements) {
                Ok(()) => {}
                Err(e) if self.recovering => {
                    self.errors.push(e);
                    self.depth = depth_before;
                    self.resync_group(before);
                }
                Err(e) => return Err(e),
            }
        }
    }

    /// Recovery resync inside a group (docs/10 §3.2): guarantee ≥1 token of
    /// progress past the failure, then skip to the next anchor — a `.`
    /// (consumed) or just before `{` / `}` / a clause keyword.
    fn resync_group(&mut self, before: usize) {
        if self.at == before {
            self.at += 1;
        }
        while let Some(kind) = self.kind() {
            match kind {
                TokenKind::Dot => {
                    self.at += 1;
                    return;
                }
                TokenKind::LBrace | TokenKind::RBrace => return,
                TokenKind::Keyword(
                    Kw::Optional
                    | Kw::Minus
                    | Kw::Graph
                    | Kw::Service
                    | Kw::Filter
                    | Kw::Bind
                    | Kw::Values,
                ) => return,
                _ => self.at += 1,
            }
        }
    }

    /// One element of a group graph pattern. The caller loops (and, when
    /// recovering, resynchronizes on error).
    fn group_element(&mut self, elements: &mut Vec<GroupElement>) -> Result<(), ParseError> {
        match self.kind() {
            None | Some(TokenKind::RBrace) => return Ok(()),
            Some(TokenKind::Keyword(Kw::Optional)) => {
                self.bump();
                elements.push(GroupElement::Optional(self.group_graph_pattern()?));
                self.take(TokenKind::Dot);
            }
            Some(TokenKind::Keyword(Kw::Minus)) => {
                self.bump();
                elements.push(GroupElement::Minus(self.group_graph_pattern()?));
                self.take(TokenKind::Dot);
            }
            Some(TokenKind::Keyword(Kw::Graph)) => {
                self.bump();
                let target = self.var_or_iri()?;
                elements.push(GroupElement::Graph(target, self.group_graph_pattern()?));
                self.take(TokenKind::Dot);
            }
            Some(TokenKind::Keyword(Kw::Service)) => {
                self.bump();
                let silent = self.take_kw(Kw::Silent).is_some();
                let target = self.var_or_iri()?;
                elements.push(GroupElement::Service {
                    silent,
                    target,
                    pattern: self.group_graph_pattern()?,
                });
                self.take(TokenKind::Dot);
            }
            Some(TokenKind::Keyword(Kw::Filter)) => {
                self.bump();
                elements.push(GroupElement::Filter(self.constraint()?));
                self.take(TokenKind::Dot);
            }
            Some(TokenKind::Keyword(Kw::Bind)) => {
                let kw = self.bump();
                self.expect(TokenKind::LParen, "`(`")?;
                let expr = self.expression()?;
                self.expect_kw(Kw::As)?;
                let v = self.expect(TokenKind::Var, "a variable after AS")?;
                let close = self.expect(TokenKind::RParen, "`)`")?;
                // §19.8: the BIND target must not already be in scope
                // in this group (everything before the BIND counts).
                let so_far = GroupPattern {
                    elements: std::mem::take(elements),
                    span: kw.span,
                };
                let mut scope = std::collections::HashSet::new();
                pattern_vars(&so_far, &mut scope);
                *elements = so_far.elements;
                let name = self.var_name(v);
                if scope.contains(&name) {
                    return Err(
                        self.err(v.span, format!("?{name} is already in scope at this BIND"))
                    );
                }
                elements.push(GroupElement::Bind {
                    expr,
                    var: name,
                    span: Span {
                        start: kw.span.start,
                        end: close.span.end,
                    },
                });
                self.take(TokenKind::Dot);
            }
            Some(TokenKind::Keyword(Kw::Values)) => {
                self.bump();
                elements.push(GroupElement::Values(self.data_block()?));
                self.take(TokenKind::Dot);
            }
            Some(TokenKind::LBrace) => {
                // GroupOrUnionGraphPattern.
                let mut branches = vec![self.group_graph_pattern()?];
                while self.take_kw(Kw::Union).is_some() {
                    branches.push(self.group_graph_pattern()?);
                }
                elements.push(GroupElement::Union(branches));
                self.take(TokenKind::Dot);
            }
            _ => {
                // A run of triples (one BGP scope).
                // FILTERs are translated over the surrounding BGP and do
                // not end its blank-node-label scope. Other intervening
                // group elements do.
                if !matches!(elements.last(), Some(GroupElement::Filter(_))) {
                    self.next_bgp_scope();
                }
                let mut triples = Vec::new();
                loop {
                    self.triples_same_subject(&mut triples, true)?;
                    if self.take(TokenKind::Dot).is_none() {
                        if self.at_triples_start() {
                            return Err(self.err_here("expected `.` between triple patterns"));
                        }
                        break;
                    }
                    if !self.at_triples_start() {
                        break;
                    }
                }
                if triples.is_empty() {
                    return Err(self.err_here("expected a graph pattern"));
                }
                elements.push(GroupElement::Triples(triples));
            }
        }
        Ok(())
    }

    fn at_triples_start(&self) -> bool {
        matches!(
            self.kind(),
            Some(
                TokenKind::Var
                    | TokenKind::IriRef
                    | TokenKind::PNameLn
                    | TokenKind::PNameNs
                    | TokenKind::BlankNode
                    | TokenKind::Anon
                    | TokenKind::Nil
                    | TokenKind::LParen
                    | TokenKind::LBracket
                    | TokenKind::String(_)
                    | TokenKind::Integer
                    | TokenKind::Decimal
                    | TokenKind::Double
                    | TokenKind::True
                    | TokenKind::False
                    | TokenKind::LtLt
                    | TokenKind::LtLtParen
            )
        )
    }

    fn next_bgp_scope(&mut self) {
        self.bgp_epoch += 1;
    }

    // ------------------------------------------------------------ triples

    /// TriplesSameSubjectPath (with paths) or TriplesSameSubject
    /// (templates; `paths=false` also forbids blank-node label reuse
    /// relaxations — templates are their own scope).
    fn triples_same_subject(
        &mut self,
        out: &mut Vec<TriplePattern>,
        paths: bool,
    ) -> Result<(), ParseError> {
        self.descend()?;
        let r = self.triples_same_subject_inner(out, paths);
        self.ascend();
        r
    }

    fn triples_same_subject_inner(
        &mut self,
        out: &mut Vec<TriplePattern>,
        paths: bool,
    ) -> Result<(), ParseError> {
        match self.kind() {
            Some(TokenKind::LParen) => {
                // Collection as subject: expands to triples; the property
                // list is optional because a non-empty collection already
                // contributes its rdf:first/rdf:rest triples.
                let s = self.collection(out, paths)?;
                self.property_list(s, out, paths, true)?;
                Ok(())
            }
            Some(TokenKind::LBracket) => {
                let s = self.blank_node_property_list(out, paths)?;
                // Property list optional after a bracketed node.
                self.property_list(s, out, paths, true)?;
                Ok(())
            }
            Some(TokenKind::LtLt) => {
                let s = self.reified_triple(out)?;
                self.property_list(s, out, paths, true)?;
                Ok(())
            }
            _ => {
                let s = self.var_or_term(true, true, out)?;
                self.property_list(s, out, paths, false)?;
                Ok(())
            }
        }
    }

    /// PropertyListPath(NotEmpty): verb + object list, `;`-separated.
    fn property_list(
        &mut self,
        subject: Term,
        out: &mut Vec<TriplePattern>,
        paths: bool,
        optional: bool,
    ) -> Result<(), ParseError> {
        let mut first = true;
        loop {
            let verb = match self.kind() {
                Some(TokenKind::Var) => {
                    let t = self.bump();
                    Some(Verb::Term(self.var_term(t)))
                }
                // `a` must go through the path grammar in path position:
                // it is a PathPrimary, so `a/ex:b`, `a?`, `a|ex:b` continue
                // (path_verb collapses a trivial path back to a term verb).
                Some(
                    TokenKind::A
                    | TokenKind::IriRef
                    | TokenKind::PNameLn
                    | TokenKind::PNameNs
                    | TokenKind::Caret
                    | TokenKind::Bang
                    | TokenKind::LParen,
                ) if paths => Some(self.path_verb()?),
                Some(TokenKind::A) => {
                    let t = self.bump();
                    Some(Verb::Term(Term {
                        kind: TermKind::Iri(RDF_TYPE.to_owned()),
                        span: t.span,
                    }))
                }
                Some(TokenKind::IriRef | TokenKind::PNameLn | TokenKind::PNameNs) => {
                    Some(Verb::Term(self.iri_term()?))
                }
                _ => None,
            };
            let Some(verb) = verb else {
                if first && !optional {
                    return Err(self.err_here("expected a predicate"));
                }
                return Ok(());
            };
            first = false;
            // Object list.
            loop {
                self.object(subject.clone(), verb.clone(), out, paths)?;
                if self.take(TokenKind::Comma).is_none() {
                    break;
                }
            }
            if self.take(TokenKind::Semicolon).is_none() {
                return Ok(());
            }
            // Empty `;` segments are legal.
            while self.take(TokenKind::Semicolon).is_some() {}
        }
    }

    /// One object; emits the triple and any 1.2 annotations.
    fn object(
        &mut self,
        s: Term,
        p: Verb,
        out: &mut Vec<TriplePattern>,
        paths: bool,
    ) -> Result<(), ParseError> {
        self.descend()?;
        let o = match self.kind() {
            Some(TokenKind::LParen) => self.collection(out, paths)?,
            Some(TokenKind::LBracket) => self.blank_node_property_list(out, paths)?,
            Some(TokenKind::LtLt) => self.reified_triple(out)?,
            _ => self.var_or_term(true, true, out)?,
        };
        out.push(TriplePattern {
            s: s.clone(),
            p: p.clone(),
            o: o.clone(),
        });
        // Annotations: `~reifier` and `{| … |}` blocks (SPARQL 1.2),
        // desugared like Turtle 1.2: each mints/uses a reifier `r` with
        // `r rdf:reifies <<(s p o)>>`, and a block parses with `r` as
        // subject.
        let mut current: Option<Term> = None;
        loop {
            match self.kind() {
                Some(TokenKind::Tilde) => {
                    let t = self.bump();
                    let p_term = match &p {
                        Verb::Term(t) => t.clone(),
                        Verb::Path(_) => {
                            return Err(self.err(t.span, "cannot reify a property-path triple"));
                        }
                    };
                    let r = if self.at_reifier_term() {
                        self.var_or_term(false, false, out)?
                    } else {
                        self.fresh_bnode(t.span)
                    };
                    out.push(self.reifies(r.clone(), s.clone(), p_term, o.clone(), t.span));
                    current = Some(r);
                }
                Some(TokenKind::LBraceBar) => {
                    let open = self.bump();
                    let r = match current.take() {
                        Some(r) => r,
                        None => {
                            let p_term = match &p {
                                Verb::Term(t) => t.clone(),
                                Verb::Path(_) => {
                                    return Err(self
                                        .err(open.span, "cannot annotate a property-path triple"));
                                }
                            };
                            let r = self.fresh_bnode(open.span);
                            out.push(self.reifies(
                                r.clone(),
                                s.clone(),
                                p_term,
                                o.clone(),
                                open.span,
                            ));
                            r
                        }
                    };
                    self.property_list(r, out, paths, false)?;
                    self.expect(TokenKind::RBarBrace, "`|}`")?;
                }
                _ => break,
            }
        }
        self.ascend();
        Ok(())
    }

    fn at_reifier_term(&self) -> bool {
        matches!(
            self.kind(),
            Some(
                TokenKind::Var
                    | TokenKind::IriRef
                    | TokenKind::PNameLn
                    | TokenKind::PNameNs
                    | TokenKind::BlankNode
            )
        )
    }

    fn reifies(&mut self, r: Term, s: Term, p: Term, o: Term, span: Span) -> TriplePattern {
        TriplePattern {
            s: r,
            p: Verb::Term(Term {
                kind: TermKind::Iri(RDF_REIFIES.to_owned()),
                span,
            }),
            o: Term {
                kind: TermKind::TripleTerm(Box::new(TriplePattern {
                    s,
                    p: Verb::Term(p),
                    o,
                })),
                span,
            },
        }
    }

    /// `<< s p o (~ reifier)? >>` — evaluates to the reifier term.
    fn reified_triple(&mut self, out: &mut Vec<TriplePattern>) -> Result<Term, ParseError> {
        self.descend()?;
        let open = self.expect(TokenKind::LtLt, "`<<`")?;
        let s = match self.kind() {
            Some(TokenKind::LtLt) => self.reified_triple(out)?,
            _ => self.var_or_term(true, false, out)?,
        };
        let p = match self.kind() {
            Some(TokenKind::A) => {
                let t = self.bump();
                Term {
                    kind: TermKind::Iri(RDF_TYPE.to_owned()),
                    span: t.span,
                }
            }
            Some(TokenKind::Var) => {
                let t = self.bump();
                self.var_term(t)
            }
            _ => self.iri_term()?,
        };
        let o = match self.kind() {
            Some(TokenKind::LtLt) => self.reified_triple(out)?,
            Some(TokenKind::LtLtParen) => self.triple_term(out)?,
            _ => self.var_or_term(false, false, out)?,
        };
        let r = if self.take(TokenKind::Tilde).is_some() {
            if self.at_reifier_term() {
                self.var_or_term(false, false, out)?
            } else {
                self.fresh_bnode(open.span)
            }
        } else {
            self.fresh_bnode(open.span)
        };
        self.expect(TokenKind::GtGt, "`>>`")?;
        out.push(self.reifies(r.clone(), s, p, o, open.span));
        self.ascend();
        Ok(r)
    }

    /// `<<( s p o )>>` — a triple term (object positions only).
    fn triple_term(&mut self, out: &mut Vec<TriplePattern>) -> Result<Term, ParseError> {
        self.descend()?;
        let open = self.expect(TokenKind::LtLtParen, "`<<(`")?;
        let s = self.var_or_term(true, false, out)?;
        let p = match self.kind() {
            Some(TokenKind::A) => {
                let t = self.bump();
                Term {
                    kind: TermKind::Iri(RDF_TYPE.to_owned()),
                    span: t.span,
                }
            }
            Some(TokenKind::Var) => {
                let t = self.bump();
                self.var_term(t)
            }
            _ => self.iri_term()?,
        };
        let o = match self.kind() {
            Some(TokenKind::LtLtParen) => self.triple_term(out)?,
            _ => self.var_or_term(false, false, out)?,
        };
        let close = self.expect(TokenKind::RParenGtGt, "`)>>`")?;
        self.ascend();
        Ok(Term {
            kind: TermKind::TripleTerm(Box::new(TriplePattern {
                s,
                p: Verb::Term(p),
                o,
            })),
            span: Span {
                start: open.span.start,
                end: close.span.end,
            },
        })
    }

    /// `( item… )` — expands to an rdf:first/rest chain, returns the head.
    fn collection(
        &mut self,
        out: &mut Vec<TriplePattern>,
        paths: bool,
    ) -> Result<Term, ParseError> {
        self.descend()?;
        let open = self.expect(TokenKind::LParen, "`(`")?;
        let mut items = Vec::new();
        while self.kind() != Some(TokenKind::RParen) {
            if self.kind().is_none() {
                return Err(self.err_here("unclosed collection"));
            }
            let item = match self.kind() {
                Some(TokenKind::LParen) => self.collection(out, paths)?,
                Some(TokenKind::LBracket) => self.blank_node_property_list(out, paths)?,
                Some(TokenKind::LtLt) => self.reified_triple(out)?,
                _ => self.var_or_term(true, true, out)?,
            };
            items.push(item);
        }
        let close = self.bump(); // RParen
        let span = Span {
            start: open.span.start,
            end: close.span.end,
        };
        // An empty collection is rdf:nil (the lexer emits NIL for `()`
        // with no items, but a spaced `( )` with items=0 lands here too).
        let mut tail = Term {
            kind: TermKind::Iri(RDF_NIL.to_owned()),
            span,
        };
        for item in items.into_iter().rev() {
            let node = self.fresh_bnode(span);
            out.push(TriplePattern {
                s: node.clone(),
                p: Verb::Term(Term {
                    kind: TermKind::Iri(RDF_FIRST.to_owned()),
                    span,
                }),
                o: item,
            });
            out.push(TriplePattern {
                s: node.clone(),
                p: Verb::Term(Term {
                    kind: TermKind::Iri(RDF_REST.to_owned()),
                    span,
                }),
                o: tail,
            });
            tail = node;
        }
        self.ascend();
        Ok(tail)
    }

    /// `[ property list ]` — a fresh blank node with its properties.
    fn blank_node_property_list(
        &mut self,
        out: &mut Vec<TriplePattern>,
        paths: bool,
    ) -> Result<Term, ParseError> {
        self.descend()?;
        let open = self.expect(TokenKind::LBracket, "`[`")?;
        let node = self.fresh_bnode(open.span);
        self.property_list(node.clone(), out, paths, false)?;
        self.expect(TokenKind::RBracket, "`]`")?;
        self.ascend();
        Ok(node)
    }

    // ------------------------------------------------------------ terms

    fn fresh_bnode(&mut self, span: Span) -> Term {
        let label = format!(".b{}", self.fresh);
        self.fresh += 1;
        Term {
            kind: TermKind::BlankNode(label),
            span,
        }
    }

    fn var_name(&self, t: Token) -> String {
        self.text(t)[1..].to_owned()
    }

    fn var_term(&self, t: Token) -> Term {
        Term {
            kind: TermKind::Var(self.var_name(t)),
            span: t.span,
        }
    }

    fn var_or_iri(&mut self) -> Result<Term, ParseError> {
        match self.kind() {
            Some(TokenKind::Var) => {
                let t = self.bump();
                Ok(self.var_term(t))
            }
            Some(TokenKind::IriRef | TokenKind::PNameLn | TokenKind::PNameNs) => self.iri_term(),
            _ => Err(self.err_here("expected a variable or an IRI")),
        }
    }

    /// GraphNode(Path) minus the bracketed forms: var, IRI, literal,
    /// labeled blank node, NIL, or (object positions only) a triple term.
    /// Reified-triple and triple-term components exclude NIL per the 1.2
    /// grammar (`allow_nil = false`).
    fn var_or_term(
        &mut self,
        allow_triple_term: bool,
        allow_nil: bool,
        out: &mut Vec<TriplePattern>,
    ) -> Result<Term, ParseError> {
        match self.kind() {
            Some(TokenKind::Var) => {
                let t = self.bump();
                Ok(self.var_term(t))
            }
            Some(TokenKind::IriRef | TokenKind::PNameLn | TokenKind::PNameNs) => self.iri_term(),
            Some(TokenKind::BlankNode) => {
                let t = self.bump();
                let label = self.text(t)[2..].to_owned();
                match self.bnode_epochs.get(&label) {
                    Some(&e) if e != self.bgp_epoch => {
                        return Err(self.err(
                            t.span,
                            format!(
                                "blank node label `_:{label}` reused across basic graph patterns"
                            ),
                        ));
                    }
                    _ => {
                        self.bnode_epochs.insert(label.clone(), self.bgp_epoch);
                    }
                }
                Ok(Term {
                    kind: TermKind::BlankNode(label),
                    span: t.span,
                })
            }
            Some(TokenKind::Anon) => {
                let t = self.bump();
                Ok(self.fresh_bnode(t.span))
            }
            Some(TokenKind::Nil) if allow_nil => {
                let t = self.bump();
                Ok(Term {
                    kind: TermKind::Iri(RDF_NIL.to_owned()),
                    span: t.span,
                })
            }
            Some(TokenKind::LtLtParen) if allow_triple_term => self.triple_term(out),
            Some(
                TokenKind::String(_)
                | TokenKind::Integer
                | TokenKind::Decimal
                | TokenKind::Double
                | TokenKind::True
                | TokenKind::False,
            ) => self.literal(),
            _ => Err(self.err_here("expected an RDF term or variable")),
        }
    }

    fn literal(&mut self) -> Result<Term, ParseError> {
        let t = self.bump();
        match t.kind {
            TokenKind::String(form) => {
                let lexical = self.decode_string(t, form)?;
                match self.kind() {
                    Some(TokenKind::LangTag(dir)) => {
                        let lt = self.bump();
                        let text = self.text(lt);
                        let tag = match dir {
                            Some(_) => {
                                let base = &text[1..];
                                let cut = base.rfind("--").expect("directional tag has --");
                                base[..cut].to_owned()
                            }
                            None => text[1..].to_owned(),
                        };
                        Ok(Term {
                            kind: TermKind::Literal {
                                lexical,
                                kind: LiteralKind::Lang { tag, dir },
                            },
                            span: Span {
                                start: t.span.start,
                                end: lt.span.end,
                            },
                        })
                    }
                    Some(TokenKind::CaretCaret) => {
                        self.bump();
                        let dt = self.iri_string()?;
                        let end = self.tokens[self.at - 1].span.end;
                        Ok(Term {
                            kind: TermKind::Literal {
                                lexical,
                                kind: LiteralKind::Typed(dt),
                            },
                            span: Span {
                                start: t.span.start,
                                end,
                            },
                        })
                    }
                    _ => Ok(Term {
                        kind: TermKind::Literal {
                            lexical,
                            kind: LiteralKind::Plain,
                        },
                        span: t.span,
                    }),
                }
            }
            TokenKind::Integer => Ok(self.numeric(t, XSD_INTEGER)),
            TokenKind::Decimal => Ok(self.numeric(t, XSD_DECIMAL)),
            TokenKind::Double => Ok(self.numeric(t, XSD_DOUBLE)),
            TokenKind::True | TokenKind::False => Ok(Term {
                kind: TermKind::Literal {
                    lexical: if t.kind == TokenKind::True {
                        "true".to_owned()
                    } else {
                        "false".to_owned()
                    },
                    kind: LiteralKind::Typed(XSD_BOOLEAN.to_owned()),
                },
                span: t.span,
            }),
            _ => Err(self.err(t.span, "expected a literal")),
        }
    }

    fn numeric(&self, t: Token, dt: &str) -> Term {
        Term {
            kind: TermKind::Literal {
                lexical: self.text(t).to_owned(),
                kind: LiteralKind::Typed(dt.to_owned()),
            },
            span: t.span,
        }
    }

    fn iri_term(&mut self) -> Result<Term, ParseError> {
        let start = self
            .peek()
            .map(|t| t.span)
            .unwrap_or(Span { start: 0, end: 0 });
        let iri = self.iri_string()?;
        let end = self.tokens[self.at - 1].span.end;
        Ok(Term {
            kind: TermKind::Iri(iri),
            span: Span {
                start: start.start,
                end,
            },
        })
    }

    /// An IRI (IRIREF or prefixed name) as an absolute string.
    fn iri_string(&mut self) -> Result<String, ParseError> {
        match self.kind() {
            Some(TokenKind::IriRef) => {
                let t = self.bump();
                self.resolve_iri(t)
            }
            Some(TokenKind::PNameLn | TokenKind::PNameNs) => {
                let t = self.bump();
                self.expand_pname(t)
            }
            _ => Err(self.err_here("expected an IRI")),
        }
    }

    fn resolve_iri(&self, t: Token) -> Result<String, ParseError> {
        let text = self.text(t);
        let reference = &text[1..text.len() - 1];
        let reference = self.decode_uchar(reference, t.span)?;
        // Absolute IRIs are RDF terms as written. RFC resolution (including
        // dot-segment removal) applies only to relative references; applying
        // it to an absolute IRI would silently change term identity.
        if graphy_core::iri::parse_reference(&reference)
            .map_err(|e| self.err(t.span, format!("invalid IRI: {e}")))?
            .scheme
            .is_some()
        {
            return Ok(reference);
        }
        match &self.base {
            Some(base) => graphy_core::iri::resolve(base, &reference)
                .map_err(|e| self.err(t.span, format!("cannot resolve IRI: {e}"))),
            None => {
                graphy_core::iri::validate_reference(&reference)
                    .map_err(|e| self.err(t.span, format!("invalid IRI: {e}")))?;
                Ok(reference)
            }
        }
    }

    fn decode_uchar(&self, text: &str, span: Span) -> Result<String, ParseError> {
        if !text.contains('\\') {
            return Ok(text.to_owned());
        }
        let bytes = text.as_bytes();
        let mut out = String::with_capacity(text.len());
        let mut at = 0usize;
        while at < bytes.len() {
            if bytes[at] != b'\\' {
                let c = text[at..].chars().next().expect("UTF-8 boundary");
                out.push(c);
                at += c.len_utf8();
                continue;
            }
            let n = match bytes.get(at + 1) {
                Some(b'u') => 4,
                Some(b'U') => 8,
                _ => return Err(self.err(span, "invalid escape in IRI")),
            };
            let hex = bytes
                .get(at + 2..at + 2 + n)
                .ok_or_else(|| self.err(span, "truncated escape in IRI"))?;
            let value = u32::from_str_radix(
                std::str::from_utf8(hex).expect("lexer checked ASCII hex"),
                16,
            )
            .map_err(|_| self.err(span, "invalid escape in IRI"))?;
            out.push(
                char::from_u32(value)
                    .ok_or_else(|| self.err(span, "invalid Unicode scalar in IRI"))?,
            );
            at += n + 2;
        }
        Ok(out)
    }

    fn expand_pname(&self, t: Token) -> Result<String, ParseError> {
        let text = self.text(t);
        let colon = text.find(':').expect("pname has a colon");
        let (prefix, local) = (&text[..colon], &text[colon + 1..]);
        let Some(ns) = self.prefixes.get(prefix) else {
            return Err(self.err(t.span, format!("undeclared prefix `{prefix}:`")));
        };
        // Local-name escapes drop the backslash; %XX stays literal.
        let mut iri = ns.clone();
        let mut chars = local.chars();
        while let Some(c) = chars.next() {
            if c == '\\' {
                if let Some(e) = chars.next() {
                    iri.push(e);
                }
            } else {
                iri.push(c);
            }
        }
        Ok(iri)
    }

    /// Decode a string token's body (delimiters stripped, escapes applied).
    fn decode_string(&self, t: Token, form: StringForm) -> Result<String, ParseError> {
        let text = self.text(t);
        let d = form.delim() as usize;
        let body = &text[d..text.len() - d];
        let mut out = String::with_capacity(body.len());
        let mut chars = body.char_indices();
        while let Some((i, c)) = chars.next() {
            if c != '\\' {
                out.push(c);
                continue;
            }
            let (_, e) = chars.next().expect("lexer validated escapes");
            match e {
                't' => out.push('\t'),
                'b' => out.push('\u{8}'),
                'n' => out.push('\n'),
                'r' => out.push('\r'),
                'f' => out.push('\u{C}'),
                '"' => out.push('"'),
                '\'' => out.push('\''),
                '\\' => out.push('\\'),
                'u' | 'U' => {
                    let n = if e == 'u' { 4 } else { 8 };
                    let hs = i + 2;
                    let hex = &body[hs..hs + n];
                    let v = u32::from_str_radix(hex, 16).expect("lexer validated hex");
                    out.push(char::from_u32(v).expect("lexer validated scalar"));
                    for _ in 0..n {
                        chars.next();
                    }
                }
                other => {
                    return Err(self.err(t.span, format!("invalid escape `\\{other}`")));
                }
            }
        }
        Ok(out)
    }

    // ------------------------------------------------------------ paths

    /// A property path in verb position; a trivial single-IRI path
    /// collapses to a plain term.
    fn path_verb(&mut self) -> Result<Verb, ParseError> {
        let start = self.peek().map(|t| t.span);
        let path = self.path_alternative()?;
        Ok(match path {
            Path::Iri(iri) => Verb::Term(Term {
                kind: TermKind::Iri(iri),
                span: start.unwrap_or(Span { start: 0, end: 0 }),
            }),
            p => Verb::Path(p),
        })
    }

    fn path_alternative(&mut self) -> Result<Path, ParseError> {
        self.descend()?;
        let mut left = self.path_sequence()?;
        while self.take(TokenKind::Pipe).is_some() {
            let right = self.path_sequence()?;
            left = Path::Alt(Box::new(left), Box::new(right));
        }
        self.ascend();
        Ok(left)
    }

    fn path_sequence(&mut self) -> Result<Path, ParseError> {
        let mut left = self.path_elt_or_inverse()?;
        while self.take(TokenKind::Slash).is_some() {
            let right = self.path_elt_or_inverse()?;
            left = Path::Seq(Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    fn path_elt_or_inverse(&mut self) -> Result<Path, ParseError> {
        if self.take(TokenKind::Caret).is_some() {
            Ok(Path::Inverse(Box::new(self.path_elt()?)))
        } else {
            self.path_elt()
        }
    }

    fn path_elt(&mut self) -> Result<Path, ParseError> {
        let primary = self.path_primary()?;
        Ok(match self.kind() {
            Some(TokenKind::Star) => {
                self.bump();
                Path::ZeroOrMore(Box::new(primary))
            }
            Some(TokenKind::Plus) => {
                self.bump();
                Path::OneOrMore(Box::new(primary))
            }
            Some(TokenKind::Question) => {
                self.bump();
                Path::ZeroOrOne(Box::new(primary))
            }
            _ => primary,
        })
    }

    fn path_primary(&mut self) -> Result<Path, ParseError> {
        self.descend()?;
        let path = match self.kind() {
            Some(TokenKind::A) => {
                self.bump();
                Path::Iri(RDF_TYPE.to_owned())
            }
            Some(TokenKind::IriRef | TokenKind::PNameLn | TokenKind::PNameNs) => {
                Path::Iri(self.iri_string()?)
            }
            Some(TokenKind::Bang) => {
                self.bump();
                Path::Nps(self.negated_property_set()?)
            }
            Some(TokenKind::LParen) => {
                self.bump();
                let inner = self.path_alternative()?;
                self.expect(TokenKind::RParen, "`)`")?;
                inner
            }
            _ => return Err(self.err_here("expected a property path")),
        };
        self.ascend();
        Ok(path)
    }

    fn negated_property_set(&mut self) -> Result<Vec<(String, bool)>, ParseError> {
        let mut items = Vec::new();
        if self.take(TokenKind::LParen).is_some() {
            if self.take(TokenKind::RParen).is_some() {
                return Ok(items);
            }
            loop {
                items.push(self.path_one_in_property_set()?);
                if self.take(TokenKind::Pipe).is_none() {
                    break;
                }
            }
            self.expect(TokenKind::RParen, "`)`")?;
        } else {
            items.push(self.path_one_in_property_set()?);
        }
        Ok(items)
    }

    fn path_one_in_property_set(&mut self) -> Result<(String, bool), ParseError> {
        let inverse = self.take(TokenKind::Caret).is_some();
        let iri = match self.kind() {
            Some(TokenKind::A) => {
                self.bump();
                RDF_TYPE.to_owned()
            }
            _ => self.iri_string()?,
        };
        Ok((iri, inverse))
    }

    // ------------------------------------------------------------ values

    fn data_block(&mut self) -> Result<ValuesBlock, ParseError> {
        let mut vars = Vec::new();
        let mut rows = Vec::new();
        match self.kind() {
            Some(TokenKind::Var) => {
                // One-var form: VALUES ?x { term… }
                let t = self.bump();
                vars.push(self.var_name(t));
                self.expect(TokenKind::LBrace, "`{`")?;
                while self.kind() != Some(TokenKind::RBrace) {
                    rows.push(vec![self.data_value()?]);
                }
                self.bump();
            }
            Some(TokenKind::LParen | TokenKind::Nil) => {
                if self.take(TokenKind::Nil).is_none() {
                    self.bump(); // LParen
                    while self.kind() == Some(TokenKind::Var) {
                        let t = self.bump();
                        let name = self.var_name(t);
                        if vars.contains(&name) {
                            return Err(
                                self.err(t.span, format!("duplicate VALUES variable ?{name}"))
                            );
                        }
                        vars.push(name);
                    }
                    self.expect(TokenKind::RParen, "`)`")?;
                }
                self.expect(TokenKind::LBrace, "`{`")?;
                loop {
                    match self.kind() {
                        Some(TokenKind::RBrace) => {
                            self.bump();
                            break;
                        }
                        Some(TokenKind::Nil) => {
                            self.bump();
                            if !vars.is_empty() {
                                return Err(
                                    self.err_here("row arity does not match VALUES variables")
                                );
                            }
                            rows.push(Vec::new());
                        }
                        Some(TokenKind::LParen) => {
                            let open = self.bump();
                            let mut row = Vec::new();
                            while self.kind() != Some(TokenKind::RParen) {
                                row.push(self.data_value()?);
                            }
                            self.bump();
                            if row.len() != vars.len() {
                                return Err(self
                                    .err(open.span, "row arity does not match VALUES variables"));
                            }
                            rows.push(row);
                        }
                        _ => return Err(self.err_here("expected a VALUES row or `}`")),
                    }
                }
            }
            _ => return Err(self.err_here("expected VALUES variables")),
        }
        Ok(ValuesBlock { vars, rows })
    }

    /// A ground data value: IRI, literal, triple term, or UNDEF.
    fn data_value(&mut self) -> Result<Option<Term>, ParseError> {
        match self.kind() {
            Some(TokenKind::Keyword(Kw::Undef)) => {
                self.bump();
                Ok(None)
            }
            Some(TokenKind::IriRef | TokenKind::PNameLn | TokenKind::PNameNs) => {
                Ok(Some(self.iri_term()?))
            }
            Some(
                TokenKind::String(_)
                | TokenKind::Integer
                | TokenKind::Decimal
                | TokenKind::Double
                | TokenKind::True
                | TokenKind::False,
            ) => Ok(Some(self.literal()?)),
            Some(TokenKind::LtLtParen) => {
                let mut side = Vec::new();
                let t = self.triple_term(&mut side)?;
                if !side.is_empty() {
                    return Err(self.err(t.span, "VALUES triple terms must be ground"));
                }
                self.validate_data_tt(&t)?;
                Ok(Some(t))
            }
            _ => Err(self.err_here("expected an RDF term or UNDEF")),
        }
    }

    // ------------------------------------------------------- construct

    fn construct_template(&mut self) -> Result<Vec<TriplePattern>, ParseError> {
        self.expect(TokenKind::LBrace, "`{`")?;
        self.next_bgp_scope();
        let mut triples = Vec::new();
        while self.kind() != Some(TokenKind::RBrace) {
            self.triples_same_subject(&mut triples, false)?;
            if self.take(TokenKind::Dot).is_none() {
                break;
            }
        }
        self.expect(TokenKind::RBrace, "`}`")?;
        Ok(triples)
    }

    // ------------------------------------------------------- expressions

    /// A bracketed expression or a call (FILTER/HAVING/ORDER constraint).
    fn constraint(&mut self) -> Result<Expr, ParseError> {
        match self.kind() {
            Some(TokenKind::LParen) => {
                self.bump();
                let e = self.expression()?;
                self.expect(TokenKind::RParen, "`)`")?;
                Ok(e)
            }
            Some(TokenKind::Keyword(kw))
                if builtin_of(kw).is_some() || matches!(kw, Kw::Exists | Kw::Not) =>
            {
                self.primary_expression()
            }
            Some(TokenKind::IriRef | TokenKind::PNameLn | TokenKind::PNameNs)
                if matches!(self.kind_at(1), Some(TokenKind::LParen | TokenKind::Nil)) =>
            {
                self.primary_expression()
            }
            _ => {
                Err(self.err_here("FILTER constraint must be parenthesized or be a function call"))
            }
        }
    }

    fn expression(&mut self) -> Result<Expr, ParseError> {
        self.descend()?;
        let e = self.or_expression()?;
        self.ascend();
        Ok(e)
    }

    fn spanned(&self, start: Span, kind: ExprKind) -> Expr {
        let end = self.tokens[self.at - 1].span.end;
        Expr {
            span: Span {
                start: start.start,
                end,
            },
            kind: Box::new(kind),
        }
    }

    fn or_expression(&mut self) -> Result<Expr, ParseError> {
        let start = self.peek().map(|t| t.span).unwrap_or_default2();
        let mut left = self.and_expression()?;
        while self.take(TokenKind::OrOr).is_some() {
            let right = self.and_expression()?;
            left = self.spanned(start, ExprKind::Or(left, right));
        }
        Ok(left)
    }

    fn and_expression(&mut self) -> Result<Expr, ParseError> {
        let start = self.peek().map(|t| t.span).unwrap_or_default2();
        let mut left = self.relational_expression()?;
        while self.take(TokenKind::AndAnd).is_some() {
            let right = self.relational_expression()?;
            left = self.spanned(start, ExprKind::And(left, right));
        }
        Ok(left)
    }

    fn relational_expression(&mut self) -> Result<Expr, ParseError> {
        let start = self.peek().map(|t| t.span).unwrap_or_default2();
        let left = self.additive_expression()?;
        let op = match self.kind() {
            Some(TokenKind::Eq) => Some(CmpOp::Eq),
            Some(TokenKind::Ne) => Some(CmpOp::Ne),
            Some(TokenKind::Lt) => Some(CmpOp::Lt),
            Some(TokenKind::Le) => Some(CmpOp::Le),
            Some(TokenKind::Gt) => Some(CmpOp::Gt),
            Some(TokenKind::Ge) => Some(CmpOp::Ge),
            _ => None,
        };
        if let Some(op) = op {
            self.bump();
            let right = self.additive_expression()?;
            return Ok(self.spanned(start, ExprKind::Cmp(op, left, right)));
        }
        // [NOT] IN ( … )
        let negated = if self.kind() == Some(TokenKind::Keyword(Kw::Not))
            && self.kind_at(1) == Some(TokenKind::Keyword(Kw::In))
        {
            self.bump();
            self.bump();
            Some(true)
        } else if self.take_kw(Kw::In).is_some() {
            Some(false)
        } else {
            None
        };
        if let Some(negated) = negated {
            let list = self.expression_list()?;
            return Ok(self.spanned(
                start,
                ExprKind::In {
                    expr: left,
                    list,
                    negated,
                },
            ));
        }
        Ok(left)
    }

    fn expression_list(&mut self) -> Result<Vec<Expr>, ParseError> {
        if self.take(TokenKind::Nil).is_some() {
            return Ok(Vec::new());
        }
        self.expect(TokenKind::LParen, "`(`")?;
        let mut out = vec![self.expression()?];
        while self.take(TokenKind::Comma).is_some() {
            out.push(self.expression()?);
        }
        self.expect(TokenKind::RParen, "`)`")?;
        Ok(out)
    }

    fn additive_expression(&mut self) -> Result<Expr, ParseError> {
        let start = self.peek().map(|t| t.span).unwrap_or_default2();
        let mut left = self.multiplicative_expression()?;
        loop {
            match self.kind() {
                Some(TokenKind::Plus) => {
                    self.bump();
                    let right = self.multiplicative_expression()?;
                    left = self.spanned(start, ExprKind::Add(left, right));
                }
                Some(TokenKind::Minus) => {
                    self.bump();
                    let right = self.multiplicative_expression()?;
                    left = self.spanned(start, ExprKind::Sub(left, right));
                }
                // The grammar's signed-numeric continuation: `?x+1` lexes
                // the `+1` into the literal, which acts as `+ 1` here
                // (and may carry on multiplicatively: `?x+1*2`).
                Some(TokenKind::Integer | TokenKind::Decimal | TokenKind::Double)
                    if self.peek().is_some_and(|t| {
                        matches!(t.span.text(self.src).as_bytes()[0], b'+' | b'-')
                    }) =>
                {
                    let t = self.bump();
                    let negative = self.text(t).as_bytes()[0] == b'-';
                    let dt = match t.kind {
                        TokenKind::Integer => XSD_INTEGER,
                        TokenKind::Decimal => XSD_DECIMAL,
                        _ => XSD_DOUBLE,
                    };
                    // Magnitude only; the sign became the operator.
                    let term = Term {
                        kind: TermKind::Literal {
                            lexical: self.text(t)[1..].to_owned(),
                            kind: LiteralKind::Typed(dt.to_owned()),
                        },
                        span: t.span,
                    };
                    let mut right = Expr {
                        span: t.span,
                        kind: Box::new(ExprKind::Term(term)),
                    };
                    loop {
                        match self.kind() {
                            Some(TokenKind::Star) => {
                                self.bump();
                                let u = self.unary_expression()?;
                                right = self.spanned(t.span, ExprKind::Mul(right, u));
                            }
                            Some(TokenKind::Slash) => {
                                self.bump();
                                let u = self.unary_expression()?;
                                right = self.spanned(t.span, ExprKind::Div(right, u));
                            }
                            _ => break,
                        }
                    }
                    left = self.spanned(
                        start,
                        if negative {
                            ExprKind::Sub(left, right)
                        } else {
                            ExprKind::Add(left, right)
                        },
                    );
                }
                _ => return Ok(left),
            }
        }
    }

    fn multiplicative_expression(&mut self) -> Result<Expr, ParseError> {
        let start = self.peek().map(|t| t.span).unwrap_or_default2();
        let mut left = self.unary_expression()?;
        loop {
            match self.kind() {
                Some(TokenKind::Star) => {
                    self.bump();
                    let right = self.unary_expression()?;
                    left = self.spanned(start, ExprKind::Mul(left, right));
                }
                Some(TokenKind::Slash) => {
                    self.bump();
                    let right = self.unary_expression()?;
                    left = self.spanned(start, ExprKind::Div(left, right));
                }
                _ => return Ok(left),
            }
        }
    }

    fn unary_expression(&mut self) -> Result<Expr, ParseError> {
        let start = self.peek().map(|t| t.span).unwrap_or_default2();
        match self.kind() {
            Some(TokenKind::Bang) => {
                self.bump();
                let e = self.unary_expression()?;
                Ok(self.spanned(start, ExprKind::Not(e)))
            }
            Some(TokenKind::Plus) => {
                self.bump();
                let e = self.unary_expression()?;
                Ok(self.spanned(start, ExprKind::UnaryPlus(e)))
            }
            Some(TokenKind::Minus) => {
                self.bump();
                let e = self.unary_expression()?;
                Ok(self.spanned(start, ExprKind::UnaryMinus(e)))
            }
            _ => self.primary_expression(),
        }
    }

    fn primary_expression(&mut self) -> Result<Expr, ParseError> {
        self.descend()?;
        let e = self.primary_expression_inner();
        self.ascend();
        e
    }

    fn primary_expression_inner(&mut self) -> Result<Expr, ParseError> {
        let start = self.peek().map(|t| t.span).unwrap_or_default2();
        match self.kind() {
            Some(TokenKind::LParen) => {
                self.bump();
                let e = self.expression()?;
                self.expect(TokenKind::RParen, "`)`")?;
                Ok(e)
            }
            Some(TokenKind::Var) => {
                let t = self.bump();
                Ok(Expr {
                    span: t.span,
                    kind: Box::new(ExprKind::Term(self.var_term(t))),
                })
            }
            Some(
                TokenKind::String(_)
                | TokenKind::Integer
                | TokenKind::Decimal
                | TokenKind::Double
                | TokenKind::True
                | TokenKind::False,
            ) => {
                let term = self.literal()?;
                Ok(self.spanned(start, ExprKind::Term(term)))
            }
            Some(TokenKind::IriRef | TokenKind::PNameLn | TokenKind::PNameNs) => {
                // iriOrFunction.
                let term = self.iri_term()?;
                if self.kind() == Some(TokenKind::LParen) || self.kind() == Some(TokenKind::Nil) {
                    let TermKind::Iri(iri) = term.kind else {
                        unreachable!("iri_term returns IRIs")
                    };
                    let (distinct, args) = self.arg_list()?;
                    Ok(self.spanned(
                        start,
                        ExprKind::Function {
                            iri,
                            args,
                            distinct,
                        },
                    ))
                } else {
                    Ok(self.spanned(start, ExprKind::Term(term)))
                }
            }
            Some(TokenKind::LtLtParen) => {
                // Triple term as an expression (SPARQL 1.2); its
                // components emit no side triples and cannot contain
                // blank nodes (an expression cannot mint nodes).
                let mut side = Vec::new();
                let term = self.triple_term(&mut side)?;
                debug_assert!(side.is_empty(), "triple terms emit no triples");
                self.reject_bnodes_in_tt(&term)?;
                Ok(self.spanned(start, ExprKind::Term(term)))
            }
            Some(TokenKind::Keyword(Kw::Exists)) => {
                self.bump();
                let g = self.group_graph_pattern()?;
                Ok(self.spanned(start, ExprKind::Exists(g)))
            }
            Some(TokenKind::Keyword(Kw::Not)) => {
                self.bump();
                self.expect_kw(Kw::Exists)?;
                let g = self.group_graph_pattern()?;
                Ok(self.spanned(start, ExprKind::NotExists(g)))
            }
            Some(TokenKind::Keyword(kw)) => {
                if let Some(agg) = aggregate_of(kw) {
                    return self.aggregate(start, agg);
                }
                if let Some(b) = builtin_of(kw) {
                    return self.builtin_call(start, b);
                }
                Err(self.err_here("expected an expression"))
            }
            _ => Err(self.err_here("expected an expression")),
        }
    }

    /// `ArgList := NIL | '(' DISTINCT? Expr (',' Expr)* ')'`
    fn arg_list(&mut self) -> Result<(bool, Vec<Expr>), ParseError> {
        if self.take(TokenKind::Nil).is_some() {
            return Ok((false, Vec::new()));
        }
        self.expect(TokenKind::LParen, "`(`")?;
        let distinct = self.take_kw(Kw::Distinct).is_some();
        let mut args = vec![self.expression()?];
        while self.take(TokenKind::Comma).is_some() {
            args.push(self.expression()?);
        }
        self.expect(TokenKind::RParen, "`)`")?;
        Ok((distinct, args))
    }

    fn builtin_call(&mut self, start: Span, b: Builtin) -> Result<Expr, ParseError> {
        self.bump(); // the keyword
        let (min, max) = builtin_arity(b);
        // Zero-arg builtins may use NIL.
        if max == 0 {
            if self.take(TokenKind::Nil).is_none() {
                self.expect(TokenKind::LParen, "`(`")?;
                self.expect(TokenKind::RParen, "`)`")?;
            }
            return Ok(self.spanned(start, ExprKind::Builtin(b, Vec::new())));
        }
        if min == 0 && self.take(TokenKind::Nil).is_some() {
            return Ok(self.spanned(start, ExprKind::Builtin(b, Vec::new())));
        }
        self.expect(TokenKind::LParen, "`(`")?;
        if min == 0 && self.take(TokenKind::RParen).is_some() {
            return Ok(self.spanned(start, ExprKind::Builtin(b, Vec::new())));
        }
        let mut args = vec![self.expression()?];
        while self.take(TokenKind::Comma).is_some() {
            args.push(self.expression()?);
        }
        self.expect(TokenKind::RParen, "`)`")?;
        if args.len() < min || (max != usize::MAX && args.len() > max) {
            return Err(self.err(
                start,
                format!("wrong number of arguments for {}", builtin_name(b)),
            ));
        }
        Ok(self.spanned(start, ExprKind::Builtin(b, args)))
    }

    fn aggregate(&mut self, start: Span, func: Aggregate) -> Result<Expr, ParseError> {
        self.bump(); // the keyword
        self.expect(TokenKind::LParen, "`(`")?;
        let distinct = self.take_kw(Kw::Distinct).is_some();
        let expr = if func == Aggregate::Count && self.take(TokenKind::Star).is_some() {
            None
        } else {
            let e = self.expression()?;
            if has_aggregate(&e) {
                return Err(self.err(e.span, "aggregate calls cannot be nested"));
            }
            Some(e)
        };
        let mut separator = None;
        if func == Aggregate::GroupConcat && self.take(TokenKind::Semicolon).is_some() {
            self.expect_kw(Kw::Separator)?;
            self.expect(TokenKind::Eq, "`=`")?;
            match self.kind() {
                Some(TokenKind::String(f)) => {
                    let t = self.bump();
                    separator = Some(self.decode_string(t, f)?);
                }
                _ => return Err(self.err_here("expected a separator string")),
            }
        }
        self.expect(TokenKind::RParen, "`)`")?;
        Ok(self.spanned(
            start,
            ExprKind::Aggregate {
                func,
                distinct,
                expr,
                separator,
            },
        ))
    }
}

impl<'a> Parser<'a> {
    /// The grammar's SELECT-level assignment rules (§19.8 notes): with
    /// GROUP BY, `*` is illegal, projected variables must be group keys,
    /// and `(expr AS v)` must not collide with the exposed scope (group
    /// keys when grouped, else the pattern's in-scope set) or an earlier
    /// projection alias.
    fn validate_select(
        &self,
        select: &SelectClause,
        pattern: &GroupPattern,
        modifiers: &SolutionModifiers,
        at: Span,
    ) -> Result<(), ParseError> {
        let grouped = !modifiers.group_by.is_empty();
        let mut exposed = std::collections::HashSet::new();
        if grouped {
            if select.projection.is_empty() {
                return Err(self.err(at, "SELECT * cannot be used with GROUP BY"));
            }
            for c in &modifiers.group_by {
                match c {
                    GroupCondition::Var(v) => {
                        exposed.insert(v.clone());
                    }
                    GroupCondition::Expr(_, Some(v)) => {
                        exposed.insert(v.clone());
                    }
                    GroupCondition::Expr(_, None) => {}
                }
            }
        } else {
            pattern_vars(pattern, &mut exposed);
        }
        let mut assigned = std::collections::HashSet::new();
        for p in &select.projection {
            match p {
                Projection::Var(v) => {
                    if grouped && !exposed.contains(v) {
                        return Err(
                            self.err(at, format!("?{v} is projected but is not a GROUP BY key"))
                        );
                    }
                }
                Projection::Expr(e, v) => {
                    if exposed.contains(v) || assigned.contains(v) {
                        return Err(self.err(
                            e.span,
                            format!("?{v} is already in scope at this SELECT expression"),
                        ));
                    }
                    assigned.insert(v.clone());
                }
            }
        }
        Ok(())
    }

    // ------------------------------------------------------------ update

    fn update(&mut self) -> Result<UpdateRequest, ParseError> {
        let mut version = None;
        let mut operations = Vec::new();
        loop {
            let before = self.at;
            let depth_before = self.depth;
            match self.update_step(&mut version, &mut operations) {
                Ok(true) => {}
                Ok(false) => break,
                Err(e) if self.recovering => {
                    self.errors.push(e);
                    self.depth = depth_before;
                    self.resync_update(before);
                    if self.kind().is_none() {
                        break;
                    }
                }
                Err(e) => return Err(e),
            }
        }
        Ok(UpdateRequest {
            version,
            prefixes: self.prefix_order.clone(),
            operations,
        })
    }

    /// Recovery resync between update operations: guarantee ≥1 token of
    /// progress, then skip to just past the next *top-level* `;` (brace depth
    /// tracked, since `;` also separates predicate lists inside templates).
    fn resync_update(&mut self, before: usize) {
        if self.at == before {
            self.at += 1;
        }
        let mut depth = 0i32;
        while let Some(kind) = self.kind() {
            match kind {
                TokenKind::LBrace => depth += 1,
                TokenKind::RBrace => depth = (depth - 1).max(0),
                TokenKind::Semicolon if depth == 0 => {
                    self.at += 1;
                    return;
                }
                _ => {}
            }
            self.at += 1;
        }
    }

    /// One update operation (with its prologue). `Ok(true)` = a `;` follows,
    /// keep looping; `Ok(false)` = the request ended cleanly.
    fn update_step(
        &mut self,
        version: &mut Option<String>,
        operations: &mut Vec<UpdateOp>,
    ) -> Result<bool, ParseError> {
        // Each operation gets its own (accumulating) prologue.
        let v = self.prologue()?;
        if v.is_some() {
            *version = v;
        }
        let Some(t) = self.peek() else {
            return Ok(false);
        };
        let op = match t.kind {
            TokenKind::Keyword(Kw::Insert) => {
                self.bump();
                if self.take_kw(Kw::Data).is_some() {
                    UpdateOp::InsertData(self.quad_data(QuadMode::GroundWithBnodes)?)
                } else {
                    self.modify(None, false)?
                }
            }
            TokenKind::Keyword(Kw::Delete) => {
                self.bump();
                if self.take_kw(Kw::Data).is_some() {
                    UpdateOp::DeleteData(self.quad_data(QuadMode::Ground)?)
                } else if self.take_kw(Kw::Where).is_some() {
                    UpdateOp::DeleteWhere(self.quad_data(QuadMode::NoBnodes)?)
                } else {
                    self.modify(None, true)?
                }
            }
            TokenKind::Keyword(Kw::With) => {
                self.bump();
                let with = self.iri_string()?;
                match self.kind() {
                    Some(TokenKind::Keyword(Kw::Insert)) => {
                        self.bump();
                        self.modify(Some(with), false)?
                    }
                    Some(TokenKind::Keyword(Kw::Delete)) => {
                        self.bump();
                        self.modify(Some(with), true)?
                    }
                    _ => return Err(self.err_here("expected INSERT or DELETE after WITH")),
                }
            }
            TokenKind::Keyword(Kw::Load) => {
                self.bump();
                let silent = self.take_kw(Kw::Silent).is_some();
                let source = self.iri_string()?;
                let into = if self.take_kw(Kw::Into).is_some() {
                    self.expect_kw(Kw::Graph)?;
                    Some(self.iri_string()?)
                } else {
                    None
                };
                UpdateOp::Load {
                    silent,
                    source,
                    into,
                }
            }
            TokenKind::Keyword(Kw::Clear) => {
                self.bump();
                let silent = self.take_kw(Kw::Silent).is_some();
                UpdateOp::Clear {
                    silent,
                    target: self.graph_ref_all()?,
                }
            }
            TokenKind::Keyword(Kw::Drop) => {
                self.bump();
                let silent = self.take_kw(Kw::Silent).is_some();
                UpdateOp::Drop {
                    silent,
                    target: self.graph_ref_all()?,
                }
            }
            TokenKind::Keyword(Kw::Create) => {
                self.bump();
                let silent = self.take_kw(Kw::Silent).is_some();
                self.expect_kw(Kw::Graph)?;
                UpdateOp::Create {
                    silent,
                    graph: self.iri_string()?,
                }
            }
            TokenKind::Keyword(kw @ (Kw::Add | Kw::Move | Kw::Copy)) => {
                self.bump();
                let silent = self.take_kw(Kw::Silent).is_some();
                let from = self.graph_or_default()?;
                self.expect_kw(Kw::To)?;
                let to = self.graph_or_default()?;
                match kw {
                    Kw::Add => UpdateOp::Add { silent, from, to },
                    Kw::Move => UpdateOp::Move { silent, from, to },
                    _ => UpdateOp::Copy { silent, from, to },
                }
            }
            _ => return Err(self.err_here("expected an update operation")),
        };
        operations.push(op);
        self.update_op += 1;
        Ok(self.take(TokenKind::Semicolon).is_some())
    }

    /// The Modify tail after `[WITH iri] DELETE|INSERT` (the leading
    /// keyword already consumed; `delete_first` says which one it was).
    fn modify(&mut self, with: Option<String>, delete_first: bool) -> Result<UpdateOp, ParseError> {
        let mut delete = None;
        let mut insert = None;
        if delete_first {
            delete = Some(self.quad_data(QuadMode::NoBnodes)?);
            if self.take_kw(Kw::Insert).is_some() {
                insert = Some(self.quad_data(QuadMode::Vars)?);
            }
        } else {
            insert = Some(self.quad_data(QuadMode::Vars)?);
        }
        let mut using = Vec::new();
        while self.take_kw(Kw::Using).is_some() {
            if self.take_kw(Kw::Named).is_some() {
                using.push(DatasetClause::Named(self.iri_string()?));
            } else {
                using.push(DatasetClause::Default(self.iri_string()?));
            }
        }
        self.expect_kw(Kw::Where)?;
        let pattern = self.group_graph_pattern()?;
        Ok(UpdateOp::Modify {
            with,
            delete,
            insert,
            using,
            pattern,
        })
    }

    fn graph_ref_all(&mut self) -> Result<GraphTarget, ParseError> {
        match self.kind() {
            Some(TokenKind::Keyword(Kw::Graph)) => {
                self.bump();
                Ok(GraphTarget::Graph(self.iri_string()?))
            }
            Some(TokenKind::Keyword(Kw::Default)) => {
                self.bump();
                Ok(GraphTarget::Default)
            }
            Some(TokenKind::Keyword(Kw::Named)) => {
                self.bump();
                Ok(GraphTarget::Named)
            }
            Some(TokenKind::Keyword(Kw::All)) => {
                self.bump();
                Ok(GraphTarget::All)
            }
            _ => Err(self.err_here("expected GRAPH <iri>, DEFAULT, NAMED, or ALL")),
        }
    }

    fn graph_or_default(&mut self) -> Result<GraphOrDefault, ParseError> {
        match self.kind() {
            Some(TokenKind::Keyword(Kw::Default)) => {
                self.bump();
                Ok(GraphOrDefault::Default)
            }
            Some(TokenKind::Keyword(Kw::Graph)) => {
                self.bump();
                Ok(GraphOrDefault::Graph(self.iri_string()?))
            }
            _ => Ok(GraphOrDefault::Graph(self.iri_string()?)),
        }
    }

    /// `'{' Quads '}'` — triples templates interleaved with
    /// `GRAPH g { … }` blocks, flattened to quads and validated per mode.
    fn quad_data(&mut self, mode: QuadMode) -> Result<Vec<Quad>, ParseError> {
        let open = self.expect(TokenKind::LBrace, "`{`")?;
        // Template labels are scoped to the template: reuse against WHERE
        // patterns or other operations is legal, so shelve the tracker.
        let saved = std::mem::take(&mut self.bnode_epochs);
        self.next_bgp_scope();
        let mut quads = Vec::new();
        loop {
            match self.kind() {
                Some(TokenKind::RBrace) => {
                    self.bump();
                    break;
                }
                Some(TokenKind::Keyword(Kw::Graph)) => {
                    self.bump();
                    let g = self.var_or_iri()?;
                    self.expect(TokenKind::LBrace, "`{`")?;
                    let mut triples = Vec::new();
                    while self.kind() != Some(TokenKind::RBrace) {
                        self.triples_same_subject(&mut triples, false)?;
                        if self.take(TokenKind::Dot).is_none() {
                            break;
                        }
                    }
                    self.expect(TokenKind::RBrace, "`}`")?;
                    self.take(TokenKind::Dot);
                    for t in triples {
                        quads.push(Quad {
                            graph: Some(g.clone()),
                            triple: t,
                        });
                    }
                }
                Some(_) => {
                    let mut triples = Vec::new();
                    self.triples_same_subject(&mut triples, false)?;
                    self.take(TokenKind::Dot);
                    for t in triples {
                        quads.push(Quad {
                            graph: None,
                            triple: t,
                        });
                    }
                }
                None => return Err(self.err_here("unclosed quad template")),
            }
        }
        // Blank-node labels are OPERATION-scoped. Reuse across operations
        // is legal for INSERT templates (spec errata; the basic-update
        // same-bnode tests assert distinct nodes per operation) but stays
        // forbidden between INSERT DATA operations (syntax-update-54).
        let template = std::mem::replace(&mut self.bnode_epochs, saved);
        let is_data = matches!(mode, QuadMode::GroundWithBnodes);
        for label in template.into_keys() {
            match self.template_labels.get(&label) {
                Some(&op) if is_data && op != self.update_op => {
                    return Err(self.err(
                        open.span,
                        format!(
                            "blank node label `_:{label}` reused across INSERT DATA operations"
                        ),
                    ));
                }
                _ if is_data => {
                    self.template_labels.insert(label, self.update_op);
                }
                _ => {}
            }
        }
        self.validate_quads(&quads, mode, open.span)?;
        Ok(quads)
    }

    /// Data-context triple terms (VALUES rows, quad data): ground, and
    /// the subject must be an IRI or blank node — literals and nested
    /// triple terms are pattern-context-only subjects.
    fn validate_data_tt(&self, t: &Term) -> Result<(), ParseError> {
        let TermKind::TripleTerm(tp) = &t.kind else {
            return Ok(());
        };
        match &tp.s.kind {
            TermKind::Iri(_) | TermKind::BlankNode(_) => {}
            _ => {
                return Err(self.err(
                    tp.s.span,
                    "a data triple term's subject must be an IRI or blank node",
                ));
            }
        }
        match &tp.o.kind {
            TermKind::Var(_) => Err(self.err(tp.o.span, "data triple terms must be ground")),
            _ => self.validate_data_tt(&tp.o),
        }?;
        match &tp.p {
            Verb::Term(Term {
                kind: TermKind::Var(_),
                span,
            }) => Err(self.err(*span, "data triple terms must be ground")),
            _ => Ok(()),
        }
    }

    /// Expression triple terms: no blank nodes anywhere (an expression
    /// cannot mint nodes), and the subject must be an IRI or variable.
    fn reject_bnodes_in_tt(&self, t: &Term) -> Result<(), ParseError> {
        match &t.kind {
            TermKind::BlankNode(_) => Err(self.err(
                t.span,
                "blank nodes are not allowed in expression triple terms",
            )),
            TermKind::TripleTerm(tp) => {
                match &tp.s.kind {
                    TermKind::Iri(_) | TermKind::Var(_) | TermKind::BlankNode(_) => {}
                    _ => {
                        return Err(self.err(
                            tp.s.span,
                            "a triple term's subject must be an IRI, variable, or blank node",
                        ));
                    }
                }
                self.reject_bnodes_in_tt(&tp.s)?;
                self.reject_bnodes_in_tt(&tp.o)
            }
            _ => Ok(()),
        }
    }

    fn validate_quads(&self, quads: &[Quad], mode: QuadMode, span: Span) -> Result<(), ParseError> {
        let no_vars = matches!(mode, QuadMode::Ground | QuadMode::GroundWithBnodes);
        let no_bnodes = matches!(mode, QuadMode::Ground | QuadMode::NoBnodes);
        let check_term = |t: &Term| -> Result<(), ParseError> {
            match &t.kind {
                TermKind::Var(v) if no_vars => Err(self.err(
                    t.span,
                    format!("variable ?{v} not allowed in ground quad data"),
                )),
                TermKind::BlankNode(_) if no_bnodes => Err(self.err(
                    if t.span.is_empty() { span } else { t.span },
                    "blank nodes are not allowed in DELETE templates",
                )),
                _ => Ok(()),
            }
        };
        for q in quads {
            if let Some(g) = &q.graph {
                check_term(g)?;
            }
            check_term(&q.triple.s)?;
            match &q.triple.p {
                Verb::Term(p) => check_term(p)?,
                Verb::Path(_) => {
                    return Err(self.err(span, "property paths are not allowed in templates"));
                }
            }
            check_term(&q.triple.o)?;
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// In-scope variables (§18.2.1) and the grammar's assignment-scope rules
// (§19.8 notes; enforced by the W3C *syntax* suites).
// ---------------------------------------------------------------------------

fn term_vars(t: &Term, out: &mut std::collections::HashSet<String>) {
    match &t.kind {
        TermKind::Var(v) => {
            out.insert(v.clone());
        }
        TermKind::TripleTerm(tp) => {
            term_vars(&tp.s, out);
            if let Verb::Term(p) = &tp.p {
                term_vars(p, out);
            }
            term_vars(&tp.o, out);
        }
        _ => {}
    }
}

fn pattern_vars(g: &GroupPattern, out: &mut std::collections::HashSet<String>) {
    for e in &g.elements {
        match e {
            GroupElement::Triples(ts) => {
                for t in ts {
                    term_vars(&t.s, out);
                    if let Verb::Term(p) = &t.p {
                        term_vars(p, out);
                    }
                    term_vars(&t.o, out);
                }
            }
            // FILTER and MINUS do not contribute to in-scope.
            GroupElement::Filter(_) | GroupElement::Minus(_) => {}
            GroupElement::Optional(g) => pattern_vars(g, out),
            GroupElement::Union(branches) => {
                for b in branches {
                    pattern_vars(b, out);
                }
            }
            GroupElement::Graph(t, g) => {
                term_vars(t, out);
                pattern_vars(g, out);
            }
            GroupElement::Service {
                target, pattern, ..
            } => {
                term_vars(target, out);
                pattern_vars(pattern, out);
            }
            GroupElement::Bind { var, .. } => {
                out.insert(var.clone());
            }
            GroupElement::Values(v) => out.extend(v.vars.iter().cloned()),
            GroupElement::SubSelect(sub) => {
                if sub.select.projection.is_empty() {
                    pattern_vars(&sub.pattern, out);
                } else {
                    for p in &sub.select.projection {
                        match p {
                            Projection::Var(v) => {
                                out.insert(v.clone());
                            }
                            Projection::Expr(_, v) => {
                                out.insert(v.clone());
                            }
                        }
                    }
                }
            }
        }
    }
}

/// Any aggregate call anywhere in the expression?
fn has_aggregate(e: &Expr) -> bool {
    match &*e.kind {
        ExprKind::Aggregate { .. } => true,
        ExprKind::Or(a, b)
        | ExprKind::And(a, b)
        | ExprKind::Cmp(_, a, b)
        | ExprKind::Add(a, b)
        | ExprKind::Sub(a, b)
        | ExprKind::Mul(a, b)
        | ExprKind::Div(a, b) => has_aggregate(a) || has_aggregate(b),
        ExprKind::In { expr, list, .. } => has_aggregate(expr) || list.iter().any(has_aggregate),
        ExprKind::Not(a) | ExprKind::UnaryMinus(a) | ExprKind::UnaryPlus(a) => has_aggregate(a),
        ExprKind::Builtin(_, args) => args.iter().any(has_aggregate),
        ExprKind::Function { args, .. } => args.iter().any(has_aggregate),
        ExprKind::Exists(_) | ExprKind::NotExists(_) | ExprKind::Term(_) => false,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum QuadMode {
    /// No variables, no blank nodes (DELETE DATA).
    Ground,
    /// No variables; blank nodes mint fresh nodes (INSERT DATA).
    GroundWithBnodes,
    /// Variables allowed, no blank nodes (DELETE WHERE / DELETE clause).
    NoBnodes,
    /// Variables and blank nodes allowed (INSERT clause).
    Vars,
}

/// `Option<Span>::unwrap_or_default` reads poorly at call sites; a tiny
/// extension keeps the expression starts terse.
trait SpanDefault {
    fn unwrap_or_default2(self) -> Span;
}

impl SpanDefault for Option<Span> {
    fn unwrap_or_default2(self) -> Span {
        self.unwrap_or(Span { start: 0, end: 0 })
    }
}

fn aggregate_of(kw: Kw) -> Option<Aggregate> {
    Some(match kw {
        Kw::Count => Aggregate::Count,
        Kw::Sum => Aggregate::Sum,
        Kw::Min => Aggregate::Min,
        Kw::Max => Aggregate::Max,
        Kw::Avg => Aggregate::Avg,
        Kw::Sample => Aggregate::Sample,
        Kw::GroupConcat => Aggregate::GroupConcat,
        _ => return None,
    })
}

fn builtin_of(kw: Kw) -> Option<Builtin> {
    use Builtin as B;
    Some(match kw {
        Kw::Str => B::Str,
        Kw::Lang => B::Lang,
        Kw::LangMatches => B::LangMatches,
        Kw::Datatype => B::Datatype,
        Kw::Bound => B::Bound,
        Kw::Iri | Kw::Uri => B::Iri,
        Kw::BNode => B::BNode,
        Kw::Rand => B::Rand,
        Kw::Abs => B::Abs,
        Kw::Ceil => B::Ceil,
        Kw::Floor => B::Floor,
        Kw::Round => B::Round,
        Kw::Concat => B::Concat,
        Kw::StrLen => B::StrLen,
        Kw::UCase => B::UCase,
        Kw::LCase => B::LCase,
        Kw::EncodeForUri => B::EncodeForUri,
        Kw::Contains => B::Contains,
        Kw::StrStarts => B::StrStarts,
        Kw::StrEnds => B::StrEnds,
        Kw::StrBefore => B::StrBefore,
        Kw::StrAfter => B::StrAfter,
        Kw::Year => B::Year,
        Kw::Month => B::Month,
        Kw::Day => B::Day,
        Kw::Hours => B::Hours,
        Kw::Minutes => B::Minutes,
        Kw::Seconds => B::Seconds,
        Kw::Timezone => B::Timezone,
        Kw::Tz => B::Tz,
        Kw::Now => B::Now,
        Kw::Uuid => B::Uuid,
        Kw::StrUuid => B::StrUuid,
        Kw::Md5 => B::Md5,
        Kw::Sha1 => B::Sha1,
        Kw::Sha256 => B::Sha256,
        Kw::Sha384 => B::Sha384,
        Kw::Sha512 => B::Sha512,
        Kw::Coalesce => B::Coalesce,
        Kw::If => B::If,
        Kw::StrLang => B::StrLang,
        Kw::StrDt => B::StrDt,
        Kw::SameTerm => B::SameTerm,
        Kw::IsIri | Kw::IsUri => B::IsIri,
        Kw::IsBlank => B::IsBlank,
        Kw::IsLiteral => B::IsLiteral,
        Kw::IsNumeric => B::IsNumeric,
        Kw::Regex => B::Regex,
        Kw::Substr => B::Substr,
        Kw::Replace => B::Replace,
        Kw::Triple => B::Triple,
        Kw::Subject => B::Subject,
        Kw::Predicate => B::Predicate,
        Kw::Object => B::Object,
        Kw::IsTriple => B::IsTriple,
        Kw::LangDir => B::LangDir,
        Kw::HasLang => B::HasLang,
        Kw::HasLangDir => B::HasLangDir,
        Kw::StrLangDir => B::StrLangDir,
        _ => return None,
    })
}

/// `(min, max)` argument counts (`usize::MAX` = unbounded).
fn builtin_arity(b: Builtin) -> (usize, usize) {
    use Builtin as B;
    match b {
        B::Rand | B::Now | B::Uuid | B::StrUuid => (0, 0),
        B::BNode => (0, 1),
        // ExpressionList admits NIL: `COALESCE()` / `CONCAT()` are valid
        // (evaluation: COALESCE() errors, CONCAT() is the empty string).
        B::Concat | B::Coalesce => (0, usize::MAX),
        B::Str
        | B::Lang
        | B::Datatype
        | B::Bound
        | B::Iri
        | B::Abs
        | B::Ceil
        | B::Floor
        | B::Round
        | B::StrLen
        | B::UCase
        | B::LCase
        | B::EncodeForUri
        | B::Year
        | B::Month
        | B::Day
        | B::Hours
        | B::Minutes
        | B::Seconds
        | B::Timezone
        | B::Tz
        | B::Md5
        | B::Sha1
        | B::Sha256
        | B::Sha384
        | B::Sha512
        | B::IsIri
        | B::IsBlank
        | B::IsLiteral
        | B::IsNumeric
        | B::IsTriple
        | B::Subject
        | B::Predicate
        | B::Object
        | B::LangDir
        | B::HasLang
        | B::HasLangDir => (1, 1),
        B::LangMatches
        | B::Contains
        | B::StrStarts
        | B::StrEnds
        | B::StrBefore
        | B::StrAfter
        | B::StrLang
        | B::StrDt
        | B::SameTerm => (2, 2),
        B::If | B::Triple | B::StrLangDir => (3, 3),
        B::Regex | B::Substr => (2, 3),
        B::Replace => (3, 4),
    }
}

fn builtin_name(b: Builtin) -> &'static str {
    use Builtin as B;
    match b {
        B::Str => "STR",
        B::Lang => "LANG",
        B::LangMatches => "LANGMATCHES",
        B::Datatype => "DATATYPE",
        B::Bound => "BOUND",
        B::Iri => "IRI",
        B::BNode => "BNODE",
        B::Rand => "RAND",
        B::Abs => "ABS",
        B::Ceil => "CEIL",
        B::Floor => "FLOOR",
        B::Round => "ROUND",
        B::Concat => "CONCAT",
        B::StrLen => "STRLEN",
        B::UCase => "UCASE",
        B::LCase => "LCASE",
        B::EncodeForUri => "ENCODE_FOR_URI",
        B::Contains => "CONTAINS",
        B::StrStarts => "STRSTARTS",
        B::StrEnds => "STRENDS",
        B::StrBefore => "STRBEFORE",
        B::StrAfter => "STRAFTER",
        B::Year => "YEAR",
        B::Month => "MONTH",
        B::Day => "DAY",
        B::Hours => "HOURS",
        B::Minutes => "MINUTES",
        B::Seconds => "SECONDS",
        B::Timezone => "TIMEZONE",
        B::Tz => "TZ",
        B::Now => "NOW",
        B::Uuid => "UUID",
        B::StrUuid => "STRUUID",
        B::Md5 => "MD5",
        B::Sha1 => "SHA1",
        B::Sha256 => "SHA256",
        B::Sha384 => "SHA384",
        B::Sha512 => "SHA512",
        B::Coalesce => "COALESCE",
        B::If => "IF",
        B::StrLang => "STRLANG",
        B::StrDt => "STRDT",
        B::SameTerm => "SAMETERM",
        B::IsIri => "ISIRI",
        B::IsBlank => "ISBLANK",
        B::IsLiteral => "ISLITERAL",
        B::IsNumeric => "ISNUMERIC",
        B::Regex => "REGEX",
        B::Substr => "SUBSTR",
        B::Replace => "REPLACE",
        B::Triple => "TRIPLE",
        B::Subject => "SUBJECT",
        B::Predicate => "PREDICATE",
        B::Object => "OBJECT",
        B::IsTriple => "ISTRIPLE",
        B::LangDir => "LANGDIR",
        B::HasLang => "HASLANG",
        B::HasLangDir => "HASLANGDIR",
        B::StrLangDir => "STRLANGDIR",
    }
}
