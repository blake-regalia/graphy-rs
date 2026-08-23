# CLI pipeline

The pipeline chains RDF stages inside one `graphy` process. Stages exchange concise-term events, avoiding text serialization between operators. The actual help is authoritative:

```sh
graphy help pipeline
graphy help read
graphy help write
```

## Shape

Commands are separated by `/`:

```sh
graphy read / tree / write -c trig --inputs data.ttl
graphy scan --threads 4 / distinct -s --inputs data.nq
```

A pipeline starts with `read` or `scan`, may contain unary operators, and ends in a serializer or result. Multiple input legs can meet at one `concat` or `merge` junction. `concat` preserves input order; `merge` processes legs concurrently and emits arrival order.

## Implemented stages

| Category | Stages |
| --- | --- |
| Sources | `read`, `scan` |
| Unary | `skip`, `head`, `tail`, `tree` |
| Junctions | `concat`, `merge` |
| Outputs | `scribe`, `write`, `count`, `distinct` |

`read` is serial. `scan` uses data-parallel parsing for N-Triples and N-Quads. Formats and options can be supplied explicitly or inferred from file extensions; standard input defaults to TriG.

`head` propagates cancellation to the source, so a short prefix need not read the entire file. `tail` buffers the requested number of items. `tree` deduplicates and regroups statements for pretty output. `distinct` supports quad, triple, and individual-position projections.

If a quad-producing pipeline has no explicit terminal, the CLI writes N-Quads. Unsupported stages such as filters, transforms, set algebra, canonicalization, and store endpoints are roadmap items, not accepted commands.
