"""A stand-in for GitHub's release pages, for the updaters in the installer tests.

    python3 relserver.py PORT DIRECTORY

- GET or HEAD /releases/latest answers a redirect to /releases/tag/v99.0.0 (what the updaters read
  the newest version from);
- /releases/download/<anything>/<file> serves <file> from DIRECTORY.

The release in DIRECTORY holds this build, so the "new" version v99.0.0 never reports itself
as v99.0.0: an updater must notice that and go back to the version it had.
"""

import http.server
import os
import sys

directory = sys.argv[2]


class Handler(http.server.BaseHTTPRequestHandler):
    def reply(self, head):
        path = self.path
        if path.endswith("/releases/latest"):
            self.send_response(302)
            self.send_header("Location", "/releases/tag/v99.0.0")
            self.end_headers()
            return
        if path.startswith("/releases/tag/"):
            self.send_response(200)
            self.end_headers()
            return
        file = os.path.join(directory, os.path.basename(path))
        if os.path.isfile(file):
            self.send_response(200)
            self.send_header("Content-Length", str(os.path.getsize(file)))
            self.end_headers()
            if not head:
                with open(file, "rb") as f:
                    self.wfile.write(f.read())
        else:
            self.send_response(404)
            self.end_headers()

    def do_GET(self):
        self.reply(False)

    def do_HEAD(self):
        self.reply(True)

    def log_message(self, *args):
        pass


http.server.ThreadingHTTPServer(("127.0.0.1", int(sys.argv[1])), Handler).serve_forever()
