# Language server

`graphy-lsp` is a synchronous stdio language server built with `lsp-server` and `lsp-types`. It supports N-Triples, N-Quads, Turtle, TriG, SPARQL Query/Update, and lexical JSON-LD assistance.

## Capabilities

- incremental text synchronization with correct UTF-16 position conversion;
- full, range, and delta semantic tokens;
- syntax diagnostics and selected quick fixes;
- completion and hover;
- document symbols and folding ranges;
- whole-document formatting.

Tokenization is resilient: malformed or incomplete input still produces bounded tokens and diagnostics. Turtle-family documents reuse `graphy-turtle`; SPARQL documents reuse resilient syntax parsing. JSON-LD support classifies JSON structure and known keywords but does not resolve contexts or provide full RDF semantics.

The current server does not advertise definition, reference, rename, range-formatting, or code-lens capabilities. It keeps open-document state in memory and has no cross-file project index.

## Running

```sh
cargo run -p graphy-lsp
```

The VS Code client under [../editors/vscode](../editors/vscode) starts this binary and registers the supported language identifiers. See its README for development and packaging instructions.

## Source layout

The crate separates document storage and line indexing from semantic tokens, diagnostics, completion, hover, folding, formatting, symbols, SPARQL analysis, JSON-LD tokenization, and protocol dispatch. The server is testable through an in-memory LSP connection as well as stdio.
