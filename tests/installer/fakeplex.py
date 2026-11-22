"""A stand-in for Plex Media Server, for the installer's terminal setup in the installer tests.

    python fakeplex.py PORT TOKEN MOVIES_DIR TV_DIR LOG

- GET /identity answers without a token (installers check Plex is there with it);
- GET /library/sections, with the token in X-Plex-Token, lists a movie library (number 1) in
  MOVIES_DIR and a TV library (number 2) in TV_DIR, as Plex's JSON does;
- GET /library/sections/<n>/all answers an empty library page (Arr's import of what you own);
- POST or GET /library/sections/<n>/refresh answers OK.

Any other path is 404; a missing or wrong token is 401. Every request's method and path goes to
LOG, one per line, so a test can see what Arr asked for.
"""

import http.server
import json
import re
import sys

port = int(sys.argv[1])
token = sys.argv[2]
movies = sys.argv[3]
tv = sys.argv[4]
log = sys.argv[5]


class Handler(http.server.BaseHTTPRequestHandler):
    def answer(self, status, body=None):
        data = json.dumps(body).encode("utf-8") if body is not None else b""
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def handle_any(self):
        with open(log, "a", encoding="utf-8") as f:
            f.write(f"{self.command} {self.path}\n")
        path = self.path.split("?", 1)[0]
        if path == "/identity":
            return self.answer(200, {"MediaContainer": {"size": 0, "claimed": True, "machineIdentifier": "fake-plex", "version": "1.0.0"}})
        if self.headers.get("X-Plex-Token") != token:
            return self.answer(401)
        if path == "/library/sections":
            return self.answer(200, {"MediaContainer": {"size": 2, "Directory": [
                {"key": "1", "type": "movie", "title": "Movies", "agent": "tv.plex.agents.movie", "Location": [{"id": 1, "path": movies}]},
                {"key": "2", "type": "show", "title": "TV Shows", "agent": "tv.plex.agents.series", "Location": [{"id": 2, "path": tv}]},
            ]}})
        if re.fullmatch(r"/library/sections/\d+/all", path):
            return self.answer(200, {"MediaContainer": {"size": 0, "totalSize": 0, "offset": 0, "Metadata": []}})
        if re.fullmatch(r"/library/sections/\d+/refresh", path):
            return self.answer(200)
        return self.answer(404)

    do_GET = handle_any
    do_POST = handle_any

    def log_message(self, *args):
        pass


http.server.ThreadingHTTPServer(("127.0.0.1", port), Handler).serve_forever()
