#!/bin/sh
# M11a Neovim smoke test: graphy-lsp through the built-in LSP client of a
# stock nvim (0.11+), fully headless. Shares fixtures with vscode-smoke.
# Exit 0 = pass; report.json has details.
set -eu

here="$(cd "$(dirname "$0")" && pwd)"
repo="$(cd "$here/../.." && pwd)"
command -v nvim >/dev/null || { echo "nvim not found" >&2; exit 2; }

cargo build -p graphy-lsp --release --manifest-path "$repo/Cargo.toml"

export GRAPHY_LSP_BIN="$repo/target/release/graphy-lsp"
export SMOKE_FIXTURES="$here/../vscode-smoke/fixtures"
export SMOKE_REPORT="$here/report.json"
rm -f "$SMOKE_REPORT"

status=0
nvim --headless --clean -l "$here/smoke.lua" > "$here/run.log" 2>&1 || status=$?

cat "$SMOKE_REPORT" 2>/dev/null || tail -20 "$here/run.log" >&2
echo
exit $status
