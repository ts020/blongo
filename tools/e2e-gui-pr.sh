#!/usr/bin/env bash
# GitHub pull request GUI end-to-end run against a fake GitHub on loopback:
# link a thread to its pull request (header chip, sidebar badge), the PR
# tab (merge readiness, checks, reviews, conversations), editing the title
# and description, a status change arriving by refresh, unlinking.
#
# Usage: tools/e2e-gui-pr.sh BINARY OUT_DIR
#
# Starts its own Xvfb (lavapipe for Vulkan) and stops it, the app and the
# fake GitHub by PID. Needs xdotool, ImageMagick's `import`, python3, git.
# Click coordinates assume the default 1280x800 window.
set -euo pipefail

BIN=$(realpath "$1")
OUT=$(realpath -m "$2")
ROOT=$(cd "$(dirname "$0")/.." && pwd)
WORK=$(mktemp -d)
mkdir -p "$OUT" "$WORK/data" "$WORK/config" "$WORK/remote.git" "$WORK/widgets"
export VK_ICD_FILENAMES=${VK_ICD_FILENAMES:-/usr/share/vulkan/icd.d/lvp_icd.json}
export LP_NUM_THREADS=${LP_NUM_THREADS:-4}
export BLONGO_DATA_DIR="$WORK/data"
export BLONGO_CONFIG_DIR="$WORK/config"
FIX="$ROOT/crates/blongo-harness/tests/fixtures"
export BLONGO_CODEX_EXE="$FIX/fake_codex.py"
export BLONGO_CLAUDE_EXE="$FIX/fake_claude.py"
export BLONGO_ANTIGRAVITY_EXE="$FIX/fake_acp.py"
export FAKE_CODEX_DELAY_MS=40
export BLONGO_TRACE_COMMANDS=1
export BLONGO_NOTIFY_CMD="$WORK/notify.sh"
# No real gh: tokens come from forge.json only.
export BLONGO_GH=/bin/false
cat >"$BLONGO_NOTIFY_CMD" <<EOF
#!/bin/sh
printf '%s|%s\n' "\$1" "\$2" >> "$WORK/notify.log"
EOF
chmod +x "$BLONGO_NOTIFY_CMD"

# A project whose origin is github.com/acme/widgets, rewritten to a local
# bare repository; the branch fast-parser is pushed.
git -C "$WORK/remote.git" init -q --bare -b main
cd "$WORK/widgets"
git init -q -b main
git config user.email e2e@blongo
git config user.name e2e
git config "url.$WORK/remote.git.insteadOf" https://github.com/acme/widgets.git
git remote add origin https://github.com/acme/widgets.git
printf '# widgets\n' >README.md
git add README.md
git commit -qm init
git push -q -u origin main
git switch -q -c fast-parser
printf 'fn parse() {}\n' >parse.rs
git add parse.rs
git commit -qm "Speed up the parser"
git push -q -u origin fast-parser
cd "$ROOT"

# The fake GitHub.
python3 "$ROOT/tools/fixtures/fake_github.py" "$WORK/github.port" &
GH_PID=$!
for _ in $(seq 1 300); do [ -s "$WORK/github.port" ] && break; sleep 0.1; done
API="http://127.0.0.1:$(cat "$WORK/github.port")"
export BLONGO_GITHUB_API="$API"
control() { curl -sf -X POST -H 'Content-Type: application/json' -d "$1" "$API/__control" >/dev/null; }
control '{"op": "repo", "repo": "acme/widgets", "default_branch": "main"}'
control '{"op": "pull", "repo": "acme/widgets", "pull": {
  "number": 7, "title": "Speed up the parser", "head": "fast-parser",
  "body": "Parses twice as fast.\n\nCloses #3.",
  "merge_state": "BLOCKED", "review": "CHANGES_REQUESTED",
  "reviews": [{"author": "alice", "state": "CHANGES_REQUESTED"}, {"author": "bob", "state": "APPROVED"}],
  "checks": [
    {"name": "test (ubuntu)", "workflow": "CI", "status": "COMPLETED", "conclusion": "FAILURE",
     "url": "https://github.com/acme/widgets/actions/runs/1/job/2",
     "started_at": "2026-10-04T00:00:00Z", "completed_at": "2026-10-04T00:03:12Z"},
    {"name": "clippy", "workflow": "CI", "status": "COMPLETED", "conclusion": "SUCCESS",
     "started_at": "2026-10-04T00:00:00Z", "completed_at": "2026-10-04T00:01:05Z"},
    {"name": "docs", "state": "PENDING", "description": "Building preview"}
  ],
  "threads": [
    {"path": "parse.rs", "line": 1, "comments": [
      {"author": "alice", "body": "Please handle empty input here."},
      {"author": "e2e", "body": "Will do."}]},
    {"resolved": true, "path": "README.md", "line": 1, "comments": [{"author": "bob", "body": "Typo"}]}
  ]
}}'

cat >"$WORK/config/forge.json" <<EOF
{ "forges": [ { "kind": "github", "api": "https://api.github.com", "token": "test-token" } ] }
EOF
printf '{ "notifications": "always" }\n' >"$WORK/config/settings.json"
chmod 600 "$WORK/config/"*.json

DISP=:${E2E_DISPLAY:-96}
Xvfb "$DISP" -screen 0 1920x1080x24 >/dev/null 2>&1 &
XVFB_PID=$!
export DISPLAY=$DISP
sleep 1

PID=
WIN=
cleanup() {
  [ -n "$PID" ] && kill "$PID" 2>/dev/null || true
  kill "$GH_PID" 2>/dev/null || true
  kill "$XVFB_PID" 2>/dev/null || true
}
trap cleanup EXIT

fail() {
  echo "FAIL: $*" >&2
  [ -n "$WIN" ] && timeout 10 import -window "$WIN" "$OUT/failed.png" 2>/dev/null || true
  cp "$WORK/app.log" "$OUT/app.log" 2>/dev/null || true
  exit 1
}
launch() {
  (cd "$WORK" && exec "$BIN" >>"$WORK/app.log" 2>&1) &
  PID=$!
  for _ in $(seq 1 100); do
    WIN=$(xdotool search --name '^Blongo$' 2>/dev/null | head -1 || true)
    [ -n "$WIN" ] && break
    sleep 0.1
  done
  [ -n "$WIN" ] || fail "no window"
  sleep 1.5
}
shot() { timeout 10 import -window "$WIN" "$OUT/$1.png"; echo "  $1.png"; }
click() { xdotool mousemove --window "$WIN" "$1" "$2" click 1; }
typ() { xdotool type --delay 15 "$1"; }
ran() { grep -q "blongo: command $1\$" "$WORK/app.log"; }
sql() { python3 - "$WORK/data/blongo.sqlite" "$1" <<'EOF'
import sqlite3, sys
con = sqlite3.connect(f"file:{sys.argv[1]}?mode=ro", uri=True)
for row in con.execute(sys.argv[2]):
    print("|".join("" if v is None else str(v) for v in row))
EOF
}
has() { [ -n "$(sql "$1")" ]; }
wait_for() { # SECONDS DESCRIPTION COMMAND...
  local secs=$1 what=$2; shift 2
  for _ in $(seq 1 $((secs * 5))); do "$@" && return 0; sleep 0.2; done
  fail "timed out waiting for $what"
}
palette() { # run a command by its title
  xdotool key ctrl+shift+p; sleep 0.5
  typ "$1"; sleep 0.4
  xdotool key Return; sleep 0.6
}
gh_log() { curl -sf "$API/__log"; }

echo "1. a thread in the project"
launch
typ "$WORK/widgets"; xdotool key Return; sleep 1.5
typ "hello"; xdotool key Return
wait_for 10 "the thread" has "SELECT id FROM threads"
sleep 1

echo "2. link the pull request by number"
palette "Link a pull request"
ran pr.link || fail "pr.link did not run"
typ "#7"; xdotool key Return
wait_for 10 "the link" has "SELECT id FROM threads WHERE pr IS NOT NULL"
wait_for 10 "the status" has "SELECT id FROM threads WHERE pr_status LIKE '%Speed up the parser%'"
gh_log | grep -q '"auth": "Bearer test-token"' || fail "the token did not reach the fake GitHub"
sleep 1
shot pr-01-linked

echo "3. the PR tab"
xdotool key alt+r; sleep 2
ran view.pr || fail "alt-r did not open the PR tab"
shot pr-02-tab
gh_log | python3 -c '
import json, sys
log = json.load(sys.stdin)
assert any("{pullRequest(number:7)" in (r.get("body") or {}).get("query", "") for r in log), "no detail query"
print("  detail fetched")'

echo "4. show resolved conversations"
click 330 632; sleep 0.6                    # Show 1 resolved
shot pr-03-resolved

echo "5. edit the title (Enter saves to GitHub)"
click 1232 70; sleep 0.8                    # Edit
shot pr-04-editing
xdotool key ctrl+a; typ "Speed up the parser 2x"; sleep 0.3
xdotool key Return
wait_for 10 "the edit" sh -c "curl -sf '$API/__log' | grep -q '\"method\": \"PATCH\"'"
wait_for 10 "the new title" has "SELECT id FROM threads WHERE pr_status LIKE '%parser 2x%'"
sleep 1
shot pr-05-edited

echo "6. the status changes (checks pass, approved) and a refresh shows it"
control '{"op": "pull", "repo": "acme/widgets", "pull": {"number": 7, "review": "APPROVED",
  "merge_state": "CLEAN",
  "checks": [{"name": "test (ubuntu)", "workflow": "CI", "status": "COMPLETED", "conclusion": "SUCCESS"}],
  "threads": [true]}}'
palette "Check the pull request now"
wait_for 10 "the new status" has "SELECT id FROM threads WHERE pr_status LIKE '%\"success\"%'"
sleep 2
shot pr-06-ready
grep -q 'passed its checks\|was approved' "$WORK/notify.log" || fail "no notification for the status change"

echo "7. unlink"
palette "Unlink the pull request"
wait_for 10 "the unlink" has "SELECT id FROM threads WHERE pr IS NULL AND pr_dismissed = 1"
sleep 1
shot pr-07-unlinked

xdotool key ctrl+q
for _ in $(seq 1 50); do kill -0 "$PID" 2>/dev/null || break; sleep 0.2; done
PID=
cp "$WORK/app.log" "$OUT/app.log"
cp "$WORK/notify.log" "$OUT/notify.log" 2>/dev/null || true
rm -rf "$WORK"
echo "done: $OUT"
