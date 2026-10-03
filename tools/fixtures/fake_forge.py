#!/usr/bin/env python3
"""A fake GitHub API on loopback for the GUI e2e: one pull request waiting
for review, its files, and a reviews endpoint that records what it gets.

Usage: fake_forge.py PORT_FILE LOG_FILE
Writes the chosen port to PORT_FILE; appends one JSON line per request to
LOG_FILE. Never talks to the real GitHub.
"""
import json
import sys
from http.server import BaseHTTPRequestHandler, HTTPServer

PORT_FILE, LOG = sys.argv[1], sys.argv[2]

PR = {
    "number": 7,
    "title": "Speed up the parser",
    "repository_url": "https://api.github.com/repos/acme/widgets",
    "html_url": "https://github.com/acme/widgets/pull/7",
    "user": {"login": "alice"},
    "updated_at": "2026-10-02T10:00:00Z",
}
PATCH = "@@ -1,4 +1,5 @@\n fn parse(s: &str) {\n-    slow(s);\n+    fast(s);\n+    check(s);\n }\n"
ROUTES = {
    "/search/issues?q=is%3Apr+is%3Aopen+review-requested%3A%40me&per_page=50": {"items": [PR]},
    "/repos/acme/widgets/pulls/7": {"head": {"sha": "a" * 40}, "base": {"sha": "b" * 40}},
    "/repos/acme/widgets/pulls/7/files?per_page=300": [
        {"filename": "src/parse.rs", "status": "modified", "additions": 2, "deletions": 1, "patch": PATCH}
    ],
}


class Handler(BaseHTTPRequestHandler):
    def log_message(self, *args):
        pass

    def record(self, body=None):
        with open(LOG, "a") as f:
            f.write(json.dumps({
                "method": self.command,
                "path": self.path,
                "auth": self.headers.get("Authorization", ""),
                "body": body,
            }) + "\n")

    def reply(self, status, value):
        data = json.dumps(value).encode()
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def authorized(self):
        return self.headers.get("Authorization") == "Bearer e2e-token"

    def do_GET(self):
        self.record()
        if not self.authorized():
            return self.reply(401, {"message": "Bad credentials"})
        if self.path in ROUTES:
            return self.reply(200, ROUTES[self.path])
        self.reply(404, {"message": "Not Found"})

    def do_POST(self):
        length = int(self.headers.get("Content-Length", "0"))
        body = json.loads(self.rfile.read(length) or b"null")
        self.record(body)
        if not self.authorized():
            return self.reply(401, {"message": "Bad credentials"})
        if self.path == "/repos/acme/widgets/pulls/7/reviews":
            return self.reply(200, {"id": 1})
        self.reply(404, {"message": "Not Found"})


server = HTTPServer(("127.0.0.1", 0), Handler)
with open(PORT_FILE, "w") as f:
    f.write(str(server.server_address[1]))
server.serve_forever()
