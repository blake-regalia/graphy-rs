# RDF text parsers

`graphy-turtle` provides incremental parsers for N-Triples, N-Quads, Turtle, and TriG, plus N-Quads and Turtle/TriG writers. RDF 1.2 syntax is enabled by default.

## Incremental API

```rust
use graphy_turtle::{Options, TurtleParser};

let mut parser = TurtleParser::new(Options::default())?;
parser.feed(b"<urn:s> <urn:p> <urn:o> .")?;
for quad in parser.drain() {
    // Consume or copy borrowed values before the next feed.
    consume(quad);
}
parser.finish()?;
# Ok::<(), graphy_turtle::ParseError>(())
```

Equivalent parser types are `TriGParser`, `NTriplesParser`, and `NQuadsParser`. `read_from` is the synchronous `Read` adapter. Parser output borrows concise-encoded terms from an internal arena.

`Options` controls the base IRI, RDF 1.2 acceptance, lenient recovery, blank-node label namespace, and trusted-input validation. A loader combining documents must use a distinct `label_ns` for each because blank-node labels are document-scoped. `trusted` may accept invalid data and should be used only for previously validated input.

## Parallel N-Triples and N-Quads

With the crate's parallel feature, `par::ntriples` and `par::nquads` split input at line boundaries and preserve document-wide blank-node identity. Turtle and TriG remain sequential because prefixes, bases, long strings, and graph blocks carry state across arbitrary offsets.

## Writers

`NQuadsWriter` emits a direct line format. `TurtleWriter` can emit Turtle or TriG, register prefixes, use terse literals, retain labels, and group statements. Call `finish` to flush pending groups and recover the inner writer.

Parser conformance commands and counts are documented in [12-conformance.md](12-conformance.md).
