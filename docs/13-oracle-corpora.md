# 13 · Independent Oracle Corpora

The W3C suites are the normative conformance gate. Independent engine
regression corpora add a different signal: they exercise bugs discovered in
production implementations without sharing Graphy's parser, algebra, value
model, or evaluator.

## Pinned sources

`testdata/oracles.lock.toml` pins the exact upstream revisions and licenses.
The corpora are not committed to this repository. Fetch the sparse checkouts
with:

```sh
scripts/fetch-oracles.sh
```

The default destination is `testdata/oracles/`. Set `GRAPHY_ORACLES_DIR` to
use a shared cache. Ordinary test runs skip absent corpora; release CI sets
`GRAPHY_REQUIRE_ORACLES=1` so absence is a failure.

| Oracle | Executed corpus | License |
|---|---|---|
| Oxigraph | Standard SPARQL syntax/evaluation, Turtle/TriG/N-Triples/N-Quads parser cases, RDF/XML cases | MIT OR Apache-2.0 |
| RDF4J | Non-W3C SPARQL 1.1 and 1.2 query-evaluation manifests | BSD-3-Clause |

At the pinned revisions, the executed census is:

| Gate | Green | Explicitly excluded |
|---|---:|---:|
| Oxigraph SPARQL syntax | 8 | 3 |
| Oxigraph SPARQL evaluation | 18 | 12 |
| Oxigraph RDF text parsing | 27 | 0 |
| Oxigraph RDF/XML parsing | 5 | 0 |
| RDF4J SPARQL evaluation | 113 | 14 |

The harness reads upstream W3C-manifest-shaped files directly. It does not
copy them into Graphy or regenerate expected results with Graphy.

## Explicit boundaries

- Oxigraph and RDF4J `LATERAL` evaluations are reported but excluded by query
  filename because Graphy does not implement SPARQL 1.2 `LATERAL`.
- Two Oxigraph `SERVICE` error-propagation cases are excluded because Graphy
  deliberately has no federated execution backend. One ordering fixture
  asserts an implementation-defined order between incomparable literal
  types. One cast fixture canonicalizes the source RDF terms in its expected
  results, contrary to Graphy's lexical-form-preserving term identity.
- Two deliberately extreme Oxigraph syntax inputs exceed Graphy's defensive
  nesting cap. Another expects scheme-specific empty-port rejection beyond
  the generic RFC 3987 IRI grammar used by Graphy.
- RDF4J exclusions cover five `LATERAL` cases, three vendor property-path
  cardinality extensions, two obsolete draft `BINDINGS` queries, and four
  malformed/stale expected-result fixtures. Every exclusion is keyed by
  query filename and prints its reason during the test run.
- Oxigraph GeoSPARQL is outside Graphy's current feature scope.
- Oxigraph optimizer input/output pairs assert Oxigraph's physical rewrite,
  not implementation-independent SPARQL behavior.
- Oxigraph lenient/recovery manifests specify non-conforming recovery modes;
  Graphy's strict conformance parser does not treat them as success criteria.
- Oxigraph result-format manifests test parsing. Graphy currently serializes
  SPARQL results but exposes no results-parser API.
- RDF4J directories suffixed `-w3c` are intentionally not fetched: Graphy
  already runs the authoritative upstream W3C checkout.

Every executed query is evaluated by both Graphy's reference and vectorized
engines. Expected solution multisets, ordered results, booleans, and graph
isomorphism use the same comparison rules as the W3C gate.

## Reproduction

```sh
scripts/fetch-oracles.sh
GRAPHY_REQUIRE_ORACLES=1 cargo test -p graphy-sparql-syntax --test w3c_syntax oxigraph
GRAPHY_REQUIRE_ORACLES=1 cargo test -p graphy-turtle --test w3c oxigraph
GRAPHY_REQUIRE_ORACLES=1 cargo test -p graphy-interop --test w3c_rdfxml oxigraph
GRAPHY_REQUIRE_ORACLES=1 cargo test -p graphy-engine --test w3c_eval oxigraph
GRAPHY_REQUIRE_ORACLES=1 cargo test -p graphy-engine --test w3c_eval rdf4j
```

When an upstream update is intentional, change its revision in both the lock
file and fetch script, refetch, review newly added/removed tests and notices,
then update the conformance census. Never float an oracle checkout in release
CI.
