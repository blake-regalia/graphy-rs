#!/bin/sh
# M11a VS Code smoke test (docs/10 §14): drive graphy-lsp through the real
# VS Code client stack — semantic tokens (incl. re-request after an edit,
# which exercises incremental sync and full/delta), document symbols, and
# folding ranges, for Turtle, SPARQL, and JSON-LD.
#
# Requires VS Code.app and node/npm. Exit 0 = pass; report.json has details.
set -eu

here="$(cd "$(dirname "$0")" && pwd)"
repo="$(cd "$here/../.." && pwd)"
electron="/Applications/Visual Studio Code.app/Contents/MacOS/Electron"
[ -x "$electron" ] || { echo "VS Code.app not found" >&2; exit 2; }

cargo build -p graphy-lsp --release --manifest-path "$repo/Cargo.toml"
[ -d "$here/ext/node_modules" ] || (cd "$here/ext" && npm install --no-audit --no-fund)

export GRAPHY_LSP_BIN="$repo/target/release/graphy-lsp"
export SMOKE_FIXTURES="$here/fixtures"
export SMOKE_REPORT="$here/report.json"
rm -f "$SMOKE_REPORT"

status=0
"$electron" \
  --extensionDevelopmentPath="$here/ext" \
  --extensionTestsPath="$here/ext/test" \
  --user-data-dir="$here/userdata" \
  --disable-extensions --disable-workspace-trust --disable-gpu \
  "$here/fixtures" > "$here/run.log" 2>&1 || status=$?

cat "$SMOKE_REPORT" 2>/dev/null || tail -20 "$here/run.log" >&2
exit $status
