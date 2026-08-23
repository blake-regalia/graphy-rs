#!/bin/sh
# wasm-threads build + smoke (docs/11 §6). Requires the rustup NIGHTLY
# toolchain with rust-src and the wasm32-unknown-unknown target:
#   rustup toolchain install nightly --profile default \
#     --component rust-src --target wasm32-unknown-unknown
# Some nightlies ship rust-lld without libLLVM.dylib on its rpath; if the
# link SIGABRTs with "Library not loaded: @rpath/libLLVM.dylib":
#   ln -sf ../../../libLLVM.dylib \
#     ~/.rustup/toolchains/nightly-*/lib/rustlib/aarch64-apple-darwin/lib/
# Serve with COOP/COEP (SharedArrayBuffer) and open smoke-threads.html —
# the worker self-asserts (identical 1-vs-4-thread results, serial-vs-
# parallel parse) and reports timings.
#
# --shared-memory/--import-memory are load-bearing: without them the
# linker emits a NON-shared memory even under +atomics, and every
# memory.atomic.wait32 throws "Atomics.wait cannot be called in this
# context" the first time a scope actually joins.
set -eu
here="$(cd "$(dirname "$0")" && pwd)"
export PATH="$HOME/.rustup/toolchains/nightly-aarch64-apple-darwin/bin:$PATH"
RUSTFLAGS='-C target-feature=+atomics,+bulk-memory,+mutable-globals -C link-arg=--shared-memory -C link-arg=--import-memory -C link-arg=--max-memory=1073741824' \
  npx --yes wasm-pack build --target web --out-dir pkg-threads "$here" \
  -- --features wasm-threads -Z build-std=std,panic_abort
echo "serving http://localhost:8735/smoke-threads.html (ctrl-c to stop)"
exec python3 "$here/coi-server.py" 8735
