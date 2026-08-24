# graphy-rs

`graphy-rs` is an experimental RDF quad store and SPARQL engine written in
Rust. It combines immutable compressed base segments with a write-ahead-logged
delta, exposes native and WebAssembly APIs, and includes command-line, HTTP,
and language-server front ends.

The workspace is pre-release (`0.0.3`) and under active development. The
implemented conformance boundary is recorded in
[docs/12-conformance.md](docs/12-conformance.md); roadmap items are not treated
as shipped features.

## Implemented today

- RDF 1.1 plus the covered RDF 1.2 Turtle, TriG, N-Triples, and N-Quads syntax.
- SPARQL 1.0/1.1 query and update, plus the covered SPARQL 1.2 syntax and
  evaluation cases. Federated `SERVICE` execution is intentionally absent.
- A compressed, memory-mappable base-segment format with `compact`, `balanced`,
  and `covering` index profiles.
- WAL-backed updates, snapshots, group commit, background compaction, recovery,
  and deterministic segment rebuilds.
- Columnar query execution with scan, bind-join, hash-join, path, grouping,
  sorting, projection, distinct, and related operators. Memory budgets fail
  cleanly when exceeded; spill-to-disk query operators are not implemented.
- CLI commands for loading, verifying, exporting, compacting, querying, serving,
  and running the implemented in-memory pipeline operators.
- A buffered SPARQL/GSP HTTP service with content negotiation, deadlines,
  ETags, and a read-only mode.
- RDF/XML and JSON-LD interchange codecs in `graphy-interop`.
- A browser-targeted `graphy-wasm` store and an RDF/SPARQL language server.

## Quick start

```sh
cargo build --release -p graphy-cli

target/release/graphy load ./store ./data.ttl
target/release/graphy verify ./store
target/release/graphy query ./store -e 'SELECT * WHERE { ?s ?p ?o } LIMIT 10'
target/release/graphy serve ./store --bind 127.0.0.1:7878
```

Build the browser package with the same `wasm-pack` target used by releases:

```sh
npx --yes wasm-pack@0.15.0 build --release --target web \
  --out-dir pkg-web crates/graphy-wasm -- --locked
```

Run `graphy --help` or `graphy help <command>` for the authoritative CLI
surface. [DEMO.md](DEMO.md) contains a fuller walkthrough.

## Design

The store has two layers:

1. An immutable base segment stores concise term dictionaries, succinct triple
   indexes, graph membership, and statistics.
2. A mutable delta stores additions and tombstones across the available scan
   orders. Commits are recorded in the WAL before publication. A merger folds a
   frozen delta into a new base generation and atomically publishes it.

Queries pin a `(base generation, delta epoch)` snapshot. Storage exposes
position-independent `TermId` bindings even though segment columns use compact
position-local ID spaces.

See [ARCHITECTURE.md](ARCHITECTURE.md) for the system overview and
[docs/08-segment-format.md](docs/08-segment-format.md) for the normative on-disk
format.

## Workspace

| crate | purpose |
|---|---|
| `graphy-core` | RDF terms, concise encoding, IRIs, and inline term IDs |
| `graphy-succinct` | bitvectors, packed integers, PFC, wavelet matrices, external sort |
| `graphy-turtle` | Turtle/TriG/N-Triples/N-Quads parsers and writers |
| `graphy-hdt` | HDT and HDTQ import/export |
| `graphy-interop` | JSON-LD and RDF/XML interchange |
| `graphy-sparql-syntax` | SPARQL lexer, parser, printer, and substitution |
| `graphy-algebra` | syntax-to-algebra translation and algebra rewrites |
| `graphy-store` | segments, delta, WAL, snapshots, merge, and recovery |
| `graphy-engine` | reference and columnar SPARQL evaluators plus update execution |
| `graphy-protocol` | transport-independent protocol and result helpers |
| `graphy-server` | axum HTTP service |
| `graphy-pipe` | in-memory CLI pipeline |
| `graphy-cli` | `graphy` command-line program |
| `graphy-lsp` | RDF/SPARQL language server |
| `graphy-wasm` | browser-facing store and query bindings |

## Documentation

- [ARCHITECTURE.md](ARCHITECTURE.md) — current system architecture and boundaries.
- [IMPLEMENTATION_PLAN.md](IMPLEMENTATION_PLAN.md) — concise current roadmap.
- [DEMO.md](DEMO.md) — CLI and HTTP walkthrough.
- [BENCHMARKS.md](BENCHMARKS.md) — current reproducible benchmark baselines.
- [CONTRIBUTING.md](CONTRIBUTING.md) — development checks and contribution expectations.
- [SECURITY.md](SECURITY.md) — vulnerability reporting and deployment cautions.
- [docs/01-data-model.md](docs/01-data-model.md) — RDF terms, concise bytes, IDs, and dictionaries.
- [docs/02-storage-engine.md](docs/02-storage-engine.md) — base segments and index profiles.
- [docs/03-turtle-parser.md](docs/03-turtle-parser.md) — RDF text parsers and writers.
- [docs/04-sparql-parser.md](docs/04-sparql-parser.md) — SPARQL syntax, printing, and algebra.
- [docs/05-query-engine.md](docs/05-query-engine.md) — implemented evaluator and planner.
- [docs/06-sparql-service.md](docs/06-sparql-service.md) — implemented HTTP surface.
- [docs/07-writes-and-concurrency.md](docs/07-writes-and-concurrency.md) — writes, snapshots, WAL, and merge.
- [docs/08-segment-format.md](docs/08-segment-format.md) — normative base-segment format v2.
- [docs/09-cli-pipeline.md](docs/09-cli-pipeline.md) — implemented pipeline commands.
- [docs/10-lsp.md](docs/10-lsp.md) — implemented language-server behavior.
- [docs/11-wasm.md](docs/11-wasm.md) — implemented WebAssembly API and deployment modes.
- [docs/12-conformance.md](docs/12-conformance.md) — executable W3C conformance boundary.
- [docs/13-oracle-corpora.md](docs/13-oracle-corpora.md) — independent oracle suites.

## Formats

The CLI loads Turtle, TriG, N-Triples, N-Quads, HDT, and HDTQ. It exports
N-Quads, HDT, and HDTQ. `graphy-interop` supplies JSON-LD and RDF/XML library
codecs; the optional native `LOAD` capability uses them when content negotiation
selects those formats.

## Releases

Version tags publish native archives containing `graphy` and `graphy-lsp` for
Linux, macOS, and Windows on x86-64 and ARM64. They also publish a
`wasm-pack --target web` browser archive whose `pkg-web` directory contains the
JavaScript loader, TypeScript declarations, and WebAssembly module. Standalone
`graphy` CLI modules are published for `wasm32-wasip1`, `wasm32-wasip2`, and
`wasm32-wasip3`; listening-server and outbound-network capabilities are not
available in those builds. Each GitHub Release includes a `SHA256SUMS` file. A
release tag must exactly match the workspace version, for example `v0.0.3` for
version `0.0.3`.

## License

Licensed under the [Apache License 2.0](LICENSE).
