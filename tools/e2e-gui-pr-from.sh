#!/usr/bin/env bash
# New thread from a pull request, an issue or a branch, end to end against
# a fake GitHub on loopback: the project's "From…" page lists them; an
# issue starts a named branch with the issue in the composer; a fork's
# pull request opens read-only; a remote branch is fetched.
#
# Usage: tools/e2e-gui-pr-from.sh BINARY OUT_DIR
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
# feature/a (a pull request's branch), feature/b (a branch only on
# GitHub) and a fork's pull request (refs/pull/4/head).
for b in feature/a feature/b fork; do
  git checkout -q -b "$b"
  printf '%s\n' "$b" >"${b//\//-}.txt"
  git add -A
  git commit -qm "Work on $b"
  if [ "$b" = fork ]; then git push -q origin fork:refs/pull/4/head; else git push -q origin "$b"; fi
  git checkout -q main
  git branch -q -D "$b"
done
FORK_SHA=$(git -C "$WORK/remote.git" rev-parse refs/pull/4/head)
A_SHA=$(git -C "$WORK/remote.git" rev-parse refs/heads/feature/a)
cd "$ROOT"

python3 "$ROOT/tools/fixtures/fake_github.py" "$WORK/github.port" &
GH_PID=$!
for _ in $(seq 1 300); do [ -s "$WORK/github.port" ] && break; sleep 0.1; done
API="http://127.0.0.1:$(cat "$WORK/github.port")"
export BLONGO_GITHUB_API="$API"
control() { curl -sf -X POST -H 'Content-Type: application/json' -d "$1" "$API/__control" >/dev/null; }
control '{"op": "repo", "repo": "acme/widgets", "default_branch": "main"}'
control '{"op": "pull", "repo": "acme/widgets", "pull": {"number": 3, "title": "Feature A", "head": "feature/a",
  "head_sha": "'"$A_SHA"'", "author": "hubot", "updated_at": "2026-10-03T10:00:00Z"}}'
control '{"op": "pull", "repo": "acme/widgets", "pull": {"number": 4, "title": "Fix a typo in the README",
  "head": "patch-1", "head_repo": "someone/widgets", "head_sha": "'"$FORK_SHA"'", "author": "someone",
  "reviewers": ["octocat"], "updated_at": "2026-10-02T10:00:00Z"}}'
control '{"op": "issue", "repo": "acme/widgets", "issue": {"number": 5, "title": "Crash on empty input",
  "body": "Parsing an empty file panics.\n\nSteps: run widgets on an empty file.", "author": "hubot",
  "assignees": ["octocat"], "updated_at": "2026-10-03T09:00:00Z"}}'
control '{"op": "issue", "repo": "acme/widgets", "issue": {"number": 6, "title": "Document the CLI flags",
  "author": "hubot", "updated_at": "2026-10-04T09:00:00Z"}}'

cat >"$WORK/config/forge.json" <<EOF
{ "forges": [ { "kind": "github", "api": "https://api.github.com", "token": "test-token" } ] }
EOF
chmod 600 "$WORK/config/"*.json

DISP=:${E2E_DISPLAY:-94}
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

# Click coordinates (default 1280x800 window).
FROM_X=84; TAB_Y=96; ROW1_Y=165

echo "1. the project's From… page lists pull requests"
launch
typ "$WORK/widgets"; xdotool key Return; sleep 1.5
shot from-01-sidebar
click "$FROM_X" 60; sleep 2                # "From…"
wait_for 10 "the lists" sh -c "curl -sf '$API/__log' | grep -q '/repos/acme/widgets/issues?'"
sleep 1
shot from-02-pulls

echo "2. an issue: a named branch, the issue in the composer"
click 420 "$TAB_Y"; sleep 0.8              # Issues
shot from-03-issues
click 600 "$ROW1_Y"                        # #5, assigned to the user (listed first)
wait_for 10 "the issue thread" has "SELECT id FROM threads WHERE worktree_branch = 'blongo/5-crash-on-empty-input'"
WT=$(sql "SELECT worktree_path FROM threads WHERE worktree_branch = 'blongo/5-crash-on-empty-input'")
[ "$(git -C "$WT" rev-parse HEAD)" = "$(git -C "$WORK/widgets" rev-parse main)" ] || fail "not from main"
has "SELECT id FROM turn_items WHERE kind LIKE '%user%'" && fail "the issue was sent without asking"
sleep 2
shot from-04-issue-thread

echo "3. a fork's pull request opens read-only"
click "$FROM_X" 60; sleep 2
click 340 "$TAB_Y"; sleep 0.8              # Pull requests
click 600 "$ROW1_Y"                        # #4 (asks the user to review: first)
wait_for 10 "the fork thread" has "SELECT id FROM threads WHERE worktree_branch = 'blongo/pr-4'"
wait_for 10 "the link" has "SELECT id FROM threads WHERE pr LIKE '%\"read_only\":true%'"
WT=$(sql "SELECT worktree_path FROM threads WHERE worktree_branch = 'blongo/pr-4'")
[ "$(git -C "$WT" rev-parse HEAD)" = "$FORK_SHA" ] || fail "not at the fork's head"
sleep 2
shot from-05-fork-thread

echo "4. a branch only on GitHub"
click "$FROM_X" 60; sleep 2
click 560 "$TAB_Y"; sleep 0.8              # Branches
shot from-06-branches
click 700 133; sleep 0.3                   # the name field
typ "feature/b"; xdotool key Return
wait_for 10 "the branch thread" has "SELECT id FROM threads WHERE worktree_branch = 'feature/b'"
WT=$(sql "SELECT worktree_path FROM threads WHERE worktree_branch = 'feature/b'")
[ "$(git -C "$WT" rev-parse HEAD)" = "$(git -C "$WORK/remote.git" rev-parse refs/heads/feature/b)" ] \
  || fail "not at GitHub's feature/b"
sleep 2
shot from-07-branch-thread

xdotool key ctrl+q
for _ in $(seq 1 50); do kill -0 "$PID" 2>/dev/null || break; sleep 0.2; done
PID=
cp "$WORK/app.log" "$OUT/app.log"
rm -rf "$WORK"
echo "done: $OUT"
