# Storage engine

`graphy-store` combines an immutable compressed base segment with an ordered in-memory delta. A snapshot pins both views and merges them during scans while hiding tombstones.

## Base segments

A segment is a directory containing:

- front-coded term dictionaries and rebuildable hash sidecars;
- one or more succinct triple orderings;
- graph-membership indexes for quad datasets;
- predicate and characteristic-set statistics;
- a JSON manifest with counts, versions, lengths, and checksums.

The index profiles are:

| Profile | Materialized orders |
| --- | --- |
| `compact` | SPO, with compact predicate/object accessors |
| `balanced` | SPO, POS, OSP |
| `covering` | all six permutations |

`balanced` is the builder default. The complete binary contract is in [08-segment-format.md](08-segment-format.md).

## Opening and scanning

`Segment::open` uses heap-backed component data. `Segment::open_with` accepts `OpenMode`, including the explicit memory-mapped mode on native targets. `Store::open` adds the write-ahead log and mutable delta.

`Snapshot::scan` accepts a `Pattern` and an `Order`; `scan_best` chooses an available order. Scans return batches of numeric columns. `term_id` and the inverse column lookup bridge position-local segment identifiers and snapshot-level `TermId`s.

## Building

`SegmentBuilder` accepts concise terms, spills provisional records under the configured sort budget, finalizes dictionaries, sorts each requested ordering, and writes the manifest last. Builds are deterministic for identical inputs and configuration.

The dictionary interning map is the structure proportional to distinct terms. `BuilderConfig::intern_budget` can spill dictionary runs; `sort_budget` bounds external-sort buffers.

## Integrity

Normal open validates structure and component metadata. Deep verification additionally reads and cross-checks component contents and digests. Memory-mapped open intentionally avoids faulting every page merely to checksum it; run `graphy verify` when integrity must be established.
