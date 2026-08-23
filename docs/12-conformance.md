# 12 · RDF and SPARQL Conformance

This document records executable conformance boundaries. It is deliberately
narrower than feature-roadmap claims elsewhere: a feature is listed as
covered only when a checked-in harness runs its authoritative W3C manifest.

## Corpus gates

| Area | Corpus | Current result |
|---|---|---:|
| RDF text syntax/evaluation | Turtle, TriG, N-Triples, N-Quads; RDF 1.1 and available RDF 1.2 manifests | 1,045 green |
| RDF/XML | W3C RDF/XML manifests, including canonical `rdf:XMLLiteral` comparisons | 166 green |
| SPARQL syntax | SPARQL 1.0 / 1.1 / available 1.2 directories | 199 / 152 / 203 green |
| SPARQL print round trip | Positive query and update syntax across 1.0/1.1/1.2 | 377 green |
| Query evaluation | SPARQL 1.0 / 1.1 / available 1.2 directories | 283 / 235 / 63 green |
| Update evaluation | SPARQL 1.1 / SPARQL 1.2 triple-term updates | 94 / 3 green |

The query harness includes property paths, subqueries, aggregates, result
formats, RDF/XML input data, and SPARQL 1.2 code-point, language-direction,
version, expression, grouping, RDF 1.1 compatibility, and triple-term
directories. Query evaluation runs both the reference evaluator and the
vectorized evaluator where applicable.

The suite checkout is optional for ordinary crate consumers. In a
conformance checkout, `testdata/rdf-tests` points at the W3C `rdf-tests`
repository; absence skips corpus tests, but CI/release qualification must
provide it.

Independent implementation regressions are also run from pinned, optional
Oxigraph and RDF4J sparse checkouts. See [13 · Independent Oracle
Corpora](13-oracle-corpora.md) for provenance, licenses, exclusions, and the
release-CI requirement.

## Intentional boundary

Federated `SERVICE` execution is not implemented. `SERVICE` is accepted by
the parser, retained in algebra, and printed, but physical planning returns
an explicit error. The federated evaluation directory is the only
intentional SPARQL corpus omission. `SERVICE SILENT` is not treated as a
license to hide the missing execution feature.

Entailment-regime tests are not simple-entailment conformance tests and are
outside the engine's stated inference scope.

## Semantics protected by regression tests

- RDF term identity is independent of storage position. A term reused
  between subject, predicate, object, and graph columns keeps one query
  identity for joins, repeated variables, and `sameTerm`.
- Absolute IRI spellings are validated but not normalized into another RDF
  term. Blank-node labels admitted by the owned-term API are legal RDF text
  labels.
- Numeric datatype subtypes and `xsd:float` survive evaluation; `STR`
  preserves lexical forms. Date/dateTime timezone comparison follows the
  partial order.
- Language tags are case-insensitive where the specifications require it;
  directional builtins and XML/JSON result serialization retain base
  direction.
- Triple terms work recursively in patterns, expressions, templates, result
  formats, and Update. Invalid `TRIPLE()` components and invalid
  instantiated `CONSTRUCT` triples produce the specified error/drop
  behavior.
- Zero-length paths include absent constant endpoints in the identity
  relation.
- `LOAD` is an injected engine capability. The native server loader supports
  HTTP(S) Turtle, N-Triples, RDF/XML, and JSON-LD when both the non-default
  `outbound-http` Cargo feature and `graphy serve --allow-network` are
  enabled; `SILENT`, graph targets, base IRIs, fresh blank-node scope, and
  the deny-by-default path are tested. WASM does not compile an HTTP client.

## Reproduction

From the workspace root:

```sh
cargo test -p graphy-turtle --test w3c
cargo test -p graphy-interop --test w3c_rdfxml
cargo test -p graphy-sparql-syntax --test w3c_syntax
cargo test -p graphy-algebra --test w3c_print_roundtrip
cargo test -p graphy-engine --test w3c_eval
```

The server protocol tests bind an ephemeral localhost port and may require
socket permission in a sandbox:

```sh
cargo test -p graphy-server --test protocol
```
