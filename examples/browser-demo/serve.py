#!/usr/bin/env python3
"""Static server with the two headers SharedArrayBuffer requires."""
import http.server, sys
class H(http.server.SimpleHTTPRequestHandler):
    def do_GET(self):
        # Result channel: lets a headless run report real wall-clock
        # timings without a WebDriver. `--virtual-time-budget` would
        # otherwise be needed to know when the page finished, and it
        # makes performance.now() meaningless.
        if self.path.startswith('/__result'):
            from urllib.parse import urlparse, parse_qs, unquote
            q = parse_qs(urlparse(self.path).query)
            print("RESULT " + unquote(q.get('data', [''])[0]), flush=True)
            self.send_response(200); self.end_headers(); self.wfile.write(b'ok')
            return
        super().do_GET()

    def end_headers(self):
        self.send_header('Cross-Origin-Opener-Policy', 'same-origin')
        self.send_header('Cross-Origin-Embedder-Policy', 'require-corp')
        super().end_headers()
    def log_message(self, *a): pass
port = int(sys.argv[1]) if len(sys.argv) > 1 else 8080
http.server.HTTPServer(('127.0.0.1', port), H).serve_forever()
