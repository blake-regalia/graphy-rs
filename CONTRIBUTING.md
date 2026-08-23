# Contributing

`graphy-rs` is pre-release. Please open an issue before a large change so its public API, format, and conformance impact can be agreed on early.

## Development checks

Install the Rust toolchain declared by the workspace, then run:

```sh
cargo fmt --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
```

Changes to RDF or SPARQL behavior should also run the relevant commands in [docs/12-conformance.md](docs/12-conformance.md). Changes to benchmark claims should include the exact command, input, hardware, peak memory where relevant, and commit in [BENCHMARKS.md](BENCHMARKS.md).

## Expectations

- Keep public documentation about current behavior; put planned work in [IMPLEMENTATION_PLAN.md](IMPLEMENTATION_PLAN.md).
- Add regression tests for bug fixes and specification-sensitive behavior.
- Preserve deterministic segment output unless a documented format change requires otherwise.
- Do not commit downloaded benchmark corpora, generated packages, credentials, or local editor state.
- Keep unsafe code narrowly scoped and document its safety invariants.

Contributions are licensed under Apache-2.0 as described in [LICENSE](LICENSE).
