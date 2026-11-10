"""A stand-in for a busy Arr, for macos-boot.sh.

Run as `python3 serve --config CONFIG` under Arr's command line. It answers /api/v1/health with the
version in $FAKE_STATE/version (read at start). On SIGTERM it stops answering at once and exits
only $FAKE_STATE/slow seconds later, like Arr finishing a worker pass before it quits.
"""

import http.server
import os
import re
import signal
import sys
import threading
import time

config = sys.argv[sys.argv.index("--config") + 1]
port = int(re.search(r'listen = "[^"]*:(\d+)"', open(config).read()).group(1))
state = os.environ["FAKE_STATE"]
version = open(state + "/version").read().strip()
drain = int(open(state + "/slow").read().strip())


class Handler(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.end_headers()
        self.wfile.write(('{"version": "%s"}' % version).encode())

    def log_message(self, *args):
        pass


server = http.server.ThreadingHTTPServer(("127.0.0.1", port), Handler)
threading.Thread(target=server.serve_forever, daemon=True).start()


def term(*args):
    server.shutdown()
    server.server_close()
    time.sleep(drain)
    os._exit(0)


signal.signal(signal.SIGTERM, term)
while True:
    signal.pause()
