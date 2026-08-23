# WebAssembly bindings

`graphy-wasm` exposes an in-browser RDF store and SPARQL engine through `wasm-bindgen`. The default build is single-threaded and keeps mutable data in memory.

## Browser build

Build the release package with:

```sh
npx --yes wasm-pack@0.15.0 build --release --target web \
  --out-dir pkg-web crates/graphy-wasm -- --locked
```

The generated `crates/graphy-wasm/pkg-web` directory contains the ES module
loader, TypeScript declarations, and `.wasm` module. Tagged releases provide
the same directory inside the `graphy-vX.Y.Z-wasm-web.tar.gz` archive.

## Core API

```js
const store = new GraphyStore();
store.load('<urn:s> <urn:p> <urn:o> .', 'turtle');
store.update('INSERT DATA { <urn:a> <urn:b> <urn:c> }');
const results = store.query('SELECT * WHERE { ?s ?p ?o }');
const trig = store.export('trig');
```

`load` accepts `turtle`, `trig`, `ntriples`, or `nquads` plus an optional base IRI. `query` returns SPARQL Results JSON for SELECT/ASK and canonical N-Triples for CONSTRUCT/DESCRIBE. `export` accepts `nquads`, `turtle`, or `trig`. `size` reports the number of quads.

## Segment images and persistence

`GraphyStore.fromSegment(names, blobs, log?)` opens a natively built segment supplied as component byte arrays. Components are loaded into WebAssembly memory; this is not lazy file paging.

`GraphyStore.withLog(log?)` creates a store backed by a capturable WAL. The host is responsible for durable storage:

1. call `drainLog()` after committed changes and append the bytes in order;
2. replay the accumulated bytes through `withLog` on startup;
3. occasionally call `packLog()` and atomically replace the stored log.

OPFS integration belongs to the JavaScript host because its APIs are asynchronous. The repository smoke pages demonstrate that wiring.

## Threaded build

The optional `wasm-threads` feature exposes Promise-returning `loadAsync`, `queryAsync`, and `updateAsync`, plus `setThreads`. It requires the repository's specialized nightly/atomics build, shared WebAssembly memory, worker support, and cross-origin isolation. Use the supplied smoke script rather than treating a normal `wasm-pack` invocation as sufficient.

Federated `SERVICE`, outbound `LOAD`, and filesystem APIs are unavailable in WebAssembly.
