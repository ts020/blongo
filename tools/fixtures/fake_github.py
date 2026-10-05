#!/usr/bin/env python3
"""A stateful fake GitHub API on loopback for the pull request tests.

Usage: fake_github.py PORT_FILE
Writes the chosen port to PORT_FILE. Never talks to the real GitHub.

State is a dict of repositories ("owner/name" -> repo). Tests change it
with POST /__control (JSON: {"op": ..., ...}) and read every request made
so far with GET /__log. Requests must carry "Authorization: Bearer
test-token" (anything else gets 401), except /__control and /__log.

Supported API (only what Blongo asks):
  GET  /repos/O/N
  GET  /repos/O/N/pulls/NUM
  GET  /repos/O/N/pulls?state=all&per_page=5&head=O%3ABRANCH
  POST /graphql   rateLimit + repository(owner,name){ pNUM: pullRequest(number:NUM){...} }

Control ops:
  {"op": "repo", "repo": "o/n", "default_branch": "main", "push": true}
  {"op": "pull", "repo": "o/n", "pull": {number, title, head, base, ...}}
      merges the given fields into the pull (created if missing). Fields:
      number, title, state ("open"/"closed"), draft, merged, head (branch),
      head_repo ("o/n"), head_sha, base, mergeable ("MERGEABLE"/
      "CONFLICTING"/"UNKNOWN"), review (null/"APPROVED"/...),
      checks: [{"name", "status", "conclusion"} | {"name", "state"}],
      threads: [true/false resolved flags]
  {"op": "rate", "limit": 5000, "remaining": 4000}
  {"op": "fail", "path_prefix": "/graphql", "status": 502, "count": 1}
  {"op": "clear_log"}
"""
import json
import re
import sys
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.parse import parse_qs, unquote, urlsplit

PORT_FILE = sys.argv[1]
TOKEN = "Bearer test-token"

LOCK = threading.Lock()
REPOS = {}
LOG = []
RATE = {"limit": 5000, "remaining": 4999}
FAILS = []


def repo(name):
    if name not in REPOS:
        REPOS[name] = {"default_branch": "main", "push": True, "pulls": {}}
    return REPOS[name]


def pull_defaults(full, number):
    return {
        "number": number,
        "title": f"Pull {number}",
        "state": "open",
        "draft": False,
        "merged": False,
        "head": f"branch-{number}",
        "head_repo": full,
        "head_sha": "a" * 40,
        "base": repo(full)["default_branch"],
        "mergeable": "MERGEABLE",
        "review": None,
        "checks": [],
        "threads": [],
    }


def rest_pull(full, p):
    host = "github.com"
    return {
        "number": p["number"],
        "title": p["title"],
        "state": p["state"],
        "draft": p["draft"],
        "merged": p["merged"],
        "merged_at": "2026-10-04T00:00:00Z" if p["merged"] else None,
        "html_url": f"https://{host}/{full}/pull/{p['number']}",
        "head": {
            "ref": p["head"],
            "sha": p["head_sha"],
            "repo": {"full_name": p["head_repo"]} if p["head_repo"] else None,
        },
        "base": {"ref": p["base"], "sha": "b" * 40},
    }


def graphql_pull(p):
    if p["merged"]:
        state = "MERGED"
    elif p["state"] == "closed":
        state = "CLOSED"
    else:
        state = "OPEN"
    contexts = []
    for c in p["checks"]:
        if "state" in c:
            contexts.append({"__typename": "StatusContext", "context": c.get("name", ""), "state": c["state"]})
        else:
            contexts.append({
                "__typename": "CheckRun",
                "name": c.get("name", ""),
                "status": c.get("status", "COMPLETED"),
                "conclusion": c.get("conclusion"),
            })
    rollup = None
    if contexts:
        failed = any(
            (x.get("conclusion") in ("FAILURE", "TIMED_OUT", "CANCELLED")) or x.get("state") in ("FAILURE", "ERROR")
            for x in contexts
        )
        pending = any(
            (x["__typename"] == "CheckRun" and x.get("status") != "COMPLETED") or x.get("state") in ("PENDING", "EXPECTED")
            for x in contexts
        )
        rollup = {
            "state": "FAILURE" if failed else "PENDING" if pending else "SUCCESS",
            "contexts": {"nodes": contexts},
        }
    return {
        "number": p["number"],
        "title": p["title"],
        "state": state,
        "isDraft": p["draft"],
        "headRefOid": p["head_sha"],
        "mergeable": p["mergeable"],
        "reviewDecision": p["review"],
        "commits": {"nodes": [{"commit": {"statusCheckRollup": rollup}}]},
        "reviewThreads": {"nodes": [{"isResolved": r} for r in p["threads"]]},
    }


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *args):
        pass

    def reply(self, status, value):
        data = json.dumps(value).encode()
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def body(self):
        length = int(self.headers.get("Content-Length", "0"))
        raw = self.rfile.read(length) if length else b""
        return json.loads(raw or b"null")

    def gate(self, body=None):
        """Log the request; answer auth failures and injected failures.
        Returns True when the request was answered."""
        LOG.append({
            "method": self.command,
            "path": self.path,
            "auth": self.headers.get("Authorization", ""),
            "body": body,
        })
        if self.headers.get("Authorization") != TOKEN:
            self.reply(401, {"message": "Bad credentials"})
            return True
        for f in FAILS:
            if f["count"] > 0 and self.path.startswith(f["path_prefix"]):
                f["count"] -= 1
                self.reply(f["status"], {"message": f.get("message", "injected failure")})
                return True
        return False

    def do_GET(self):
        with LOCK:
            if self.path == "/__log":
                return self.reply(200, LOG)
            if self.gate():
                return
            url = urlsplit(self.path)
            m = re.fullmatch(r"/repos/([^/]+)/([^/]+)", url.path)
            if m:
                full = f"{m[1]}/{m[2]}"
                if full not in REPOS:
                    return self.reply(404, {"message": "Not Found"})
                r = REPOS[full]
                return self.reply(200, {
                    "full_name": full,
                    "default_branch": r["default_branch"],
                    "permissions": {"admin": False, "push": r["push"], "pull": True},
                    "allow_merge_commit": True,
                    "allow_squash_merge": True,
                    "allow_rebase_merge": False,
                    "delete_branch_on_merge": False,
                })
            m = re.fullmatch(r"/repos/([^/]+)/([^/]+)/pulls/(\d+)", url.path)
            if m:
                full = f"{m[1]}/{m[2]}"
                p = REPOS.get(full, {}).get("pulls", {}).get(int(m[3]))
                if not p:
                    return self.reply(404, {"message": "Not Found"})
                return self.reply(200, rest_pull(full, p))
            m = re.fullmatch(r"/repos/([^/]+)/([^/]+)/pulls", url.path)
            if m:
                full = f"{m[1]}/{m[2]}"
                q = parse_qs(url.query)
                head = unquote(q.get("head", [""])[0])
                owner, _, branch = head.partition(":")
                pulls = [
                    rest_pull(full, p)
                    for p in sorted(REPOS.get(full, {}).get("pulls", {}).values(), key=lambda p: -p["number"])
                    if p["head"] == branch and (p["head_repo"] or "").split("/")[0] == owner
                ]
                return self.reply(200, pulls)
            self.reply(404, {"message": "Not Found"})

    def do_POST(self):
        body = self.body()
        with LOCK:
            if self.path == "/__control":
                return self.control(body)
            if self.gate(body):
                return
            if self.path == "/graphql":
                return self.graphql(body.get("query", ""))
            self.reply(404, {"message": "Not Found"})

    def graphql(self, query):
        RATE["remaining"] = max(0, RATE["remaining"] - 1)
        m = re.search(r'repository\(owner:"([^"]+)",name:"([^"]+)"\)', query)
        data = {"rateLimit": dict(RATE)}
        if not m or f"{m[1]}/{m[2]}" not in REPOS:
            data["repository"] = None
            return self.reply(200, {
                "data": data,
                "errors": [{"type": "NOT_FOUND", "message": "Could not resolve to a Repository"}],
            })
        full = f"{m[1]}/{m[2]}"
        pulls = REPOS[full]["pulls"]
        out = {}
        errors = []
        for alias, number in re.findall(r"(p\d+):pullRequest\(number:(\d+)\)", query):
            p = pulls.get(int(number))
            if p:
                out[alias] = graphql_pull(p)
            else:
                out[alias] = None
                errors.append({"type": "NOT_FOUND", "path": ["repository", alias]})
        data["repository"] = out
        resp = {"data": data}
        if errors:
            resp["errors"] = errors
        self.reply(200, resp)

    def control(self, body):
        op = body.get("op")
        if op == "repo":
            r = repo(body["repo"])
            for k in ("default_branch", "push"):
                if k in body:
                    r[k] = body[k]
        elif op == "pull":
            full = body["repo"]
            fields = body["pull"]
            number = int(fields["number"])
            pulls = repo(full)["pulls"]
            p = pulls.get(number) or pull_defaults(full, number)
            p.update(fields)
            pulls[number] = p
        elif op == "rate":
            RATE.update({k: body[k] for k in ("limit", "remaining") if k in body})
        elif op == "fail":
            FAILS.append({
                "path_prefix": body["path_prefix"],
                "status": body.get("status", 502),
                "count": body.get("count", 1),
                "message": body.get("message", "injected failure"),
            })
        elif op == "clear_log":
            LOG.clear()
        else:
            return self.reply(400, {"message": f"unknown op {op}"})
        self.reply(200, {"ok": True})


server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
with open(PORT_FILE + ".tmp", "w") as f:
    f.write(str(server.server_address[1]))
import os  # noqa: E402

os.replace(PORT_FILE + ".tmp", PORT_FILE)
server.serve_forever()
