# graphy-rs roadmap

This is a current roadmap, not a development log. Completed work is summarized here only when it clarifies what remains; performance claims belong in [BENCHMARKS.md](BENCHMARKS.md).

## Current state

- Compact RDF term and triple representations, including RDF-star quoted triples.
- Succinct indexes, bit vectors, wavelet matrices, and compressed integer encodings.
- Streaming Turtle, TriG, N-Triples, N-Quads, RDF/XML, and JSON-LD parsers with interoperability tests.
- A versioned segment format with checksums, validation, heap and memory-mapped readers, snapshots, and a write-ahead log.
- SPARQL 1.1 parsing, algebra, updates, property paths, aggregates, and RDF-star syntax.
- A reference evaluator and an indexed evaluator with scans, bind joins, hash joins, grouping, sorting, projection, distinct, and slicing.
- A CLI, buffered HTTP service, composable pipeline runner, language server, and WebAssembly bindings.
- W3C-derived syntax/evaluation suites plus Oxigraph and RDF4J differential checks.

## Before public v0.1

### Public API and packaging

- Stabilize the published crate set and semantic-versioning policy.
- Add small, tested examples for each public crate.
- Publish the WebAssembly package with generated TypeScript declarations.
- Package the VS Code extension without repository-local symlink assumptions.

### HTTP service

- Stream large request and response bodies with explicit backpressure.
- Add admission control and configurable resource limits.
- Add structured tracing and operational metrics.
- Document and test authentication, CORS, and compression integration points.
- Add SPARQL Protocol and Graph Store Protocol conformance coverage and sustained-load tests.

### Query execution

- Add disk-backed spill paths for memory-bound operators.
- Expand differential result comparison and plan regression coverage.
- Profile and improve the slow WatDiv and SP2Bench query shapes.
- Evaluate merge joins or worst-case-optimal joins only where measurements justify their complexity.

### Storage and operations

- Measure load and compaction on independently sourced real-world corpora under explicit memory budgets.
- Run long-duration snapshot, recovery, update, and compaction tests.
- Validate HDT/HDTQ output with independent implementations.
- Decide and document policies for pre-faulting, memory locking, and persistent storage directories.

### CLI pipeline

The implemented verbs are `read`, `scan`, `scribe`, `write`, `skip`, `head`, `tail`, `tree`, `concat`, `merge`, `count`, and `distinct`.

Potential additions include filtering, term replacement, RDFC normalization, transforms, store integration, HDT conversion, and richer writer controls. They should not appear in examples until implemented.

### Language server

The server currently provides resilient tokenization, incremental synchronization, diagnostics, completion, hover, document symbols, folding ranges, formatting, semantic-token deltas, and quick fixes.

Remaining work includes definition and rename support, semantic JSON-LD assistance, CodeLens features, browser packaging, and sustained editor-session testing.

## Release gates

- `cargo fmt --check`, workspace Clippy, and the full test suite pass.
- W3C-derived suites and configured interoperability oracles pass with documented exclusions.
- Segment verification rejects corrupted or inconsistent data.
- Benchmark tables are reproducible from documented commands and hardware.
- Public docs describe shipped behavior and clearly label planned behavior.
- Licensing, contribution, and security-reporting instructions are present.

## Deferred beyond v0.1

- Replication and sharding.
- Inference and rule engines.
- Full-text and geospatial indexes.
- Federated `SERVICE` execution.
- Built-in multi-tenant access control.
- Query compilation.
- Write-biased or stacked-delta storage designs.
- Browser-side lazy paging of persistent segments.
