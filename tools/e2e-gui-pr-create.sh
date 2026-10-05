#!/usr/bin/env bash
# Creating a pull request from the GUI against a fake GitHub on loopback:
# a worktree thread starts from GitHub's base branch, the Create PR form
# (alt-p) reads the branch, the agent drafts the text, Create commits,
# renames, pushes (to a local bare repository) and opens the pull request,
# and the tab turns into the PR tab. Then the GitHub settings tab.
#
# Usage: tools/e2e-gui-pr-create.sh BINARY OUT_DIR
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
export BLONGO_GH=/bin/false
export GIT_AUTHOR_NAME=e2e GIT_AUTHOR_EMAIL=e2e@blongo GIT_COMMITTER_NAME=e2e GIT_COMMITTER_EMAIL=e2e@blongo

# A project whose origin is github.com/acme/widgets, rewritten to a local
# bare repository. GitHub's main moves on after the project last fetched.
git -C "$WORK/remote.git" init -q --bare -b main
cd "$WORK/widgets"
git init -q -b main
git config "url.$WORK/remote.git.insteadOf" https://github.com/acme/widgets.git
git remote add origin https://github.com/acme/widgets.git
printf '# widgets\n' >README.md
git add README.md
git commit -qm init
git push -q -u origin main
git clone -q "$WORK/remote.git" "$WORK/other"
cd "$WORK/other"
printf 'upstream\n' >upstream.txt
git add upstream.txt
git commit -qm "Upstream change"
git push -q origin main
cd "$ROOT"

python3 "$ROOT/tools/fixtures/fake_github.py" "$WORK/github.port" &
GH_PID=$!
for _ in $(seq 1 300); do [ -s "$WORK/github.port" ] && break; sleep 0.1; done
API="http://127.0.0.1:$(cat "$WORK/github.port")"
export BLONGO_GITHUB_API="$API"
control() { curl -sf -X POST -H 'Content-Type: application/json' -d "$1" "$API/__control" >/dev/null; }
control '{"op": "repo", "repo": "acme/widgets", "default_branch": "main"}'

cat >"$WORK/config/forge.json" <<EOF
{ "forges": [ { "kind": "github", "api": "https://api.github.com", "token": "test-token" } ] }
EOF
chmod 600 "$WORK/config/"*.json

DISP=:${E2E_DISPLAY:-95}
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
  rm -rf "$WORK"
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
gh_log() { curl -sf "$API/__log"; }

echo "1. a worktree thread starts from GitHub's main"
launch
typ "$WORK/widgets"; xdotool key Return; sleep 1.5
click 150 60; sleep 2                      # "+ Worktree"
typ "echo: fix the parser"; xdotool key Return
wait_for 10 "the worktree thread" has "SELECT id FROM threads WHERE worktree_branch IS NOT NULL"
WT=$(sql "SELECT worktree_path FROM threads WHERE worktree_branch IS NOT NULL")
BRANCH=$(sql "SELECT worktree_branch FROM threads WHERE worktree_branch IS NOT NULL")
[ -f "$WT/upstream.txt" ] || fail "the worktree did not start from origin/main"
echo "  $BRANCH at $WT"
sleep 2
printf 'fn parse(s: &str) {}\n' >"$WT/parser.rs"

echo "2. Create PR (alt-p) reads the branch"
xdotool key alt+p; sleep 2
ran pr.create || fail "alt-p did not run pr.create"
wait_for 10 "the prepare call" sh -c "curl -sf '$API/__log' | grep -q '\"path\": \"/repos/acme/widgets\"'"
sleep 1
shot create-01-form

echo "3. the agent drafts the title and description"
click 1150 70; sleep 0.5                   # Ask agent to draft
wait_for 15 "the draft" sh -c "grep -q 'run .* finished' '$WORK/app.log'"
sleep 2
shot create-02-drafted

echo "4. Draft, then Commit, push and create"
click 476 564; sleep 0.4                   # Draft checkbox
click 372 564                              # Commit, push and create
wait_for 20 "the link" has "SELECT id FROM threads WHERE pr IS NOT NULL"
NEW=$(sql "SELECT worktree_branch FROM threads WHERE pr IS NOT NULL")
[ "$NEW" = "blongo/echo-fix-the-parser" ] || fail "the branch was not renamed: $NEW"
[ "$(git -C "$WORK/remote.git" rev-parse refs/heads/$NEW)" = "$(git -C "$WT" rev-parse HEAD)" ] \
  || fail "the branch was not pushed"
[ "$(git -C "$WT" log -1 --format=%s)" = "Handle empty input" ] || fail "not committed with the draft message"
gh_log | python3 -c '
import json, sys
post = [r for r in json.load(sys.stdin) if r["method"] == "POST" and r["path"] == "/repos/acme/widgets/pulls"]
assert len(post) == 1, post
b = post[0]["body"]
assert b["title"] == "Handle empty input in the parser", b
assert b["head"] == "blongo/echo-fix-the-parser" and b["base"] == "main" and b["draft"] is True, b
assert "Empty input crashed" in b["body"], b
print("  created", b["head"], "->", b["base"], "(draft)")'
sleep 3
shot create-03-created

echo "5. GitHub settings for the project"
click 142 19; sleep 1                      # Settings
click 300 198; sleep 0.8                   # GitHub
shot create-04-settings
click 950 150; sleep 0.3                   # the base branch field
typ "develop"; xdotool key Return
wait_for 10 "the base setting" has "SELECT id FROM projects WHERE forge LIKE '%develop%'"
sleep 1
shot create-05-base

xdotool key ctrl+q
for _ in $(seq 1 50); do kill -0 "$PID" 2>/dev/null || break; sleep 0.2; done
PID=
cp "$WORK/app.log" "$OUT/app.log"
rm -rf "$WORK"
echo "done: $OUT"
