#!/bin/sh
# M12d browser smoke (docs/11): build the web-target package and serve the
# self-asserting smoke page. Open http://localhost:8734/smoke.html — the
# page prints VERDICT: PASS after exercising load/query/NOW()/update/export.
set -eu
here="$(cd "$(dirname "$0")" && pwd)"
PATH="$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin:$PATH" \
  npx --yes wasm-pack build --target web --out-dir pkg-web "$here"
echo "serving http://localhost:8734/smoke.html (ctrl-c to stop)"
exec python3 -m http.server 8734 -d "$here"
