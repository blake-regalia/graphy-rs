#!/usr/bin/env python3
"""Static server with COOP/COEP headers (SharedArrayBuffer requirement for
the wasm-threads build — docs/11 §6)."""
import http.server, sys

class Handler(http.server.SimpleHTTPRequestHandler):
    def end_headers(self):
        self.send_header("Cross-Origin-Opener-Policy", "same-origin")
        self.send_header("Cross-Origin-Embedder-Policy", "require-corp")
        super().end_headers()

http.server.ThreadingHTTPServer(
    ("127.0.0.1", int(sys.argv[1]) if len(sys.argv) > 1 else 8735), Handler
).serve_forever()
