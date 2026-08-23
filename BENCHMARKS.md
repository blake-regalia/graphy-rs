# Benchmark baselines

These are dated development baselines, not universal performance guarantees. Unless noted otherwise, results were recorded on an Apple M4 Max with 128 GB RAM. Re-run the commands on the target machine before making capacity decisions.

## Reproducing measurements

```sh
cargo bench -p graphy-succinct
cargo bench -p graphy-store
cargo bench -p graphy-query
cargo bench -p graphy-pipe
```

For end-to-end runs, use a release build and record the input checksum, command, wall time, peak resident memory, output size, operating system, and commit:

```sh
cargo build --release -p graphy-cli
/usr/bin/time -l target/release/graphy load output.graphy input.nt
/usr/bin/time -l target/release/graphy verify output.graphy
```

## Succinct primitives

Microbenchmarks cover rank/select bit vectors, Elias–Fano sequences, wavelet matrices, and packed integer arrays. Typical point operations range from tens of nanoseconds for direct packed access to low microseconds for compound wavelet-matrix queries. Consult the current Criterion output instead of treating a single stored number as a contract.

## Parser throughput

A post-audit streaming-parser run measured approximately:

| Format | Throughput |
| --- | ---: |
| N-Triples | 363 MiB/s |
| RDF/XML | 166 MiB/s |

Parser speed depends heavily on term shape, allocation pressure, validation settings, and chunk size. Interoperability and failure behavior take precedence over peak throughput.

## Segment construction

A synthetic 100-million-triple load produced the following baseline:

| Stage | Result |
| --- | ---: |
| Initial load | 0.92 million triples/s |
| Tuned load | 1.73 million triples/s |
| Verification | 34 s |
| Raw-to-segment size ratio | 2.56× |
| Subsequent compaction size improvement | 1.10× |

Peak RSS varied substantially with input ordering and interning strategy. Treat memory as a first-class measurement when comparing loader settings.

## Segment reads

An earlier segment-reader baseline reported:

| Operation | Result |
| --- | ---: |
| Point lookup latency | 1.10 µs |
| Sequential scan | 122 million triples/s |
| Subject-bound scan | 37.1 million triples/s |
| Subject/predicate-bound scan | 10.8 million triples/s |
| Heap versus mapped scan ratio | 5.56× |

This run predates the latest parser and format audit. Reproduce it before using the figures in comparisons.

## Updates and merge

Representative update-path measurements:

| Operation | Result |
| --- | ---: |
| Insert throughput | 150 thousand triples/s |
| Delete throughput | 127 thousand triples/s |
| Delta memory overhead | 3.6% |
| Merge, baseline | 2.37 million triples/s |
| Merge, tuned | 2.85 million triples/s |
| Snapshot acquisition | 19.5 ms |

Long-running recovery and compaction soaks remain a release requirement.

## HDT export

A direct-export experiment measured 0.23 s versus 0.49 s for an intermediate-path export, a 2.1× speedup on that input. Independent HDTQ compatibility validation is still pending; this is a construction benchmark, not a conformance claim.

## Query execution

Post-audit microbenchmarks included:

| Shape | Result |
| --- | ---: |
| Simple indexed query | 2.099 µs |
| Star join, reference | 7.746 µs |
| Star join, indexed | 5.800 µs |
| Chain join, reference | 10.878 µs |
| Chain join, indexed | 5.515 µs |
| Filter | 2.214 µs |
| Grouping | 8.363 µs |

A one-million-row differential run was also completed. Current physical operators are in-memory; there is no disk spill path, so peak memory must be measured alongside latency.

## Pipeline

One pipeline comparison measured the native event pipeline at 94% of a direct baseline on its test input. The useful regression signal is whether the ratio changes under the same command and corpus, not the absolute number in isolation.

## Workload smoke tests

WatDiv, BSBM, and SP2Bench queries are included as smoke and regression workloads. Their current results help locate slow shapes; they are not a claim of competitiveness with mature database systems.

## Interpreting these numbers

- Confirm the commit and command before comparing results.
- Compare like-for-like validation, durability, and output settings.
- Record both median latency and tail behavior for services.
- Record peak memory for loads, joins, grouping, sorting, and updates.
- Keep conformance results separate from performance results.
