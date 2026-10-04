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
  PATCH /repos/O/N/pulls/NUM   {title, body}
  POST /repos/O/N/pulls        {title, body, head, base, draft} (422 when an
                               open pull has that head already)
  GET  /repos/O/N/commits/SHA/check-runs   checks of the pull whose head is SHA
  GET  /repos/O/N/check-runs/ID/annotations
  GET  /repos/O/N/actions/jobs/ID/logs     302 to /__blob/ID (served without
                                           a token; requests are logged)
  GET  /repos/O/N/commits/SHA/status       status checks (those with "state")
  Checks may carry "summary", "annotations": [{"path", "start_line",
  "message"}] and "log" (text) for these.
  POST /graphql   rateLimit + repository(owner,name){ pNUM: pullRequest(number:NUM){...} }
                  repository(owner,name){ pullRequest(number:NUM){...detail...} }

Control ops:
  {"op": "repo", "repo": "o/n", "default_branch": "main", "push": true}
      also "allow_merge_commit", "allow_squash_merge", "allow_rebase_merge",
      "delete_branch_on_merge". PUT .../pulls/N/merge merges (409 when
      "sha" is not the head, 405 when not mergeable or the method is not
      allowed); the GraphQL enablePullRequestAutoMerge mutation sets
      "auto_merge"; DELETE .../git/refs/heads/B is logged.
  {"op": "ref", "repo": "o/n", "branch": "b", "sha": "..."}
      what GET .../git/ref/heads/b answers (default: the head of the
      pull request from b).
  {"op": "pull", "repo": "o/n", "pull": {number, title, head, base, ...}}
      merges the given fields into the pull (created if missing). Fields:
      number, title, state ("open"/"closed"), draft, merged, head (branch),
      head_repo ("o/n"), head_sha, base, mergeable ("MERGEABLE"/
      "CONFLICTING"/"UNKNOWN"), review (null/"APPROVED"/...),
      checks: [{"name", "status", "conclusion"} | {"name", "state"}],
      threads: [true/false resolved flags | {"resolved", "path", "line",
               "outdated", "comments": [{"author", "body"}]}],
      body, author, merge_state ("CLEAN"/"BLOCKED"/...), reviews:
      [{"author", "state"}], can_update, checks may also carry "url",
      "workflow", "started_at", "completed_at"
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
        REPOS[name] = {"default_branch": "main", "push": True, "pulls": {},
                       "allow_merge_commit": True, "allow_squash_merge": True,
                       "allow_rebase_merge": False, "delete_branch_on_merge": False,
                       "deleted_refs": []}
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
        "body": "",
        "author": "octocat",
        "merge_state": "CLEAN",
        "reviews": [],
        "can_update": True,
        "additions": 1,
        "deletions": 0,
        "changed_files": 1,
    }


def thread(t):
    if isinstance(t, dict):
        return {"resolved": False, "path": "README.md", "line": 1, "outdated": False, "comments": [], **t}
    return {"resolved": t, "path": "README.md", "line": 1, "outdated": False, "comments": []}


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
        "reviewThreads": {"nodes": [{"isResolved": thread(t)["resolved"]} for t in p["threads"]]},
    }


def graphql_detail(full, p):
    out = graphql_pull(p)
    rollup = out["commits"]["nodes"][0]["commit"]["statusCheckRollup"]
    if rollup:
        for node, c in zip(rollup["contexts"]["nodes"], p["checks"]):
            if node["__typename"] == "CheckRun":
                node["detailsUrl"] = c.get("url", "")
                node["startedAt"] = c.get("started_at")
                node["completedAt"] = c.get("completed_at")
                node["checkSuite"] = {"workflowRun": {"workflow": {"name": c["workflow"]}}} if c.get("workflow") else None
            else:
                node["targetUrl"] = c.get("url", "")
                node["description"] = c.get("description")
                node["createdAt"] = c.get("started_at")
        rollup["contexts"]["totalCount"] = len(p["checks"])
    threads = [thread(t) for t in p["threads"]]
    out.update({
        "id": f"PR_{p['number']}",
        "autoMergeRequest": {"enabledAt": "2026-10-04T00:00:00Z"} if p.get("auto_merge") else None,
        "body": p["body"],
        "url": f"https://github.com/{full}/pull/{p['number']}",
        "author": {"login": p["author"]},
        "mergeStateStatus": p["merge_state"],
        "additions": p["additions"],
        "deletions": p["deletions"],
        "changedFiles": p["changed_files"],
        "viewerCanUpdate": p["can_update"],
        "latestReviews": {"nodes": [{"author": {"login": r["author"]}, "state": r["state"]} for r in p["reviews"]]},
        "reviewThreads": {"totalCount": len(threads), "nodes": [{
            "id": f"T{i}",
            "isResolved": t["resolved"],
            "isOutdated": t["outdated"],
            "path": t["path"],
            "line": t["line"],
            "comments": {"totalCount": len(t["comments"]), "nodes": [
                {"author": {"login": c.get("author", "rev")}, "body": c.get("body", ""), "createdAt": "2026-10-04T00:00:00Z"}
                for c in t["comments"]
            ]},
        } for i, t in enumerate(threads)]},
    })
    return out


def check_run(p, i, c):
    return {
        "id": p["number"] * 100 + i,
        "name": c["name"],
        "status": c["status"].lower(),
        "conclusion": (c.get("conclusion") or "").lower() or None,
        "details_url": c.get("url"),
        "app": {"slug": "github-actions" if c.get("workflow") else "other"},
        "output": {"title": c.get("summary"), "summary": None, "text": None},
    }


def find_check(run_id):
    number, i = divmod(run_id, 100)
    for r in REPOS.values():
        p = r["pulls"].get(number)
        if p and i < len(p["checks"]):
            return p, p["checks"][i]
    return None


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
            m = re.fullmatch(r"/__blob/(\d+)", self.path)
            if m:
                LOG.append({"method": "GET", "path": self.path,
                            "auth": self.headers.get("Authorization", ""), "body": None})
                c = find_check(int(m[1]))
                data = (c[1].get("log", "") if c else "").encode()
                self.send_response(200 if c else 404)
                self.send_header("Content-Type", "text/plain")
                self.send_header("Content-Length", str(len(data)))
                self.end_headers()
                self.wfile.write(data)
                return
            if self.gate():
                return
            url = urlsplit(self.path)
            m = re.fullmatch(r"/repos/([^/]+)/([^/]+)/commits/([0-9a-f]+)/check-runs", url.path)
            if m:
                runs = []
                for p in REPOS.get(f"{m[1]}/{m[2]}", {}).get("pulls", {}).values():
                    if p["head_sha"] != m[3]:
                        continue
                    for i, c in enumerate(p["checks"]):
                        if "status" in c:
                            runs.append(check_run(p, i, c))
                return self.reply(200, {"total_count": len(runs), "check_runs": runs})
            m = re.fullmatch(r"/repos/([^/]+)/([^/]+)/commits/([0-9a-f]+)/status", url.path)
            if m:
                statuses = []
                for p in REPOS.get(f"{m[1]}/{m[2]}", {}).get("pulls", {}).values():
                    if p["head_sha"] == m[3]:
                        statuses += [{"context": c["name"], "state": c["state"].lower(),
                                      "description": c.get("description", "")}
                                     for c in p["checks"] if "state" in c]
                return self.reply(200, {"statuses": statuses})
            m = re.fullmatch(r"/repos/[^/]+/[^/]+/check-runs/(\d+)/annotations", url.path)
            if m:
                c = find_check(int(m[1]))
                return self.reply(200, c[1].get("annotations", []) if c else [])
            m = re.fullmatch(r"/repos/[^/]+/[^/]+/actions/jobs/(\d+)/logs", url.path)
            if m:
                if not find_check(int(m[1])):
                    return self.reply(404, {"message": "Not Found"})
                self.send_response(302)
                self.send_header("Location", f"http://{self.headers['Host']}/__blob/{m[1]}")
                self.send_header("Content-Length", "0")
                self.end_headers()
                return
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
                    "allow_merge_commit": r["allow_merge_commit"],
                    "allow_squash_merge": r["allow_squash_merge"],
                    "allow_rebase_merge": r["allow_rebase_merge"],
                    "delete_branch_on_merge": r["delete_branch_on_merge"],
                })
            m = re.fullmatch(r"/repos/([^/]+)/([^/]+)/git/ref/heads/(.+)", url.path)
            if m:
                r = REPOS.get(f"{m[1]}/{m[2]}", {})
                branch = unquote(m[3])
                sha = None
                if branch not in r.get("deleted_refs", []):
                    sha = r.get("refs", {}).get(branch) or next(
                        (p["head_sha"] for p in r.get("pulls", {}).values() if p["head"] == branch), None)
                if not sha:
                    return self.reply(404, {"message": "Not Found"})
                return self.reply(200, {"ref": f"refs/heads/{branch}", "object": {"sha": sha, "type": "commit"}})
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
            m = re.fullmatch(r"/repos/([^/]+)/([^/]+)/pulls", urlsplit(self.path).path)
            if m and f"{m[1]}/{m[2]}" in REPOS:
                return self.create_pull(f"{m[1]}/{m[2]}", body)
            self.reply(404, {"message": "Not Found"})

    def create_pull(self, full, body):
        r = REPOS[full]
        if not r["push"]:
            return self.reply(403, {"message": "Resource not accessible by integration"})
        missing = [k for k in ("title", "head", "base") if not body.get(k)]
        if missing:
            return self.reply(422, {"message": "Validation Failed",
                                    "errors": [{"code": "missing_field", "field": f} for f in missing]})
        if any(p["head"] == body["head"] and p["state"] == "open" for p in r["pulls"].values()):
            return self.reply(422, {"message": "Validation Failed", "errors": [
                {"message": f"A pull request already exists for {full.split('/')[0]}:{body['head']}."}]})
        number = max(r["pulls"], default=0) + 1
        p = pull_defaults(full, number)
        p.update({"title": body["title"], "body": body.get("body") or "", "head": body["head"],
                  "base": body["base"], "draft": bool(body.get("draft"))})
        r["pulls"][number] = p
        self.reply(201, rest_pull(full, p))

    def do_PUT(self):
        body = self.body()
        with LOCK:
            if self.gate(body):
                return
            m = re.fullmatch(r"/repos/([^/]+)/([^/]+)/pulls/(\d+)/merge", urlsplit(self.path).path)
            full = f"{m[1]}/{m[2]}" if m else ""
            p = REPOS.get(full, {}).get("pulls", {}).get(int(m[3])) if m else None
            if not p:
                return self.reply(404, {"message": "Not Found"})
            r = REPOS[full]
            if not r["push"]:
                return self.reply(403, {"message": "Resource not accessible by integration"})
            method = (body or {}).get("merge_method", "merge")
            allowed = {"merge": r["allow_merge_commit"], "squash": r["allow_squash_merge"],
                       "rebase": r["allow_rebase_merge"]}
            if p["merged"] or p["state"] != "open" or p["draft"] or p["mergeable"] == "CONFLICTING":
                return self.reply(405, {"message": "Pull Request is not mergeable"})
            if not allowed.get(method):
                return self.reply(405, {"message": f"{method.capitalize()} merges are not allowed on this repository."})
            if (body or {}).get("sha") and body["sha"] != p["head_sha"]:
                return self.reply(409, {"message": "Head branch was modified. Review and try the merge again."})
            p.update({"merged": True, "state": "closed", "merged_with": method})
            self.reply(200, {"sha": "c" * 40, "merged": True, "message": "Pull Request successfully merged"})

    def do_DELETE(self):
        with LOCK:
            if self.gate():
                return
            m = re.fullmatch(r"/repos/([^/]+)/([^/]+)/git/refs/heads/(.+)", urlsplit(self.path).path)
            full = f"{m[1]}/{m[2]}" if m else ""
            if full not in REPOS:
                return self.reply(404, {"message": "Not Found"})
            REPOS[full]["deleted_refs"].append(unquote(m[3]))
            self.send_response(204)
            self.send_header("Content-Length", "0")
            self.end_headers()

    def do_PATCH(self):
        body = self.body()
        with LOCK:
            if self.gate(body):
                return
            m = re.fullmatch(r"/repos/([^/]+)/([^/]+)/pulls/(\d+)", urlsplit(self.path).path)
            full = f"{m[1]}/{m[2]}" if m else ""
            p = REPOS.get(full, {}).get("pulls", {}).get(int(m[3])) if m else None
            if not p:
                return self.reply(404, {"message": "Not Found"})
            if not REPOS[full]["push"]:
                return self.reply(403, {"message": "Resource not accessible by integration"})
            for k in ("title", "body"):
                if k in body:
                    p[k] = body[k]
            self.reply(200, rest_pull(full, p))

    def graphql(self, query):
        RATE["remaining"] = max(0, RATE["remaining"] - 1)
        m = re.search(r'enablePullRequestAutoMerge\(input:\{pullRequestId:"PR_(\d+)",'
                      r'mergeMethod:(\w+),expectedHeadOid:"([0-9a-f]+)"\}\)', query)
        if query.startswith("mutation"):
            p = None
            for r in REPOS.values():
                if m and int(m[1]) in r["pulls"]:
                    p = r["pulls"][int(m[1])]
            if not p:
                return self.reply(200, {"data": None, "errors": [
                    {"type": "NOT_FOUND", "message": "Could not resolve to a node"}]})
            if p["head_sha"] != m[3]:
                return self.reply(200, {"data": None, "errors": [
                    {"type": "UNPROCESSABLE", "message": "Head sha didn't match expected head sha"}]})
            p["auto_merge"] = m[2]
            return self.reply(200, {"data": {"enablePullRequestAutoMerge": {"clientMutationId": None}}})
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
        single = re.search(r"\{pullRequest\(number:(\d+)\)", query)
        if single:
            p = pulls.get(int(single[1]))
            data["repository"] = {"pullRequest": graphql_detail(full, p) if p else None}
            resp = {"data": data}
            if not p:
                resp["errors"] = [{"type": "NOT_FOUND", "message": "Could not resolve to a PullRequest"}]
            return self.reply(200, resp)
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
            for k in ("default_branch", "push", "allow_merge_commit", "allow_squash_merge",
                      "allow_rebase_merge", "delete_branch_on_merge"):
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
        elif op == "ref":
            repo(body["repo"]).setdefault("refs", {})[body["branch"]] = body["sha"]
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
