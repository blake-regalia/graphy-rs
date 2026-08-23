# Data model and term encoding

`graphy-core` defines owned and borrowed RDF terms, triples, quads, graph names, concise byte encodings, and 64-bit store identifiers.

## RDF model

A dataset contains a default graph and zero or more named graphs. Subjects are IRIs, blank nodes, or supported triple terms; predicates are IRIs; objects are any RDF term; named graphs are IRIs or blank nodes. Literals retain their lexical form, datatype, optional lowercased language tag, and optional RDF 1.2 base direction.

## Concise terms

Crates exchange self-delimiting byte strings whose first byte identifies the term kind:

| Prefix | Meaning | Example |
| --- | --- | --- |
| `>` | Absolute IRI | `>https://example.test/item` |
| `_` | Blank node | `_b17` |
| `"` | Simple string literal | `"hello` |
| `@` | Language literal | `@en"hello` |
| `^>` | Datatyped literal | `^>http://www.w3.org/2001/XMLSchema#integer"42` |
| byte `0x09` | Triple term | length-prefixed encoded components |

The outer value length supplies the boundary, so the payload needs no closing delimiter or escaping. `Term::from_concise` and `concise::decode` validate externally supplied bytes. Absolute IRIs and literal lexical forms are preserved; RDF identity does not apply Unicode normalization.

Standalone triple terms store three length-prefixed concise terms and are limited to 32 levels of nesting. A persisted segment uses an internal ordinal instead.

## `TermId`

`TermId` uses a four-bit tag and a 60-bit payload. It can represent:

- a dictionary section and ordinal;
- canonical inline integers, decimals, floats/doubles, booleans, dates/date-times, and optionally short strings;
- a triple-term ordinal;
- `DEFAULT_GRAPH` and `UNDEF` sentinels.

Only canonical lexical forms are inlined, preserving RDF term identity. Dictionary identifiers are meaningful within their snapshot; callers should not persist them as portable identifiers.

## Public types

The main types are `Term`, `TermRef`, `TripleTermRef`, `Triple`, `Quad`, `GraphName`, `TermId`, `Section`, and `InlineValue`. Constructors validate IRIs, blank-node labels, language tags, datatypes, and triple-term structure. Borrowed views avoid allocation when a parser or dictionary already owns the bytes.
