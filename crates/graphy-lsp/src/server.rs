//! The LSP server: capabilities, the stdio JSON-RPC loop, and request/
//! notification dispatch (docs/10 §4). Synchronous — no async runtime
//! (`lsp-server` + `lsp-types`); the tier-1 analyzers are CPU-bound and
//! per-document, so a plain loop with one document store is all that is needed.

use std::collections::HashMap;

use lsp_server::{Connection, ErrorCode, Message, RequestId, Response, ResponseError};
use lsp_types::notification::{
    DidChangeTextDocument, DidCloseTextDocument, DidOpenTextDocument, LogMessage,
    Notification as _, PublishDiagnostics,
};
use lsp_types::request::{
    CodeActionRequest, Completion as CompletionRequest, DocumentSymbolRequest, FoldingRangeRequest,
    Formatting, HoverRequest, Request as _, SemanticTokensFullDeltaRequest,
    SemanticTokensFullRequest, SemanticTokensRangeRequest,
};
use lsp_types::{
    CodeAction, CodeActionKind, CodeActionOrCommand, CodeActionProviderCapability, CompletionItem,
    CompletionItemKind, CompletionOptions, CompletionResponse, Diagnostic, DiagnosticSeverity,
    DocumentSymbol, DocumentSymbolResponse, FoldingRange, FoldingRangeProviderCapability, Hover,
    HoverContents, HoverProviderCapability, LogMessageParams, MarkupContent, MarkupKind,
    MessageType, OneOf, Position, PublishDiagnosticsParams, Range, SemanticToken,
    SemanticTokenModifier, SemanticTokenType, SemanticTokens, SemanticTokensDelta,
    SemanticTokensEdit, SemanticTokensFullDeltaResult, SemanticTokensFullOptions,
    SemanticTokensLegend, SemanticTokensOptions, SemanticTokensRangeResult, SemanticTokensResult,
    SemanticTokensServerCapabilities, ServerCapabilities, SymbolKind, TextDocumentSyncCapability,
    TextDocumentSyncKind, TextEdit, Uri, WorkspaceEdit,
};
use serde::de::DeserializeOwned;
use serde::Serialize;

use crate::completion::{CompKind, Completion};
use crate::diagnostics::{Diag, FixKind, Sev};
use crate::document::{Doc, DocStore, Lang};
use crate::legend::{SemKind, SemMod};
use crate::line_index::LineIndex;
use crate::semantic::{encode, SemToken};
use crate::symbols::{SymKind, Symbol};

type BoxError = Box<dyn std::error::Error + Sync + Send>;

/// Build and run the server over stdio: handshake, main loop, clean shutdown.
pub fn run_stdio() -> Result<(), BoxError> {
    let (connection, io_threads) = Connection::stdio();
    let caps = serde_json::to_value(capabilities())?;
    connection.initialize(caps)?;
    main_loop(&connection)?;
    // Drop the connection *before* joining so the writer thread's channel
    // closes and the I/O threads can exit (otherwise `join` hangs forever).
    drop(connection);
    io_threads.join()?;
    Ok(())
}

/// The dispatch loop. Public (and initialize-free) so tests can drive it over an
/// in-memory connection.
pub fn main_loop(connection: &Connection) -> Result<(), BoxError> {
    let mut state = State::default();
    // First thing in the client's output channel: proof of life + version.
    log(
        connection,
        MessageType::INFO,
        format!("graphy-lsp {} ready", env!("CARGO_PKG_VERSION")),
    )?;
    for msg in &connection.receiver {
        match msg {
            Message::Request(req) => {
                if connection.handle_shutdown(&req)? {
                    return Ok(());
                }
                handle_request(connection, &mut state, req)?;
            }
            Message::Notification(note) => handle_notification(connection, &mut state, note)?,
            Message::Response(_) => {}
        }
    }
    Ok(())
}

/// Per-connection server state: the open documents plus, for
/// `semanticTokens/full/delta`, the last token result handed out per document.
#[derive(Default)]
struct State {
    store: DocStore,
    /// uri → (result id, encoded data) of the last full/delta response.
    tokens: HashMap<Uri, (String, Vec<u32>)>,
    result_ids: u64,
}

impl State {
    fn fresh_result_id(&mut self) -> String {
        self.result_ids += 1;
        self.result_ids.to_string()
    }
}

/// The tier-1 capabilities we advertise (docs/10 §10 M11a).
pub fn capabilities() -> ServerCapabilities {
    ServerCapabilities {
        text_document_sync: Some(TextDocumentSyncCapability::Kind(
            TextDocumentSyncKind::INCREMENTAL,
        )),
        semantic_tokens_provider: Some(SemanticTokensServerCapabilities::SemanticTokensOptions(
            SemanticTokensOptions {
                work_done_progress_options: Default::default(),
                legend: SemanticTokensLegend {
                    token_types: SemKind::LEGEND
                        .iter()
                        .map(|s| SemanticTokenType::new(s))
                        .collect(),
                    token_modifiers: SemMod::LEGEND
                        .iter()
                        .map(|s| SemanticTokenModifier::new(s))
                        .collect(),
                },
                range: Some(true),
                full: Some(SemanticTokensFullOptions::Delta { delta: Some(true) }),
            },
        )),
        folding_range_provider: Some(FoldingRangeProviderCapability::Simple(true)),
        document_symbol_provider: Some(OneOf::Left(true)),
        code_action_provider: Some(CodeActionProviderCapability::Simple(true)),
        document_formatting_provider: Some(OneOf::Left(true)),
        hover_provider: Some(HoverProviderCapability::Simple(true)),
        completion_provider: Some(CompletionOptions {
            trigger_characters: Some([":", "?", "$", "@"].iter().map(|s| s.to_string()).collect()),
            ..Default::default()
        }),
        ..Default::default()
    }
}

fn handle_request(
    connection: &Connection,
    state: &mut State,
    req: lsp_server::Request,
) -> Result<(), BoxError> {
    let lsp_server::Request { id, method, params } = req;
    match method.as_str() {
        SemanticTokensFullRequest::METHOD => {
            let Some(params) =
                valid_params::<lsp_types::SemanticTokensParams>(connection, &id, params)?
            else {
                return Ok(());
            };
            let uri = params.text_document.uri;
            let encoded = state
                .store
                .get(&uri)
                .map(|doc| encode(&analyze_tokens(doc)));
            let result = encoded.map(|data| {
                let result_id = state.fresh_result_id();
                let wire = to_wire(&data);
                state.tokens.insert(uri, (result_id.clone(), data));
                SemanticTokensResult::Tokens(SemanticTokens {
                    result_id: Some(result_id),
                    data: wire,
                })
            });
            respond(connection, id, result)
        }
        SemanticTokensFullDeltaRequest::METHOD => {
            let Some(params) =
                valid_params::<lsp_types::SemanticTokensDeltaParams>(connection, &id, params)?
            else {
                return Ok(());
            };
            let uri = params.text_document.uri;
            let encoded = state
                .store
                .get(&uri)
                .map(|doc| encode(&analyze_tokens(doc)));
            let result = encoded.map(|data| {
                let result_id = state.fresh_result_id();
                let prev = state.tokens.insert(uri, (result_id.clone(), data.clone()));
                match prev {
                    // The client's baseline is the result we last handed out:
                    // answer with a splice against it.
                    Some((pid, pdata)) if pid == params.previous_result_id => {
                        let edits = if pdata == data {
                            Vec::new()
                        } else {
                            vec![token_edit(&pdata, &data)]
                        };
                        SemanticTokensFullDeltaResult::TokensDelta(SemanticTokensDelta {
                            result_id: Some(result_id),
                            edits,
                        })
                    }
                    // Unknown baseline: fall back to a full result (the spec's
                    // recovery path).
                    _ => SemanticTokensFullDeltaResult::Tokens(SemanticTokens {
                        result_id: Some(result_id),
                        data: to_wire(&data),
                    }),
                }
            });
            respond(connection, id, result)
        }
        SemanticTokensRangeRequest::METHOD => {
            let Some(params) =
                valid_params::<lsp_types::SemanticTokensRangeParams>(connection, &id, params)?
            else {
                return Ok(());
            };
            let range = params.range;
            let result = state.store.get(&params.text_document.uri).map(|doc| {
                let toks: Vec<SemToken> = analyze_tokens(doc)
                    .into_iter()
                    .filter(|t| t.line >= range.start.line && t.line <= range.end.line)
                    .collect();
                SemanticTokensRangeResult::Tokens(SemanticTokens {
                    result_id: None,
                    data: to_wire(&encode(&toks)),
                })
            });
            respond(connection, id, result)
        }
        CompletionRequest::METHOD => {
            let Some(params) =
                valid_params::<lsp_types::CompletionParams>(connection, &id, params)?
            else {
                return Ok(());
            };
            let tdp = params.text_document_position;
            let result = state.store.get(&tdp.text_document.uri).map(|doc| {
                let text = doc.text();
                let at = doc.byte_of(tdp.position);
                let items = match doc.lang {
                    Lang::Turtle => crate::turtle_completions(&text, at),
                    Lang::Sparql => crate::sparql_completions(&text, at),
                    Lang::JsonLd => crate::jsonld_completions(&text, at),
                };
                CompletionResponse::Array(items.into_iter().map(completion_item).collect())
            });
            respond(connection, id, result)
        }
        HoverRequest::METHOD => {
            let Some(params) = valid_params::<lsp_types::HoverParams>(connection, &id, params)?
            else {
                return Ok(());
            };
            let tdp = params.text_document_position_params;
            let result = state.store.get(&tdp.text_document.uri).and_then(|doc| {
                let text = doc.text();
                let at = doc.byte_of(tdp.position);
                let h = match doc.lang {
                    Lang::Turtle => crate::turtle_hover(&text, at),
                    Lang::Sparql => crate::sparql_hover(&text, at),
                    Lang::JsonLd => crate::jsonld_hover(&text, at),
                }?;
                let li = LineIndex::new(&text);
                Some(Hover {
                    contents: HoverContents::Markup(MarkupContent {
                        kind: MarkupKind::Markdown,
                        value: h.markdown,
                    }),
                    range: Some(byte_range(&text, &li, h.start, h.end)),
                })
            });
            respond(connection, id, result)
        }
        Formatting::METHOD => {
            let Some(params) =
                valid_params::<lsp_types::DocumentFormattingParams>(connection, &id, params)?
            else {
                return Ok(());
            };
            let uri = params.text_document.uri;
            // Formatting refusals are silent in the editor (a null result),
            // so every decision is logged to the client's output channel.
            let mut result = None;
            if let Some(doc) = state.store.get(&uri) {
                let text = doc.text();
                match doc.lang {
                    Lang::Sparql | Lang::JsonLd => log(
                        connection,
                        MessageType::INFO,
                        format!(
                            "format: only Turtle-family documents have a canonical \
                             pretty-print (this one is {:?})",
                            doc.lang
                        ),
                    )?,
                    Lang::Turtle => match pretty_print(doc, &uri, &text) {
                        Some(new) if new == text => {
                            log(
                                connection,
                                MessageType::INFO,
                                "format: already in canonical form, no edits".to_string(),
                            )?;
                            result = Some(Vec::new());
                        }
                        Some(new) => {
                            log(
                                connection,
                                MessageType::INFO,
                                format!("format: {} bytes -> {}", text.len(), new.len()),
                            )?;
                            result = Some(vec![whole_doc_edit(&text, new)]);
                        }
                        None => {
                            let errors = compute_diags(doc, &uri, &text)
                                .iter()
                                .filter(|d| d.sev == Sev::Error)
                                .count();
                            let why = if errors > 0 {
                                format!(
                                    "{errors} syntax error(s) — a broken buffer is never \
                                     reformatted (see the squiggles/Problems panel)"
                                )
                            } else {
                                "no data statements to canonicalize".to_string()
                            };
                            log(
                                connection,
                                MessageType::WARNING,
                                format!("format refused: {why}"),
                            )?;
                        }
                    },
                }
            }
            respond(connection, id, result)
        }
        CodeActionRequest::METHOD => {
            let Some(params) =
                valid_params::<lsp_types::CodeActionParams>(connection, &id, params)?
            else {
                return Ok(());
            };
            let uri = params.text_document.uri;
            let only = params.context.only;
            let result = state.store.get(&uri).map(|doc| {
                let text = doc.text();
                let li = LineIndex::new(&text);
                let diags = compute_diags(doc, &uri, &text);
                let mut actions = Vec::new();
                // Fix kinds whose diagnostic overlaps the requested range —
                // used to decide which aggregates also join the Quick Fix
                // (lightbulb) menu, which only shows `quickfix`-kind actions.
                let mut at_cursor: Vec<FixKind> = Vec::new();
                for d in &diags {
                    let Some(fix) = d.fix.clone() else { continue };
                    let dr = byte_range(&text, &li, d.start, d.end);
                    let r = params.range;
                    let overlaps = (dr.start.line, dr.start.character)
                        <= (r.end.line, r.end.character)
                        && (r.start.line, r.start.character) <= (dr.end.line, dr.end.character);
                    if !overlaps {
                        continue;
                    }
                    at_cursor.push(fix.kind);
                    if !kind_wanted(&only, "quickfix") {
                        continue;
                    }
                    let edits = fix
                        .edits
                        .iter()
                        .map(|e| TextEdit {
                            range: byte_range(&text, &li, e.start, e.end),
                            new_text: e.text.clone(),
                        })
                        .collect::<Vec<_>>();
                    // lsp-types mandates HashMap<Uri, _> here; Uri's interior
                    // mutability (clippy: mutable_key_type) never mutates keys.
                    #[allow(clippy::mutable_key_type)]
                    let changes = HashMap::from([(uri.clone(), edits)]);
                    actions.push(CodeActionOrCommand::CodeAction(CodeAction {
                        title: fix.title,
                        kind: Some(CodeActionKind::QUICKFIX),
                        diagnostics: Some(vec![lsp_diagnostic(&text, &li, d)]),
                        edit: Some(WorkspaceEdit {
                            changes: Some(changes),
                            ..Default::default()
                        }),
                        ..Default::default()
                    }));
                }
                // Aggregates: always offered as `source.*` (Source Action…
                // menu, codeActionsOnSave); ALSO offered as `quickfix` twins
                // in the lightbulb menu when the cursor sits on a fixable
                // diagnostic and the aggregate would do more than that one
                // fix (mirrors the TypeScript extension's UX).
                type KindFilter = fn(FixKind) -> bool;
                let sources: [(&str, &str, KindFilter); 2] = [
                    (
                        "source.removeUnusedImports",
                        "Remove all unused prefix declarations",
                        |k| k == FixKind::RemoveUnusedPrefix,
                    ),
                    ("source.fixAll", "Fix all auto-fixable problems", |_| true),
                ];
                for (kind, title, fix_wanted) in sources {
                    let mut edits = Vec::new();
                    let mut fixed_diags = Vec::new();
                    let mut count = 0usize;
                    for d in &diags {
                        let Some(f) = &d.fix else { continue };
                        if !fix_wanted(f.kind) {
                            continue;
                        }
                        count += 1;
                        edits.extend(f.edits.iter().map(|e| TextEdit {
                            range: byte_range(&text, &li, e.start, e.end),
                            new_text: e.text.clone(),
                        }));
                        fixed_diags.push(lsp_diagnostic(&text, &li, d));
                    }
                    if edits.is_empty() {
                        continue;
                    }
                    edits.sort_by_key(|e| (e.range.start.line, e.range.start.character));
                    #[allow(clippy::mutable_key_type)]
                    let changes = HashMap::from([(uri.clone(), edits)]);
                    let action = CodeAction {
                        title: format!("{title} ({count})"),
                        kind: Some(kind.to_string().into()),
                        diagnostics: Some(fixed_diags),
                        edit: Some(WorkspaceEdit {
                            changes: Some(changes),
                            ..Default::default()
                        }),
                        ..Default::default()
                    };
                    if kind_wanted(&only, "quickfix")
                        && count > 1
                        && at_cursor.iter().any(|k| fix_wanted(*k))
                    {
                        actions.push(CodeActionOrCommand::CodeAction(CodeAction {
                            kind: Some(CodeActionKind::QUICKFIX),
                            ..action.clone()
                        }));
                    }
                    if kind_wanted(&only, kind) {
                        actions.push(CodeActionOrCommand::CodeAction(action));
                    }
                }
                // The canonical pretty-print (graphy `read / tree / write`)
                // as an explicit source action, alongside Format Document.
                if kind_wanted(&only, "source.prettyPrint.graphy") {
                    if let Some(new) = pretty_print(doc, &uri, &text) {
                        if new != text {
                            // See the quickfix arm: Uri keys never mutate.
                            #[allow(clippy::mutable_key_type)]
                            let changes =
                                HashMap::from([(uri.clone(), vec![whole_doc_edit(&text, new)])]);
                            actions.push(CodeActionOrCommand::CodeAction(CodeAction {
                                title: "Pretty-print document (graphy tree + write)".to_string(),
                                kind: Some("source.prettyPrint.graphy".to_string().into()),
                                edit: Some(WorkspaceEdit {
                                    changes: Some(changes),
                                    ..Default::default()
                                }),
                                ..Default::default()
                            }));
                        }
                    }
                }
                actions
            });
            respond(connection, id, result)
        }
        FoldingRangeRequest::METHOD => {
            let Some(params) =
                valid_params::<lsp_types::FoldingRangeParams>(connection, &id, params)?
            else {
                return Ok(());
            };
            let result = state.store.get(&params.text_document.uri).map(folds);
            respond(connection, id, result)
        }
        DocumentSymbolRequest::METHOD => {
            let Some(params) =
                valid_params::<lsp_types::DocumentSymbolParams>(connection, &id, params)?
            else {
                return Ok(());
            };
            let result = state
                .store
                .get(&params.text_document.uri)
                .map(|doc| DocumentSymbolResponse::Nested(symbols(doc)));
            respond(connection, id, result)
        }
        _ => respond_error(
            connection,
            id,
            ErrorCode::MethodNotFound,
            format!("unhandled method: {method}"),
        ),
    }
}

fn handle_notification(
    connection: &Connection,
    state: &mut State,
    note: lsp_server::Notification,
) -> Result<(), BoxError> {
    match note.method.as_str() {
        DidOpenTextDocument::METHOD => {
            if let Ok(p) = de::<lsp_types::DidOpenTextDocumentParams>(note) {
                let td = p.text_document;
                let lang = Lang::detect(&td.language_id, &td.uri);
                state.tokens.remove(&td.uri);
                state
                    .store
                    .open(td.uri.clone(), Doc::new(&td.text, td.version, lang));
                publish_diagnostics(connection, state, &td.uri)?;
            }
        }
        DidChangeTextDocument::METHOD => {
            if let Ok(p) = de::<lsp_types::DidChangeTextDocumentParams>(note) {
                let uri = p.text_document.uri;
                if let Some(doc) = state.store.get_mut(&uri) {
                    for change in p.content_changes {
                        doc.apply(change);
                    }
                    doc.version = p.text_document.version;
                    publish_diagnostics(connection, state, &uri)?;
                }
            }
        }
        DidCloseTextDocument::METHOD => {
            if let Ok(p) = de::<lsp_types::DidCloseTextDocumentParams>(note) {
                state.tokens.remove(&p.text_document.uri);
                state.store.close(&p.text_document.uri);
                // Clear stale squiggles for the closed document.
                send_diagnostics(connection, p.text_document.uri, None, Vec::new())?;
            }
        }
        _ => {}
    }
    Ok(())
}

/// Compute and publish diagnostics for one open document (docs/10 §9).
fn publish_diagnostics(connection: &Connection, state: &State, uri: &Uri) -> Result<(), BoxError> {
    let Some(doc) = state.store.get(uri) else {
        return Ok(());
    };
    let text = doc.text();
    let li = LineIndex::new(&text);
    let wire = compute_diags(doc, uri, &text)
        .iter()
        .map(|d| lsp_diagnostic(&text, &li, d))
        .collect();
    send_diagnostics(connection, uri.clone(), Some(doc.version), wire)
}

/// Language-dispatched diagnostics for one document (shared by the publish
/// path and the codeAction handler, which needs the attached fixes).
fn compute_diags(doc: &Doc, uri: &Uri, text: &str) -> Vec<Diag> {
    match doc.lang {
        // The document's own URI is the base for relative references.
        Lang::Turtle => crate::turtle_diagnostics(text, Some(uri.as_str())),
        Lang::Sparql => crate::sparql_diagnostics(text),
        Lang::JsonLd => crate::jsonld_diagnostics(text),
    }
}

/// The graphy canonical pretty-print for a document, when its language has
/// one (docs/09 `read / tree / write`; Turtle family only).
fn pretty_print(doc: &Doc, uri: &Uri, text: &str) -> Option<String> {
    match doc.lang {
        Lang::Turtle => crate::turtle_pretty(text, Some(uri.as_str())),
        Lang::Sparql | Lang::JsonLd => None,
    }
}

/// One edit replacing the whole document.
fn whole_doc_edit(text: &str, new_text: String) -> TextEdit {
    let li = LineIndex::new(text);
    TextEdit {
        range: byte_range(text, &li, 0, text.len() as u32),
        new_text,
    }
}

/// LSP codeAction kind filtering: when the client sends `only`, an action is
/// wanted iff some requested kind is it or one of its dotted prefixes.
fn kind_wanted(only: &Option<Vec<CodeActionKind>>, kind: &str) -> bool {
    match only {
        None => true,
        Some(ks) => ks.iter().any(|k| {
            let k = k.as_str();
            k.is_empty() || kind == k || kind.starts_with(&format!("{k}."))
        }),
    }
}

/// Byte span → LSP range via the line index.
fn byte_range(text: &str, li: &LineIndex, start: u32, end: u32) -> Range {
    let (sl, sc) = li.position(text, start);
    let (el, ec) = li.position(text, end);
    Range::new(Position::new(sl, sc), Position::new(el, ec))
}

fn lsp_diagnostic(text: &str, li: &LineIndex, d: &Diag) -> Diagnostic {
    Diagnostic {
        range: byte_range(text, li, d.start, d.end),
        severity: Some(match d.sev {
            Sev::Error => DiagnosticSeverity::ERROR,
            Sev::Warning => DiagnosticSeverity::WARNING,
        }),
        source: Some("graphy-lsp".to_string()),
        message: d.message.clone(),
        ..Default::default()
    }
}

/// `window/logMessage` — lands in the client's output channel unconditionally
/// (unlike `$/logTrace`, which needs the trace setting), so silent decisions
/// like formatting refusals stay explainable.
fn log(connection: &Connection, typ: MessageType, message: String) -> Result<(), BoxError> {
    connection
        .sender
        .send(Message::Notification(lsp_server::Notification {
            method: LogMessage::METHOD.to_string(),
            params: serde_json::to_value(LogMessageParams { typ, message })?,
        }))?;
    Ok(())
}

fn send_diagnostics(
    connection: &Connection,
    uri: Uri,
    version: Option<i32>,
    diagnostics: Vec<Diagnostic>,
) -> Result<(), BoxError> {
    let params = PublishDiagnosticsParams {
        uri,
        diagnostics,
        version,
    };
    connection
        .sender
        .send(Message::Notification(lsp_server::Notification {
            method: PublishDiagnostics::METHOD.to_string(),
            params: serde_json::to_value(params)?,
        }))?;
    Ok(())
}

// ---- analysis dispatch by language ----

fn analyze_tokens(doc: &Doc) -> Vec<SemToken> {
    let text = doc.text();
    match doc.lang {
        Lang::Turtle => crate::turtle_semantic_tokens(&text),
        Lang::Sparql => crate::sparql_semantic_tokens(&text),
        Lang::JsonLd => crate::jsonld_semantic_tokens(&text),
    }
}

fn folds(doc: &Doc) -> Vec<FoldingRange> {
    let text = doc.text();
    let ranges = match doc.lang {
        Lang::Turtle => crate::turtle_folds(&text),
        Lang::Sparql => crate::sparql_folds(&text),
        Lang::JsonLd => crate::jsonld_folds(&text),
    };
    ranges
        .into_iter()
        .map(|f| FoldingRange {
            start_line: f.start_line,
            start_character: None,
            end_line: f.end_line,
            end_character: None,
            kind: None,
            collapsed_text: None,
        })
        .collect()
}

fn symbols(doc: &Doc) -> Vec<DocumentSymbol> {
    let text = doc.text();
    let syms = match doc.lang {
        Lang::Turtle => crate::turtle_symbols(&text),
        Lang::Sparql => crate::sparql_symbols(&text),
        Lang::JsonLd => crate::jsonld_symbols(&text),
    };
    syms.into_iter().map(document_symbol).collect()
}

fn completion_item(c: Completion) -> CompletionItem {
    CompletionItem {
        label: c.label,
        kind: Some(match c.kind {
            CompKind::Prefix => CompletionItemKind::MODULE,
            CompKind::LocalName => CompletionItemKind::PROPERTY,
            CompKind::Keyword => CompletionItemKind::KEYWORD,
            CompKind::Variable => CompletionItemKind::VARIABLE,
        }),
        detail: c.detail,
        ..Default::default()
    }
}

fn document_symbol(s: Symbol) -> DocumentSymbol {
    let kind = match s.kind {
        SymKind::Namespace => SymbolKind::NAMESPACE,
        SymKind::Query => SymbolKind::FUNCTION,
        SymKind::Key => SymbolKind::FIELD,
    };
    let range = Range::new(
        Position::new(s.line, s.start),
        Position::new(s.line, s.start + s.len),
    );
    #[allow(deprecated)]
    DocumentSymbol {
        name: s.name,
        detail: None,
        kind,
        tags: None,
        deprecated: None,
        range,
        selection_range: range,
        children: None,
    }
}

/// Convert [`encode`]d relative-delta data to the LSP wire form (`lsp-types`
/// flattens `Vec<SemanticToken>` back to the 5-per-token integer array).
fn to_wire(data: &[u32]) -> Vec<SemanticToken> {
    data.chunks_exact(5)
        .map(|c| SemanticToken {
            delta_line: c[0],
            delta_start: c[1],
            length: c[2],
            token_type: c[3],
            token_modifiers_bitset: c[4],
        })
        .collect()
}

/// The single splice turning `old` into `new` for `semanticTokens/full/delta`.
/// `start`/`delete_count` index the flat integer array (per the spec), aligned
/// down to whole 5-`u32` tokens so the replacement `data` can be carried as
/// `SemanticToken`s.
fn token_edit(old: &[u32], new: &[u32]) -> SemanticTokensEdit {
    let common = old.len().min(new.len());
    let mut prefix = 0;
    while prefix < common && old[prefix] == new[prefix] {
        prefix += 1;
    }
    prefix -= prefix % 5;
    let mut suffix = 0;
    while suffix < common - prefix && old[old.len() - 1 - suffix] == new[new.len() - 1 - suffix] {
        suffix += 1;
    }
    suffix -= suffix % 5;
    SemanticTokensEdit {
        start: prefix as u32,
        delete_count: (old.len() - prefix - suffix) as u32,
        data: Some(to_wire(&new[prefix..new.len() - suffix])),
    }
}

// ---- small JSON-RPC helpers ----

fn de<P: DeserializeOwned>(note: lsp_server::Notification) -> Result<P, serde_json::Error> {
    serde_json::from_value(note.params)
}

/// Deserialize request params, answering `InvalidParams` (and yielding `None`)
/// on malformed input — a bad request from the client must get an error
/// response, never take the whole loop down.
fn valid_params<P: DeserializeOwned>(
    connection: &Connection,
    id: &RequestId,
    params: serde_json::Value,
) -> Result<Option<P>, BoxError> {
    match serde_json::from_value(params) {
        Ok(p) => Ok(Some(p)),
        Err(e) => {
            respond_error(
                connection,
                id.clone(),
                ErrorCode::InvalidParams,
                e.to_string(),
            )?;
            Ok(None)
        }
    }
}

fn respond<T: Serialize>(
    connection: &Connection,
    id: lsp_server::RequestId,
    result: T,
) -> Result<(), BoxError> {
    let resp = Response {
        id,
        result: Some(serde_json::to_value(result)?),
        error: None,
    };
    connection.sender.send(Message::Response(resp))?;
    Ok(())
}

fn respond_error(
    connection: &Connection,
    id: lsp_server::RequestId,
    code: ErrorCode,
    message: String,
) -> Result<(), BoxError> {
    let resp = Response {
        id,
        result: None,
        error: Some(ResponseError {
            code: code as i32,
            message,
            data: None,
        }),
    };
    connection.sender.send(Message::Response(resp))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use lsp_server::{Notification, Request, RequestId};
    use serde_json::json;

    /// Drive the loop over an in-memory connection: open a Turtle doc, request
    /// full semantic tokens, and check we get a non-empty token array back.
    #[test]
    fn semantic_tokens_over_memory_connection() {
        let (server, client) = Connection::memory();
        let handle = std::thread::spawn(move || main_loop(&server));

        client
            .sender
            .send(Message::Notification(Notification {
                method: DidOpenTextDocument::METHOD.to_string(),
                params: json!({
                    "textDocument": {
                        "uri": "file:///t.ttl",
                        "languageId": "turtle",
                        "version": 1,
                        "text": "ex:s ex:p ex:o ."
                    }
                }),
            }))
            .unwrap();

        client
            .sender
            .send(Message::Request(Request {
                id: RequestId::from(1),
                method: SemanticTokensFullRequest::METHOD.to_string(),
                params: json!({ "textDocument": { "uri": "file:///t.ttl" } }),
            }))
            .unwrap();

        let resp = response(&client);
        let value = resp.result.expect("tokens result");
        let data = value["data"].as_array().expect("data array");
        assert!(!data.is_empty(), "expected semantic tokens");
        assert_eq!(data.len() % 5, 0);

        // Shut the loop down cleanly.
        client
            .sender
            .send(Message::Request(Request {
                id: RequestId::from(2),
                method: "shutdown".to_string(),
                params: json!(null),
            }))
            .unwrap();
        let _ = client.receiver.recv(); // shutdown response
        client
            .sender
            .send(Message::Notification(Notification {
                method: "exit".to_string(),
                params: json!(null),
            }))
            .unwrap();
        handle.join().unwrap().unwrap();
    }

    #[test]
    fn document_symbols_and_folding_dispatch() {
        let (server, client) = Connection::memory();
        let handle = std::thread::spawn(move || main_loop(&server));

        client
            .sender
            .send(Message::Notification(Notification {
                method: DidOpenTextDocument::METHOD.to_string(),
                params: json!({
                    "textDocument": {
                        "uri": "file:///q.rq",
                        "languageId": "sparql",
                        "version": 1,
                        "text": "PREFIX ex: <http://e/>\nSELECT * WHERE {\n  ?s ex:p ?o\n}"
                    }
                }),
            }))
            .unwrap();

        client
            .sender
            .send(Message::Request(Request {
                id: RequestId::from(1),
                method: DocumentSymbolRequest::METHOD.to_string(),
                params: json!({ "textDocument": { "uri": "file:///q.rq" } }),
            }))
            .unwrap();
        let syms = response(&client).result.unwrap();
        let arr = syms.as_array().unwrap();
        assert!(arr.iter().any(|s| s["name"] == "SELECT"));
        assert!(arr.iter().any(|s| s["name"] == "ex:"));

        client
            .sender
            .send(Message::Request(Request {
                id: RequestId::from(2),
                method: FoldingRangeRequest::METHOD.to_string(),
                params: json!({ "textDocument": { "uri": "file:///q.rq" } }),
            }))
            .unwrap();
        let folds = response(&client).result.unwrap();
        assert!(!folds.as_array().unwrap().is_empty());

        client
            .sender
            .send(Message::Request(Request {
                id: RequestId::from(3),
                method: "shutdown".to_string(),
                params: json!(null),
            }))
            .unwrap();
        let _ = client.receiver.recv();
        client
            .sender
            .send(Message::Notification(Notification {
                method: "exit".to_string(),
                params: json!(null),
            }))
            .unwrap();
        handle.join().unwrap().unwrap();
    }

    fn open(client: &Connection, uri: &str, language_id: &str, text: &str) {
        client
            .sender
            .send(Message::Notification(Notification {
                method: DidOpenTextDocument::METHOD.to_string(),
                params: json!({
                    "textDocument": {
                        "uri": uri, "languageId": language_id, "version": 1, "text": text
                    }
                }),
            }))
            .unwrap();
    }

    fn request(client: &Connection, id: i32, method: &str, params: serde_json::Value) {
        client
            .sender
            .send(Message::Request(Request {
                id: RequestId::from(id),
                method: method.to_string(),
                params,
            }))
            .unwrap();
    }

    /// Next response, skipping server-initiated notifications (diagnostics).
    fn response(client: &Connection) -> lsp_server::Response {
        loop {
            match client.receiver.recv().unwrap() {
                Message::Response(r) => return r,
                Message::Notification(_) => continue,
                other => panic!("expected response, got {other:?}"),
            }
        }
    }

    /// Next notification with the given method, skipping everything else.
    fn notification(client: &Connection, method: &str) -> serde_json::Value {
        loop {
            match client.receiver.recv().unwrap() {
                Message::Notification(n) if n.method == method => return n.params,
                Message::Notification(_) => continue,
                other => panic!("expected notification {method}, got {other:?}"),
            }
        }
    }

    fn shut_down(client: Connection, handle: std::thread::JoinHandle<Result<(), BoxError>>) {
        request(&client, 999, "shutdown", json!(null));
        let _ = client.receiver.recv();
        client
            .sender
            .send(Message::Notification(Notification {
                method: "exit".to_string(),
                params: json!(null),
            }))
            .unwrap();
        handle.join().unwrap().unwrap();
    }

    /// A request with malformed params must get `InvalidParams` back — and the
    /// loop must keep serving afterwards, not die.
    #[test]
    fn malformed_params_answer_invalid_params_and_loop_survives() {
        let (server, client) = Connection::memory();
        let handle = std::thread::spawn(move || main_loop(&server));
        open(&client, "file:///t.ttl", "turtle", "ex:s ex:p ex:o .");

        // Missing `textDocument` entirely.
        request(&client, 1, SemanticTokensFullRequest::METHOD, json!({}));
        let err = response(&client).error.expect("expected an error response");
        assert_eq!(err.code, ErrorCode::InvalidParams as i32);

        // The server is still alive and answers a valid request.
        request(
            &client,
            2,
            SemanticTokensFullRequest::METHOD,
            json!({ "textDocument": { "uri": "file:///t.ttl" } }),
        );
        let ok = response(&client).result.expect("tokens after bad request");
        assert!(!ok["data"].as_array().unwrap().is_empty());

        shut_down(client, handle);
    }

    /// full → didChange → full/delta returns a splice against the previous
    /// result id; an unknown id falls back to full tokens.
    #[test]
    fn semantic_tokens_delta_roundtrip() {
        let (server, client) = Connection::memory();
        let handle = std::thread::spawn(move || main_loop(&server));
        open(&client, "file:///t.ttl", "turtle", "ex:s ex:p ex:o .");

        request(
            &client,
            1,
            SemanticTokensFullRequest::METHOD,
            json!({ "textDocument": { "uri": "file:///t.ttl" } }),
        );
        let full = response(&client).result.unwrap();
        let rid = full["resultId"].as_str().expect("full carries a resultId");
        let full_len = full["data"].as_array().unwrap().len();

        // Append a second statement (full-replace change).
        client
            .sender
            .send(Message::Notification(Notification {
                method: DidChangeTextDocument::METHOD.to_string(),
                params: json!({
                    "textDocument": { "uri": "file:///t.ttl", "version": 2 },
                    "contentChanges": [
                        { "text": "ex:s ex:p ex:o .\nex:a ex:b ex:c ." }
                    ]
                }),
            }))
            .unwrap();

        request(
            &client,
            2,
            SemanticTokensFullDeltaRequest::METHOD,
            json!({
                "textDocument": { "uri": "file:///t.ttl" },
                "previousResultId": rid
            }),
        );
        let delta = response(&client).result.unwrap();
        let edits = delta["edits"].as_array().expect("delta answers with edits");
        assert_eq!(edits.len(), 1);
        // The old prefix is untouched: the splice appends at (or before) the
        // old end and only inserts whole tokens.
        let edit = &edits[0];
        assert!(edit["start"].as_u64().unwrap() as usize <= full_len);
        assert_eq!(edit["data"].as_array().unwrap().len() % 5, 0);
        let new_rid = delta["resultId"].as_str().unwrap().to_string();
        assert_ne!(new_rid, rid);

        // Unknown baseline: full tokens, not a delta.
        request(
            &client,
            3,
            SemanticTokensFullDeltaRequest::METHOD,
            json!({
                "textDocument": { "uri": "file:///t.ttl" },
                "previousResultId": "no-such-id"
            }),
        );
        let fallback = response(&client).result.unwrap();
        assert!(
            fallback["data"].is_array(),
            "expected full fallback: {fallback}"
        );

        shut_down(client, handle);
    }

    /// Diagnostics are published on open (broken doc → errors, with the doc
    /// version), re-published on change (fixed doc → empty), and cleared on
    /// close.
    #[test]
    fn diagnostics_published_on_open_change_and_close() {
        let (server, client) = Connection::memory();
        let handle = std::thread::spawn(move || main_loop(&server));
        open(
            &client,
            "file:///d.ttl",
            "turtle",
            "@prefix ex: <http://x/> .\nex:s ex:p BROKEN HERE .",
        );

        let p = notification(&client, PublishDiagnostics::METHOD);
        assert_eq!(p["version"], 1);
        let diags = p["diagnostics"].as_array().unwrap();
        assert!(!diags.is_empty(), "expected errors on open: {p}");
        assert_eq!(diags[0]["severity"], 1); // Error
        assert_eq!(diags[0]["source"], "graphy-lsp");
        // The range lands on the broken statement's line, not at 0:0.
        assert_eq!(diags[0]["range"]["start"]["line"], 1);

        client
            .sender
            .send(Message::Notification(Notification {
                method: DidChangeTextDocument::METHOD.to_string(),
                params: json!({
                    "textDocument": { "uri": "file:///d.ttl", "version": 2 },
                    "contentChanges": [
                        { "text": "@prefix ex: <http://x/> .\nex:s ex:p ex:o ." }
                    ]
                }),
            }))
            .unwrap();
        let p = notification(&client, PublishDiagnostics::METHOD);
        assert_eq!(p["version"], 2);
        assert!(p["diagnostics"].as_array().unwrap().is_empty(), "{p}");

        client
            .sender
            .send(Message::Notification(Notification {
                method: DidCloseTextDocument::METHOD.to_string(),
                params: json!({ "textDocument": { "uri": "file:///d.ttl" } }),
            }))
            .unwrap();
        let p = notification(&client, PublishDiagnostics::METHOD);
        assert!(p["diagnostics"].as_array().unwrap().is_empty());

        shut_down(client, handle);
    }

    /// Completion dispatch: local names under a typed prefix, at a cursor
    /// position expressed in (line, UTF-16 character).
    #[test]
    fn completion_dispatch() {
        let (server, client) = Connection::memory();
        let handle = std::thread::spawn(move || main_loop(&server));
        open(
            &client,
            "file:///c.ttl",
            "turtle",
            "@prefix ex: <http://e/> .\nex:alpha ex:beta ex:gamma .\nex:s ex:p ex:",
        );

        request(
            &client,
            1,
            CompletionRequest::METHOD,
            json!({
                "textDocument": { "uri": "file:///c.ttl" },
                "position": { "line": 2, "character": 13 }
            }),
        );
        let items = response(&client).result.unwrap();
        let labels: Vec<&str> = items
            .as_array()
            .unwrap()
            .iter()
            .map(|i| i["label"].as_str().unwrap())
            .collect();
        assert!(
            labels.contains(&"alpha") && labels.contains(&"gamma"),
            "{labels:?}"
        );

        shut_down(client, handle);
    }

    /// codeAction dispatch: the unused-prefix warning yields a quickfix whose
    /// WorkspaceEdit deletes the declaration line.
    #[test]
    fn code_action_dispatch() {
        let (server, client) = Connection::memory();
        let handle = std::thread::spawn(move || main_loop(&server));
        open(
            &client,
            "file:///a.ttl",
            "turtle",
            "@prefix ex: <http://x/> .\n@prefix u: <http://u/> .\nex:s ex:p ex:o .",
        );

        request(
            &client,
            1,
            CodeActionRequest::METHOD,
            json!({
                "textDocument": { "uri": "file:///a.ttl" },
                "range": {
                    "start": { "line": 1, "character": 0 },
                    "end": { "line": 1, "character": 10 }
                },
                "context": { "diagnostics": [], "only": ["quickfix"] }
            }),
        );
        let actions = response(&client).result.unwrap();
        let arr = actions.as_array().unwrap();
        assert_eq!(arr.len(), 1, "{actions}");
        assert!(arr[0]["title"].as_str().unwrap().contains("u:"));
        assert_eq!(arr[0]["kind"], "quickfix");
        let edits = &arr[0]["edit"]["changes"]["file:///a.ttl"];
        assert_eq!(edits[0]["newText"], "");
        assert_eq!(edits[0]["range"]["start"]["line"], 1);
        assert_eq!(edits[0]["range"]["end"]["line"], 2);

        // A range not covering the warning yields no actions.
        request(
            &client,
            2,
            CodeActionRequest::METHOD,
            json!({
                "textDocument": { "uri": "file:///a.ttl" },
                "range": {
                    "start": { "line": 2, "character": 0 },
                    "end": { "line": 2, "character": 4 }
                },
                "context": { "diagnostics": [], "only": ["quickfix"] }
            }),
        );
        let actions = response(&client).result.unwrap();
        assert!(actions.as_array().unwrap().is_empty(), "{actions}");

        shut_down(client, handle);
    }

    /// Formatting dispatch: the graphy pretty-print pipeline replaces the
    /// whole document; broken docs get null (never mangled).
    #[test]
    fn formatting_dispatch() {
        let (server, client) = Connection::memory();
        let handle = std::thread::spawn(move || main_loop(&server));
        open(
            &client,
            "file:///f.ttl",
            "turtle",
            "@prefix ex: <http://e/> .\nex:s ex:p ex:o .\nex:t ex:q 1 .\nex:s ex:p2 2 .",
        );

        request(
            &client,
            1,
            Formatting::METHOD,
            json!({
                "textDocument": { "uri": "file:///f.ttl" },
                "options": { "tabSize": 4, "insertSpaces": false }
            }),
        );
        let edits = response(&client).result.unwrap();
        let arr = edits.as_array().expect("edit array");
        assert_eq!(arr.len(), 1);
        let new_text = arr[0]["newText"].as_str().unwrap();
        // Regrouped: ex:s owns one stanza even though its statements were
        // split around ex:t in the source.
        assert_eq!(new_text.matches("ex:s ").count(), 1, "{new_text}");
        assert_eq!(arr[0]["range"]["start"], json!({"line": 0, "character": 0}));

        // Broken doc → null (refuse to format), with the reason logged to the
        // client's output channel so the refusal isn't a silent mystery.
        open(&client, "file:///b.ttl", "turtle", "ex:s ex:p BROKEN .");
        request(
            &client,
            2,
            Formatting::METHOD,
            json!({
                "textDocument": { "uri": "file:///b.ttl" },
                "options": { "tabSize": 4, "insertSpaces": false }
            }),
        );
        let logm = notification(&client, "window/logMessage");
        assert!(
            logm["message"].as_str().unwrap().contains("format refused"),
            "{logm}"
        );
        assert_eq!(logm["type"], 2); // Warning
        assert!(response(&client).result.unwrap().is_null());

        // The same pretty-print is offered as a source action.
        request(
            &client,
            3,
            CodeActionRequest::METHOD,
            json!({
                "textDocument": { "uri": "file:///f.ttl" },
                "range": {
                    "start": { "line": 0, "character": 0 },
                    "end": { "line": 0, "character": 0 }
                },
                "context": { "diagnostics": [], "only": ["source"] }
            }),
        );
        let actions = response(&client).result.unwrap();
        let arr = actions.as_array().unwrap();
        assert_eq!(arr.len(), 1, "{actions}");
        assert_eq!(arr[0]["kind"], "source.prettyPrint.graphy");
        assert!(arr[0]["title"].as_str().unwrap().contains("Pretty-print"));

        shut_down(client, handle);
    }

    /// Apply LSP TextEdits (JSON form) to an ASCII source string.
    fn apply_edits(src: &str, edits: &serde_json::Value) -> String {
        let line_start =
            |line: usize| -> usize { src.split_inclusive('\n').take(line).map(str::len).sum() };
        let byte_of = |p: &serde_json::Value| -> usize {
            line_start(p["line"].as_u64().unwrap() as usize)
                + p["character"].as_u64().unwrap() as usize
        };
        let mut es: Vec<(usize, usize, String)> = edits
            .as_array()
            .unwrap()
            .iter()
            .map(|e| {
                (
                    byte_of(&e["range"]["start"]),
                    byte_of(&e["range"]["end"]),
                    e["newText"].as_str().unwrap().to_string(),
                )
            })
            .collect();
        es.sort_by_key(|e| (e.0, e.1));
        let mut out = src.to_string();
        for (s, e, t) in es.iter().rev() {
            out.replace_range(*s..*e, t);
        }
        out
    }

    /// `source.removeUnusedImports` deletes every unused declaration at once;
    /// `source.fixAll` additionally applies the remaining fixes and leaves a
    /// document that re-diagnoses clean.
    #[test]
    fn source_aggregate_actions() {
        let (server, client) = Connection::memory();
        let handle = std::thread::spawn(move || main_loop(&server));
        let src = "@prefix ex: <http://x/> .\n\
                   @prefix u1: <http://u1/> .\n\
                   @prefix u2: <http://u2/> .\n\
                   ex:s foaf:knows ex:o .";
        open(&client, "file:///agg.ttl", "turtle", src);

        request(
            &client,
            1,
            CodeActionRequest::METHOD,
            json!({
                "textDocument": { "uri": "file:///agg.ttl" },
                "range": {
                    "start": { "line": 0, "character": 0 },
                    "end": { "line": 0, "character": 0 }
                },
                "context": { "diagnostics": [], "only": ["source.removeUnusedImports"] }
            }),
        );
        let actions = response(&client).result.unwrap();
        let arr = actions.as_array().unwrap();
        assert_eq!(arr.len(), 1, "{actions}");
        assert_eq!(arr[0]["kind"], "source.removeUnusedImports");
        let fixed = apply_edits(src, &arr[0]["edit"]["changes"]["file:///agg.ttl"]);
        assert_eq!(fixed, "@prefix ex: <http://x/> .\nex:s foaf:knows ex:o .");

        request(
            &client,
            2,
            CodeActionRequest::METHOD,
            json!({
                "textDocument": { "uri": "file:///agg.ttl" },
                "range": {
                    "start": { "line": 0, "character": 0 },
                    "end": { "line": 0, "character": 0 }
                },
                "context": { "diagnostics": [], "only": ["source.fixAll"] }
            }),
        );
        let actions = response(&client).result.unwrap();
        let arr = actions.as_array().unwrap();
        assert_eq!(arr.len(), 1, "{actions}");
        assert_eq!(arr[0]["title"], "Fix all auto-fixable problems (3)");
        let fixed = apply_edits(src, &arr[0]["edit"]["changes"]["file:///agg.ttl"]);
        assert!(fixed.contains("@prefix foaf:"), "{fixed}");
        assert!(!fixed.contains("u1:") && !fixed.contains("u2:"), "{fixed}");
        // The fixed document re-diagnoses completely clean.
        assert!(
            crate::turtle_diagnostics(&fixed, None).is_empty(),
            "{fixed}"
        );

        // Lightbulb menu (only=[quickfix]) on the u1 declaration: the
        // individual fix PLUS quickfix twins of both aggregates.
        request(
            &client,
            3,
            CodeActionRequest::METHOD,
            json!({
                "textDocument": { "uri": "file:///agg.ttl" },
                "range": {
                    "start": { "line": 1, "character": 9 },
                    "end": { "line": 1, "character": 9 }
                },
                "context": { "diagnostics": [], "only": ["quickfix"] }
            }),
        );
        let actions = response(&client).result.unwrap();
        let titles: Vec<String> = actions
            .as_array()
            .unwrap()
            .iter()
            .map(|a| {
                assert_eq!(a["kind"], "quickfix", "{a}");
                a["title"].as_str().unwrap().to_string()
            })
            .collect();
        assert_eq!(
            titles,
            vec![
                "Remove unused prefix declaration `u1:`",
                "Remove all unused prefix declarations (2)",
                "Fix all auto-fixable problems (3)",
            ],
            "{actions}"
        );

        shut_down(client, handle);
    }

    #[test]
    fn token_edit_is_a_token_aligned_splice() {
        // One token changes in the middle.
        let old = [
            0, 0, 3, 1, 0, /**/ 0, 4, 2, 2, 0, /**/ 1, 0, 5, 3, 0,
        ];
        let new = [
            0, 0, 3, 1, 0, /**/ 0, 4, 9, 2, 0, /**/ 1, 0, 5, 3, 0,
        ];
        let e = token_edit(&old, &new);
        assert_eq!(e.start % 5, 0);
        assert_eq!(e.delete_count % 5, 0);
        // Splicing old with the edit reproduces new.
        let mut spliced: Vec<u32> = old[..e.start as usize].to_vec();
        for t in e.data.as_deref().unwrap() {
            spliced.extend_from_slice(&[
                t.delta_line,
                t.delta_start,
                t.length,
                t.token_type,
                t.token_modifiers_bitset,
            ]);
        }
        spliced.extend_from_slice(&old[(e.start + e.delete_count) as usize..]);
        assert_eq!(spliced, new);

        // Identical inputs: an empty splice.
        let e = token_edit(&old, &old);
        assert_eq!(e.delete_count, 0);
        assert!(e.data.as_deref().unwrap().is_empty());

        // Grow-from-empty and shrink-to-empty stay well-formed.
        let e = token_edit(&[], &new);
        assert_eq!((e.start, e.delete_count), (0, 0));
        assert_eq!(e.data.as_deref().unwrap().len(), 3);
        let e = token_edit(&new, &[]);
        assert_eq!((e.start, e.delete_count), (0, new.len() as u32));
    }
}
