#!/usr/bin/env bash
# Merging and archiving, end to end against a fake GitHub on loopback: a
# pull request created from a worktree thread; Merge asks again while it
# is not ready, auto-merge, a squash merge at the head shown, then
# Archive thread deletes the branch on GitHub, the worktree and the local
# branch; the "after a merge" setting.
#
# Usage: tools/e2e-gui-pr-merge.sh BINARY OUT_DIR
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

DISP=:${E2E_DISPLAY:-93}
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

echo "4. Commit, push and create"
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
assert b["head"] == "blongo/echo-fix-the-parser" and b["base"] == "main" and b["draft"] is False, b
assert "Empty input crashed" in b["body"], b
print("  created", b["head"], "->", b["base"])'
sleep 3
shot create-03-created

echo "5. not ready: Merge asks again; Merge when ready turns auto-merge on"
TIP=$(git -C "$WORK/remote.git" rev-parse "refs/heads/$NEW")
control '{"op": "pull", "repo": "acme/widgets", "pull": {"number": 1, "head_sha": "'"$TIP"'",
  "merge_state": "BLOCKED", "review": "REVIEW_REQUIRED",
  "checks": [{"name": "test (ubuntu)", "workflow": "CI", "status": "IN_PROGRESS", "conclusion": null}]}}'
xdotool key ctrl+shift+p; sleep 0.5; typ "Check the pull request now"; sleep 0.4; xdotool key Return
wait_for 10 "the status" has "SELECT id FROM threads WHERE pr_status LIKE '%review_required%'"
sleep 2
shot merge-01-not-ready
click 502 246; sleep 0.4                   # Squash
click 571 246; sleep 0.8                   # Merge
shot merge-02-confirm
gh_log | grep -q '"method": "PUT"' && fail "merged without asking again"
click 925 246; sleep 0.5              # Cancel
click 678 246                              # Merge when ready
wait_for 10 "auto-merge" sh -c "curl -sf '$API/__log' | grep -q enablePullRequestAutoMerge"
gh_log | grep -q 'mergeMethod:SQUASH' || fail "auto-merge without the chosen method"
sleep 2
shot merge-03-auto

echo "6. ready: squash merge at the head shown"
control '{"op": "pull", "repo": "acme/widgets", "pull": {"number": 1, "merge_state": "CLEAN", "review": "APPROVED",
  "auto_merge": null,
  "checks": [{"name": "test (ubuntu)", "workflow": "CI", "status": "COMPLETED", "conclusion": "SUCCESS"}]}}'
xdotool key ctrl+shift+p; sleep 0.5; typ "Check the pull request now"; sleep 0.4; xdotool key Return
wait_for 10 "the ready status" has "SELECT id FROM threads WHERE pr_status LIKE '%approved%'"
sleep 2
shot merge-04-ready
click 571 200          # Merge
wait_for 10 "the merge" sh -c "curl -sf '$API/__log' | grep -q '\"method\": \"PUT\"'"
gh_log | python3 -c '
import json, sys
put = [r for r in json.load(sys.stdin) if r["method"] == "PUT"]
assert put[0]["path"] == "/repos/acme/widgets/pulls/1/merge", put
assert put[0]["body"] == {"merge_method": "squash", "sha": sys.argv[1]}, put
print("  merged by squash at", sys.argv[1][:10])' "$TIP"
wait_for 10 "the merged status" has "SELECT id FROM threads WHERE pr_status LIKE '%\"merged\"%'"
sleep 2
shot merge-05-merged

echo "7. archive, deleting the branch on GitHub"
click 600 200; sleep 0.4         # Also delete the branch on GitHub
click 822 200                  # Archive thread
wait_for 10 "the archive" has "SELECT id FROM threads WHERE archived = 1"
gh_log | grep -q '"method": "DELETE", "path": "/repos/acme/widgets/git/refs/heads/blongo/echo-fix-the-parser"' \
  || fail "the branch on GitHub was not deleted"
wait_for 10 "the worktree removed" sh -c "[ ! -d '$WT' ]"
wait_for 10 "the local branch deleted" sh -c "[ -z \"\$(git -C '$WORK/widgets' branch --list '$NEW')\" ]"
sleep 1.5
shot merge-06-archived

echo "8. the after-merge setting"
click 75 91; sleep 1                       # the project's other thread
click 142 19; sleep 1                      # Settings
click 300 198; sleep 0.8                   # GitHub
shot merge-07-settings
click 827 287                              # After a merge: Archive the thread
wait_for 10 "the archive setting" has "SELECT id FROM projects WHERE forge LIKE '%\"archive_on_merge\":true%'"
sleep 0.8
shot merge-08-archive-setting

xdotool key ctrl+q
for _ in $(seq 1 50); do kill -0 "$PID" 2>/dev/null || break; sleep 0.2; done
PID=
cp "$WORK/app.log" "$OUT/app.log"
rm -rf "$WORK"
echo "done: $OUT"
