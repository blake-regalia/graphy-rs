# graphy CLI walkthrough

The built-in help is authoritative:

```sh
cargo run -p graphy-cli -- --help
cargo run -p graphy-cli -- load --help
```

For repeated use, build the release binary and place it on your path:

```sh
cargo build --release -p graphy-cli
export PATH="$PWD/target/release:$PATH"
```

Create a small input used by the examples:

```sh
mkdir -p data
printf '%s\n' '<urn:alice> <urn:name> "Alice" .' > data/example.ttl
```

## Load and inspect a store

```sh
graphy load example.graphy data/example.ttl
graphy verify example.graphy
```

Useful load controls include `--profile`, `--threads`, `--intern-budget`, `--sort-budget`, `--base`, and `--trusted`. Run `graphy load --help` for their current values and constraints. Blank-node labels are scoped per input document.

## Export data

```sh
graphy export example.graphy > example.nq
graphy export example.graphy --mmap --format hdt -o example.hdt
graphy export example.graphy --format hdtq -o example.hdtq
```

N-Quads is the default export. HDT drops graph membership; HDTQ uses the qEndpoint dialect. Inspect `graphy export --help` before using either binary format for interchange.

## Query and update locally

```sh
graphy query example.graphy -e 'SELECT ?s ?p ?o WHERE { ?s ?p ?o } LIMIT 10'
graphy query example.graphy query.rq
graphy query example.graphy --explain \
  -e 'SELECT ?s WHERE { ?s <https://example.test/name> ?name }'
graphy query example.graphy --analyze query.rq
```

Local query execution does not implement federated `SERVICE` requests.

## Compact updates

```sh
graphy compact example.graphy
graphy verify example.graphy
```

Compaction writes a new generation inside the store and atomically updates its `CURRENT` pointer.

## Run the HTTP service

```sh
graphy serve example.graphy --bind 127.0.0.1:7878
```

The current routes are:

| Route | Purpose |
| --- | --- |
| `/health` | Liveness check |
| `/sparql` | SPARQL query and update requests |
| `/sparql/service` | SPARQL Protocol service description |
| `/graphs` | Graph Store Protocol operations |

Example requests:

```sh
curl --get http://127.0.0.1:7878/sparql \
  --data-urlencode 'query=ASK { ?s ?p ?o }'

curl http://127.0.0.1:7878/sparql \
  -H 'content-type: application/sparql-update' \
  --data 'INSERT DATA { <urn:s> <urn:p> <urn:o> }'

curl --get http://127.0.0.1:7878/graphs \
  --data-urlencode 'graph=https://example.test/graph'
```

Use `--read-only` to reject updates. Native builds can optionally enable outbound HTTP for SPARQL `LOAD`; it is disabled by default. Requests and result sets are currently buffered in memory. There is no `/load`, `/metrics`, or `/stats` HTTP route.

## Compose streaming pipelines

The pipeline accepts RDF events from files or standard input. Implemented verbs are `read`, `scan`, `scribe`, `write`, `skip`, `head`, `tail`, `tree`, `concat`, `merge`, `count`, and `distinct`.

```sh
graphy read -c ttl / tree / write -c ttl --inputs data/example.ttl
graphy read -c nt / head 10 / scribe -c nt --inputs data/example.nt
graphy read / concat / count --inputs left.ttl right.ttl
```

Use `graphy help pipeline` and `graphy help <verb>` for exact options.

## Libraries and browser use

The workspace crates can be used independently; see [README.md](README.md) and the guides in [docs](docs). Browser bindings are provided by `graphy-wasm`; the language server and VS Code client live under [editors/vscode](editors/vscode).
