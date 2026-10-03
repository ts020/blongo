#!/usr/bin/env bash
# Phase 4 GUI end-to-end run: diff panel + inline comment, command palette,
# a keybinding override, settings, file search, branch switch, a scheduled
# run firing, the PR inbox against a fake forge, notifications and a
# blongo:// link. Fake agents and a fake forge on loopback only.
#
# Usage: tools/e2e-gui-phase4.sh BINARY OUT_DIR
#
# Starts its own Xvfb (lavapipe for Vulkan) and stops it, the app and the
# fake forge by PID. Needs xdotool, ImageMagick's `import`, python3, git.
# Click coordinates assume the default 1280x800 window.
set -euo pipefail

BIN=$(realpath "$1")
OUT=$(realpath -m "$2")
ROOT=$(cd "$(dirname "$0")/.." && pwd)
WORK=$(mktemp -d)
mkdir -p "$OUT" "$WORK/data" "$WORK/config" "$WORK/myproject/src" "$WORK/myproject/target"
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
cat >"$BLONGO_NOTIFY_CMD" <<EOF
#!/bin/sh
printf '%s|%s\n' "\$1" "\$2" >> "$WORK/notify.log"
EOF
chmod +x "$BLONGO_NOTIFY_CMD"

# A project with an ignored folder (the file search must skip it).
cd "$WORK/myproject"
git init -q
printf '# demo\n' >README.md
printf 'fn main() {}\n' >src/main.rs
printf 'pub fn lib() {}\n' >src/lib.rs
printf 'target/\n' >.gitignore
printf 'ignored\n' >target/main_ignored.rs
git add README.md src .gitignore
git -c user.email=e2e@blongo -c user.name=e2e commit -qm init
cd "$ROOT"

# The fake forge.
FORGE_LOG="$WORK/forge.log"
python3 "$ROOT/tools/fixtures/fake_forge.py" "$WORK/forge.port" "$FORGE_LOG" &
FORGE_PID=$!
for _ in $(seq 1 50); do [ -s "$WORK/forge.port" ] && break; sleep 0.1; done
FORGE_PORT=$(cat "$WORK/forge.port")

# User files: keybinding overrides, a forge token, notifications always.
cat >"$WORK/config/keybindings.json" <<'EOF'
[
  { "key": "alt-k", "command": "palette.commands" },
  { "key": "alt-d", "command": "-view.diff" },
  { "key": "alt-g", "command": "view.diff", "when": "threadOpen && !view.settings" }
]
EOF
cat >"$WORK/config/forge.json" <<EOF
{ "forges": [ { "kind": "github", "api": "http://127.0.0.1:$FORGE_PORT", "token": "e2e-token" } ] }
EOF
printf '{ "notifications": "always" }\n' >"$WORK/config/settings.json"
chmod 600 "$WORK/config/"*.json

# Our own X server.
DISP=:${E2E_DISPLAY:-95}
Xvfb "$DISP" -screen 0 1920x1080x24 >/dev/null 2>&1 &
XVFB_PID=$!
export DISPLAY=$DISP
sleep 1

PID=
WIN=
cleanup() {
  [ -n "$PID" ] && kill "$PID" 2>/dev/null || true
  kill "$FORGE_PID" 2>/dev/null || true
  kill "$XVFB_PID" 2>/dev/null || true
}
trap cleanup EXIT

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
fail() {
  echo "FAIL: $*" >&2
  cp "$WORK/app.log" "$OUT/app.log" 2>/dev/null || true
  exit 1
}
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
quit() {
  xdotool key ctrl+q
  for _ in $(seq 1 50); do kill -0 "$PID" 2>/dev/null || return 0; sleep 0.2; done
  echo "blongo did not quit" >&2
  kill "$PID"
}
ours() {
  for p in /proc/[0-9]*; do
    grep -qE 'fake_(codex|claude|acp).py' "$p/cmdline" 2>/dev/null &&
      tr '\0' '\n' <"$p/environ" 2>/dev/null | grep -qx "BLONGO_DATA_DIR=$WORK/data" &&
      echo "${p#/proc/}"
  done
  return 0
}

echo "1. a turn that edits a file; notification on completion"
launch
typ "$WORK/myproject"; xdotool key Return; sleep 1.5
typ "write README.md changed"; xdotool key Return
wait_for 10 "the run" grep -qs 'a run finished|Completed' "$WORK/notify.log"
echo "  notified: $(head -1 "$WORK/notify.log")"
shot p4-01-turn

echo "1b. token usage in the thread header"
typ "usage"; xdotool key Return
wait_for 10 "the usage" has "SELECT usage FROM runs WHERE usage IS NOT NULL"
sleep 0.5
shot p4-01b-usage

echo "2. keybinding override: alt-d removed, alt-g opens the diff"
xdotool key alt+d; sleep 0.5
ran view.diff && fail "alt-d still opens the diff"
xdotool key alt+g; sleep 1.5
ran view.diff || fail "alt-g did not open the diff"
shot p4-02-diff

echo "3. inline comment sent back to the agent"
click 388 55; sleep 1                       # per-turn diff: Turn 1
shot p4-03b-turn-diff
click 314 55; sleep 1                       # back to all changes
click 600 184; sleep 0.6                    # the added line
typ "Please keep the heading"; xdotool key Return; sleep 0.6
shot p4-03-comment
click 1212 89; sleep 2                      # Send to agent
[ -n "$(sql "SELECT id FROM turn_items WHERE body LIKE 'Review comments on your changes:%README.md:1%Please keep the heading%'")" ] ||
  fail "the review comment did not reach the thread"
wait_for 10 "the approval notification" grep -qs 'approval needed' "$WORK/notify.log"
shot p4-04-comment-sent
click 1140 757; sleep 1.5                   # Stop (the fake agent asks to run ls)

echo "4. command palette (alt-k from keybindings.json)"
xdotool key alt+k; sleep 0.6
ran palette.commands || fail "alt-k did not open the palette"
typ "sett"; sleep 0.5
shot p4-05-palette
xdotool key Return; sleep 1
ran view.settings || fail "the palette did not run Open settings"
shot p4-06-settings

echo "5. settings persist: light theme"
click 707 193; sleep 1                      # General → Light
grep -q '"theme": "light"' "$WORK/config/settings.json" || fail "theme not saved"
shot p4-07-light
click 654 193; sleep 0.8                    # Dark
grep -q '"theme": "dark"' "$WORK/config/settings.json" || fail "theme not saved"

echo "6. a scheduled run (every minute, into this thread) fires"
click 330 132; sleep 0.5                    # Scheduled runs
click 900 146; typ "* * * * *"
click 900 188; typ "echo: scheduled hello"
click 962 227; sleep 0.2                    # post into the open thread
click 1086 227; sleep 1                     # Create
[ -n "$(sql "SELECT id FROM schedules WHERE cron = '* * * * *'")" ] || fail "schedule not created"
shot p4-08-schedule
echo "  waiting for the next minute…"
wait_for 75 "the schedule to fire" has "SELECT last_run_at FROM schedules WHERE last_run_at IS NOT NULL"
wait_for 10 "the scheduled message" has "SELECT id FROM turn_items WHERE body = 'echo: scheduled hello'"
sleep 1.5
shot p4-09-schedule-fired

echo "7. file search (respects .gitignore) and the file browser"
click 305 19; sleep 0.8                     # ← Back
xdotool key ctrl+p; sleep 0.5
ran palette.files || fail "ctrl+p did not open the file finder"
typ "main"; sleep 1
shot p4-10-file-search
xdotool key Return; sleep 1.5
shot p4-11-files

echo "8. create and switch a branch"
click 312 58; sleep 0.8                     # branch menu
typ "feature-x"; xdotool key Return; sleep 2
[ "$(git -C "$WORK/myproject" branch --show-current)" = feature-x ] || fail "branch not switched"
shot p4-12-branch

echo "9. PR inbox against the fake forge"
click 89 19; sleep 2.5                      # Inbox
shot p4-13-inbox
grep -q '"auth": "Bearer e2e-token"' "$FORGE_LOG" || fail "the inbox did not use the token"
click 410 100; sleep 2                      # the pull request
shot p4-14-pr
click 800 174; sleep 0.6                    # "+ fast(s);"
typ "Is fast() safe for empty input?"; xdotool key Return; sleep 0.5
click 900 773; typ "Nice speedup"
shot p4-15-review-draft
click 1212 57; sleep 2                      # Submit review
python3 - "$FORGE_LOG" <<'PY' || fail "the review did not reach the forge"
import json, sys
posts = [json.loads(l) for l in open(sys.argv[1]) if '"POST"' in l]
assert posts, "no POST"
p = posts[-1]
assert p["path"] == "/repos/acme/widgets/pulls/7/reviews", p
assert p["auth"] == "Bearer e2e-token"
b = p["body"]
assert b["commit_id"] == "a" * 40 and b["event"] == "COMMENT", b
assert b["body"] == "Nice speedup", b
c = b["comments"][0]
assert (c["path"], c["line"], c["side"]) == ("src/parse.rs", 2, "RIGHT"), c
assert c["body"] == "Is fast() safe for empty input?", c
print("  review posted:", c["path"], c["line"])
PY
shot p4-16-review-sent

echo "10. a blongo:// link handed to the running instance"
click 305 19; sleep 0.5                     # back to the chat
start=$(date +%s%N)
timeout 10 "$BIN" "blongo://settings" >>"$WORK/app.log" 2>&1 || fail "the link process failed"
echo "  second process exited in $(( ($(date +%s%N) - start) / 1000000 )) ms"
sleep 1
shot p4-17-deeplink-settings

echo "11. quit"
quit
PID=
left=$(ours)
[ -z "$left" ] || fail "agent processes outlived blongo: $left"
[ ! -e "$WORK/data/app.sock" ] || fail "the link socket was left behind"
cp "$WORK/app.log" "$OUT/app.log"
cp "$WORK/notify.log" "$OUT/notify.log"
rm -rf "$WORK"
echo "done: $OUT"
