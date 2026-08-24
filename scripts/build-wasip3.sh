#!/usr/bin/env bash
set -euo pipefail

readonly toolchain="nightly-2026-08-22"
readonly sdk_tag="wasi-sdk-34-rc.3"
readonly sdk_version="34.0-rc.3+m"
readonly archive="wasi-sysroot-${sdk_version}.tar.gz"
readonly checksum="ea83b395c11bdddc30e9fe6ba7d34d6d0370dfd3decf616f1febfb962418a2f3"
readonly url="https://github.com/WebAssembly/wasi-sdk/releases/download/${sdk_tag}/wasi-sysroot-34.0-rc.3%2Bm.tar.gz"

work_dir=$(mktemp -d "${TMPDIR:-/tmp}/graphy-wasip3.XXXXXX")
trap 'rm -r -- "$work_dir"' EXIT

curl --fail --location --retry 3 --output "${work_dir}/${archive}" "${url}"
if command -v sha256sum >/dev/null 2>&1; then
  actual_checksum=$(sha256sum "${work_dir}/${archive}" | cut -d ' ' -f 1)
else
  actual_checksum=$(shasum -a 256 "${work_dir}/${archive}" | cut -d ' ' -f 1)
fi
test "${actual_checksum}" = "${checksum}"
tar -xzf "${work_dir}/${archive}" -C "${work_dir}"

lib_dir="${work_dir}/wasi-sysroot-${sdk_version}/lib/wasm32-wasip3"
test -f "${lib_dir}/crt1-command.o"
test -f "${lib_dir}/libc.a"

cargo_bin=$(rustup which --toolchain "${toolchain}" cargo)
rustc_bin=$(rustup which --toolchain "${toolchain}" rustc)
if [[ "$(uname -s)" = Darwin ]]; then
  toolchain_lib="$(dirname "$(dirname "${rustc_bin}")")/lib"
  export DYLD_LIBRARY_PATH="${toolchain_lib}${DYLD_LIBRARY_PATH:+:${DYLD_LIBRARY_PATH}}"
fi
target_rustflags="${RUSTFLAGS:-}${RUSTFLAGS:+ }${CARGO_TARGET_WASM32_WASIP3_RUSTFLAGS:-}"
export CARGO_TARGET_WASM32_WASIP3_RUSTFLAGS="${target_rustflags}${target_rustflags:+ }-L native=${lib_dir}"
unset RUSTFLAGS
RUSTC="${rustc_bin}" "${cargo_bin}" build -Z build-std=std,panic_abort \
  --release --locked -p graphy-cli --target wasm32-wasip3
