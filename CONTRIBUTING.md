# Contributing

`graphy-rs` is pre-release. Please open an issue before a large change so its public API, format, and conformance impact can be agreed on early.

## Development checks

Install the Rust toolchain declared by the workspace, then run:

```sh
cargo fmt --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
npx --yes wasm-pack@0.15.0 build --release --target web \
  --out-dir pkg-web crates/graphy-wasm -- --locked
```

Release changes should also build the CLI for each supported WASI ABI:

```sh
cargo build --release --locked -p graphy-cli --target wasm32-wasip1
cargo build --release --locked -p graphy-cli --target wasm32-wasip2
./scripts/build-wasip3.sh
```

Changes to RDF or SPARQL behavior should also run the relevant commands in [docs/12-conformance.md](docs/12-conformance.md). Changes to benchmark claims should include the exact command, input, hardware, peak memory where relevant, and commit in [BENCHMARKS.md](BENCHMARKS.md).

## Expectations

- Keep public documentation about current behavior; put planned work in [IMPLEMENTATION_PLAN.md](IMPLEMENTATION_PLAN.md).
- Add regression tests for bug fixes and specification-sensitive behavior.
- Preserve deterministic segment output unless a documented format change requires otherwise.
- Do not commit downloaded benchmark corpora, generated packages, credentials, or local editor state.
- Keep unsafe code narrowly scoped and document its safety invariants.

Contributions are licensed under Apache-2.0 as described in [LICENSE](LICENSE).

## Cutting a release

After CI passes on `main`, ensure the workspace version and lockfiles are
current, then push the matching `vX.Y.Z` tag. The release workflow builds the
native binary matrix, browser WebAssembly package, and WASI CLI modules;
publishes checksummed artifacts; and generates GitHub release notes. Do not
move or reuse a published version tag.
