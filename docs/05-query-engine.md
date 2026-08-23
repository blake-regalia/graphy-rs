# Query engine

`graphy-engine` executes the algebra produced by `graphy-algebra` against a pinned `graphy-store::Snapshot`.

## Execution tiers

The reference evaluator is the semantic fallback and conformance oracle. The vectorized executor plans supported algebra into batches and falls back cleanly when it encounters an unsupported physical shape.

Current physical operators include:

- scans and index-nested-loop bind joins;
- property-path steps and hash joins;
- left join, minus, filter, extend, union, and graph scopes;
- inline tables, grouping and aggregates;
- sort, project, distinct, offset, and limit.

Operators are in memory. There is no disk spill path, merge join, or worst-case-optimal join in the current implementation.

## Planning

Leaf estimates use snapshot pattern counts. Basic graph patterns are ordered by a greedy cardinality/connectivity heuristic. A pattern on the right side of a pipeline becomes a bind join; other joins use hash tables over known shared variables.

Plan-cache entries are scoped to the snapshot identity so a plan cannot silently reuse stale dictionary identifiers after a generation change.

## Semantics

The evaluator covers SPARQL query forms, dataset clauses, graph variables, paths, expressions, aggregates, ordering, and solution modifiers. Updates are executed atomically per operation through the store. Federated `SERVICE` execution is not supported.

Both evaluators are exercised by W3C-derived suites and differential tests. Current counts and exclusions are in [12-conformance.md](12-conformance.md) and [13-oracle-corpora.md](13-oracle-corpora.md).
