# SPARQL syntax and algebra

`graphy-sparql-syntax` parses SPARQL Query and Update text into a span-carrying AST. `graphy-algebra` translates that AST into the engine-independent algebra consumed by `graphy-engine`.

## Parsing and printing

```rust
use graphy_sparql_syntax::{parse_query, print_query};

let query = parse_query("SELECT ?s WHERE { ?s ?p ?o }")?;
let normalized = print_query(&query);
# Ok::<(), graphy_sparql_syntax::ParseError>(())
```

The primary entry points are `parse_query`, `parse_update`, `print_query`, and `print_update`. `parse_query_recovering` and `parse_update_recovering` retain partial results and diagnostics for editor use. `tokenize_resilient` is the non-failing lexical path used by the language server.

The parser supports SPARQL 1.0/1.1 plus the implemented RDF/SPARQL 1.2 working-draft syntax, including version declarations, triple terms, reifiers, annotations, and directional language tags.

## Algebra

Translation covers graph patterns, property paths, joins, optional patterns, minus, union, graph scopes, values, grouping and aggregates, subqueries, solution modifiers, query forms, and SPARQL Update operations. Algebra rewrites are semantics-preserving; cost-based planning belongs to `graphy-engine`.

`SERVICE` is represented in syntax and algebra but is not executed by the local engine.

## Safe substitution

The syntax crate provides query/update substitution helpers that replace variables with parsed terms while respecting scopes. Prefer these helpers to building SPARQL strings by concatenation.

See [12-conformance.md](12-conformance.md) for the syntax and print-roundtrip suites.
