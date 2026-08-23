# graphy-rs architecture

This document describes the current implementation. Future work is kept in
[IMPLEMENTATION_PLAN.md](IMPLEMENTATION_PLAN.md), not mixed into the component
descriptions below.

## System overview

```text
                         graphy-cli / graphy-server / graphy-wasm
                                         |
               +-------------------------+-------------------------+
               |                         |                         |
         graphy-turtle          graphy-sparql-syntax        graphy-interop
               |                         |                         |
               |                  graphy-algebra                    |
               |                         |                         |
               +------------------- graphy-engine ----------------+
                                         |
                                    graphy-store
                                  /              \
                       immutable base          mutable delta
                       compressed indexes      WAL + ordered maps
                                  \              /
                                   snapshot reads
```

The HTTP layer is split between transport-independent helpers in
`graphy-protocol` and the axum router in `graphy-server`. The language server and
CLI pipeline reuse the syntax crates without depending on the store where that
is unnecessary.

## Crate boundaries

- `graphy-core` owns RDF term identity, concise bytes, IRI handling, and
  `TermId`.
- `graphy-succinct` owns compact primitives and external sorting.
- `graphy-turtle`, `graphy-sparql-syntax`, and `graphy-algebra` do not depend on
  the store.
- `graphy-store` exposes ID-space scans, counts, snapshots, writes, and merge;
  it does not interpret SPARQL.
- `graphy-engine` binds SPARQL algebra to `graphy-store`.
- `graphy-protocol` owns result encodings and HTTP-neutral protocol rules.
- `graphy-server` confines the native async HTTP runtime and optional outbound
  client.

Outbound network access is deny-by-default. Native SPARQL `LOAD` over HTTP(S)
requires both the non-default `outbound-http` Cargo feature and runtime
`graphy serve --allow-network`. WASM does not compile the outbound client.

## Data model

An RDF dataset contains a default graph and zero or more named graphs. Public
APIs use validated concise term bytes and snapshot-relative `TermId` values.

Base-segment columns use dense, position-local IDs for compression. At the
`Snapshot` boundary, `term_id` and `column` translate between those columns and
a position-independent identity, so joins, repeated variables, and `sameTerm`
remain correct when one RDF term occurs in several positions.

Canonical numeric, boolean, and temporal lexical forms may be encoded directly
in a 64-bit `TermId`. Non-canonical lexical forms remain dictionary terms so RDF
term identity is preserved.

Full details are in [docs/01-data-model.md](docs/01-data-model.md).

## Base segments

A base segment is an immutable directory whose files share a versioned,
checksummed component envelope. The current format is v2.

- PFC dictionaries store shared, subject, predicate, object, and graph terms.
- Rebuildable hash sidecars accelerate term-to-ordinal lookup.
- BitmapTriples structures provide materialized scan orders.
- The compact profile uses FoQ wavelet accessors instead of secondary
  BitmapTriples orders.
- Quad datasets add graph membership and triple-to-graph components.
- Predicate and characteristic-set statistics support planning.

`Segment::open` and `Store::open` use fully verified heap mode by default.
`OpenMode::Mmap` is an explicit zero-copy alternative that validates headers
and structure without faulting every payload page merely to recompute digests;
`graphy verify` performs the full integrity sweep.

Index profiles are:

| profile | available structures |
|---|---|
| `compact` | SPO plus FoQ P/O accessors |
| `balanced` | SPO, POS, OSP |
| `covering` | all six triple orders |

See [docs/02-storage-engine.md](docs/02-storage-engine.md) and
[docs/08-segment-format.md](docs/08-segment-format.md).

## Writes and snapshots

The mutable delta maintains ordered maps for the configured scan orders. Each
key carries epoch-stamped add/tombstone events. A short-lived `RwLock` protects
collection from the delta maps; scans do not hold the lock while returning
batches to callers.

The write path is:

1. Resolve and validate delete/add sets against the current snapshot.
2. Append the transaction to the WAL.
3. Flush or fsync according to the requested durability.
4. Apply delta events at the next epoch.
5. Publish the new snapshot.

Concurrent callers participate in group commit. A snapshot pins one immutable
base generation and one delta epoch, so older snapshots continue to observe
their original state.

A merger freezes an epoch, builds a new base through the same deterministic
builders used by bulk load, remaps the active suffix, stages a rotated WAL,
atomically flips `CURRENT`, and publishes the new generation. Old generations
are reclaimed after their last snapshot drops.

See [docs/07-writes-and-concurrency.md](docs/07-writes-and-concurrency.md).

## Query path

1. `graphy-sparql-syntax` parses a request and retains source spans.
2. `graphy-algebra` translates it according to the SPARQL algebra rules and
   applies cost-independent rewrites.
3. `graphy-engine` plans against exact leaf counts from a pinned snapshot.
4. The columnar evaluator executes batches of `TermId` bindings.
5. The CLI, protocol, or WASM layer serializes the result.

The physical evaluator currently provides scans, bind joins, hash joins, left
joins, minus, property paths, filters, extension, union, graph scope, values,
grouping, sorting, projection, distinct, and slicing. Driving-scan bind-join
chains can run as parallel morsels. Other shapes execute on the ordinary
operator tree or fall back to the reference evaluator where appropriate.

Implemented controls include cancellation, deadlines, a materialized-row memory
budget, plan caching keyed by snapshot identity, and EXPLAIN/ANALYZE. Query
operators do not yet spill to disk, and there is no merge-join or
worst-case-optimal-join operator.

Federated `SERVICE` is parsed, translated, and printed, but physical execution
returns an explicit unsupported error.

See [docs/04-sparql-parser.md](docs/04-sparql-parser.md) and
[docs/05-query-engine.md](docs/05-query-engine.md).

## HTTP service

`graphy-server` currently exposes `/sparql`, `/sparql/service`, `/graphs`, and
`/health`. Query work runs on Tokio's blocking pool, with one snapshot pinned
per request. Results support SPARQL JSON/XML/CSV/TSV and Turtle/N-Triples graph
responses.

Request bodies, evaluated rows, and serialized responses are currently
buffered. Streaming response bodies, admission control, authentication,
metrics, compression, and additional administrative endpoints remain roadmap
work.

See [docs/06-sparql-service.md](docs/06-sparql-service.md).

## RDF and SPARQL 1.2

The syntax crates track the available RDF/SPARQL 1.2 Working Draft suites.
Turtle-family parsers expose `Options::spec12`; the SPARQL parser accepts its
implemented 1.2 syntax directly. Triple terms, directional language literals,
the implemented builtins, updates, and result encodings are covered by the
checked-in conformance harnesses.

The precise tested boundary and intentional omissions are in
[docs/12-conformance.md](docs/12-conformance.md).

## Safety and portability

Unsafe code is limited to the memory-view and mmap boundaries in
`graphy-succinct`, `graphy-store`, and the CLI/pipeline mmap call sites. The
workspace denies undocumented unsafe blocks. Segment zero-copy views require a
little-endian target; heap-backed and embedded-image paths share the same
component parser.
