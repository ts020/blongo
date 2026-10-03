#!/usr/bin/env bash
# GUI end-to-end run of blongo against the fake Codex, Claude Code and
# Antigravity agents, driven with xdotool.
#
# Usage: tools/e2e-gui.sh BINARY OUT_DIR
#
# Needs an X server in $DISPLAY (e.g. `Xvfb :93 -screen 0 1920x1080x24`),
# xdotool and ImageMagick's `import`. Screenshots land in OUT_DIR. Click
# coordinates assume the default 1280x800 window. Only processes started
# here are stopped (by PID).
set -euo pipefail

BIN=$(realpath "$1")
OUT=$(realpath -m "$2")
ROOT=$(cd "$(dirname "$0")/.." && pwd)
WORK=$(mktemp -d)
mkdir -p "$OUT" "$WORK/data" "$WORK/myproject"
export VK_ICD_FILENAMES=${VK_ICD_FILENAMES:-/usr/share/vulkan/icd.d/lvp_icd.json}
export BLONGO_DATA_DIR="$WORK/data"
FIX="$ROOT/crates/blongo-harness/tests/fixtures"
export BLONGO_CODEX_EXE="$FIX/fake_codex.py"
export BLONGO_CLAUDE_EXE="$FIX/fake_claude.py"
export BLONGO_ANTIGRAVITY_EXE="$FIX/fake_acp.py"
export FAKE_CODEX_DELAY_MS=60 FAKE_CLAUDE_DELAY_MS=40
export BLONGO_TERMINAL_SHELL=/bin/bash
# The import action reads this synthetic t3code database (never ~/.t3).
export BLONGO_T3_DB="$WORK/t3/statev2.sqlite"
mkdir -p "$WORK/t3" "$WORK/t3project"
python3 "$ROOT/tools/fixtures/make_t3_db.py" "$BLONGO_T3_DB" "$WORK/t3project"
T3_SUM=$(sha1sum "$BLONGO_T3_DB" | cut -d' ' -f1)
# Checkpoints and worktrees need a git repository.
git -C "$WORK/myproject" init -q
echo "# demo" >"$WORK/myproject/README.md"
git -C "$WORK/myproject" add README.md
git -C "$WORK/myproject" -c user.email=e2e@blongo -c user.name=e2e commit -qm init
PID=
WIN=

launch() {
  (cd "$WORK" && exec "$BIN" >>"$WORK/app.log" 2>&1) &
  PID=$!
  for _ in $(seq 1 100); do
    WIN=$(xdotool search --name '^Blongo$' 2>/dev/null | head -1 || true)
    [ -n "$WIN" ] && break
    sleep 0.1
  done
  sleep 1.5
}
shot() { timeout 10 import -window "$WIN" "$OUT/$1.png"; echo "  $1.png"; }
click() { xdotool mousemove --window "$WIN" "$1" "$2" click 1; }
typ() { xdotool type --delay 15 "$1"; }
quit() {
  xdotool key ctrl+q
  for _ in $(seq 1 50); do kill -0 "$PID" 2>/dev/null || return 0; sleep 0.2; done
  echo "blongo did not quit" >&2
  kill "$PID"
}
fail() { echo "FAIL: $*" >&2; exit 1; }
# PIDs of live fake agents started by this run's blongo (they inherit its
# BLONGO_DATA_DIR).
ours() {
  for p in /proc/[0-9]*; do
    grep -qE 'fake_(codex|claude|acp).py' "$p/cmdline" 2>/dev/null &&
      tr '\0' '\n' <"$p/environ" 2>/dev/null | grep -qx "BLONGO_DATA_DIR=$WORK/data" &&
      echo "${p#/proc/}"
  done
  return 0
}
trap '[ -n "$PID" ] && kill "$PID" 2>/dev/null || true' EXIT

echo "1. empty state, add project"
launch
shot 01-empty
typ "$WORK/myproject"; xdotool key Return; sleep 1.5
shot 02-project-and-thread

echo "2. multi-line prompt, streamed Markdown reply with reasoning + file change"
typ "markdown please"; xdotool key shift+Return; typ "second line"; sleep 0.3
shot 03-composer-multiline
xdotool key Return; sleep 1.2
shot 04-streaming
sleep 5
shot 05-streamed

echo "3. approval: approve"
click 217 60; sleep 1                     # "+ New" thread
typ "list files"; xdotool key Return; sleep 1.5
shot 06-approval-pending
click 433 611; sleep 1.5                  # Approve
shot 07-approved
click 700 461; sleep 0.5                  # expand the command output
shot 08-tool-output

echo "4. interrupt"
click 217 60; sleep 1
typ "loop"; xdotool key Return; sleep 2
shot 09-running
click 1140 757; sleep 1.2                 # Stop
shot 10-interrupted

echo "5. quit + relaunch restores from SQLite"
quit
launch
shot 11-restored
click 100 157; sleep 1                    # the "markdown please" thread
shot 12-restored-markdown

echo "6. crash mid-turn, relaunch recovers"
typ "loop"; xdotool key Return; sleep 1.5
kill -9 "$PID"; wait "$PID" 2>/dev/null || true
sleep 0.5
left=$(ours)
if [ -n "$left" ]; then
  echo "FAIL: agent processes outlived blongo: $left" >&2
  ps -o pid,ppid,cmd -p "$(echo $left | tr ' ' ,)" >&2 || true
  exit 1
fi
echo "  no orphaned agent after kill -9"
launch
shot 13-crash-recovered

echo "7. Claude Code thread: streamed Markdown with a highlighted code block"
xdotool key ctrl+n; sleep 1
xdotool key alt+2; sleep 0.5              # provider: Claude Code
typ "markdown"; xdotool key Return; sleep 4
shot 14-claude-markdown-highlight

echo "8. queue and steer while a run is active"
typ "slow"; xdotool key Return; sleep 1
typ "echo: queued one"; xdotool key Return; sleep 0.5
typ "please also check tests"; sleep 0.3
shot 15-queued
xdotool key ctrl+Return; sleep 0.8         # steer into the running turn
shot 16-steered
sleep 3
shot 17-queue-drained

echo "9. checkpoint rollback"
typ "write notes.txt hello"; xdotool key Return; sleep 2
[ -f "$WORK/myproject/notes.txt" ] || fail "the turn did not write notes.txt"
shot 18-before-rollback
xdotool key alt+z; sleep 2                 # undo the last turn
[ ! -e "$WORK/myproject/notes.txt" ] || fail "rollback left notes.txt behind"
echo "  notes.txt restored away by the rollback"
shot 19-rolled-back

echo "10. fork the thread"
xdotool key alt+f; sleep 1.5
typ "echo: hello from the fork"; xdotool key Return; sleep 2.5
shot 20-fork

echo "11. Antigravity thread: plan, highlighted code, model picker"
xdotool key ctrl+n; sleep 1
xdotool key alt+3; sleep 0.5
typ "plan"; xdotool key Return; sleep 3
shot 21-antigravity-plan
typ "markdown"; xdotool key Return; sleep 3
shot 22-antigravity-markdown
click 395 757; sleep 0.6                   # provider picker
shot 23-provider-picker
xdotool key Escape; click 395 757; sleep 0.3
xdotool key alt+m; sleep 0.8               # next model
shot 24-model-selected

echo "12. switch provider with context handoff"
xdotool key alt+1; sleep 0.5               # Codex
typ "echo: after the switch"; xdotool key Return; sleep 3
shot 25-provider-switch

echo "13. worktree thread"
click 150 60; sleep 2                      # "+ Worktree"
typ "echo: in a worktree"; xdotool key Return; sleep 3
ls -d "$WORK"/data/worktrees/* >/dev/null 2>&1 || fail "no worktree was created"
shot 26-worktree

echo "14. terminal"
xdotool key ctrl+grave; sleep 1.5
typ "echo hello-from-pty; printf '\\033[32mgreen\\033[0m\\n'; git branch --show-current"
xdotool key Return; sleep 1.5
shot 27-terminal
TERM_PIDS=$(for p in /proc/[0-9]*; do
  [ "$(awk '{print $4}' "$p/stat" 2>/dev/null)" = "$PID" ] &&
    grep -q '^/bin/bash' "$p/cmdline" 2>/dev/null && echo "${p#/proc/}"; done; true)
[ -n "$TERM_PIDS" ] || fail "no terminal shell found"
xdotool key ctrl+grave; sleep 1.5          # close: the shell is hung up
for t in $TERM_PIDS; do
  [ -e "/proc/$t" ] && [ "$(awk '{print $3}' "/proc/$t/stat")" != Z ] &&
    fail "terminal shell $t survived closing the panel"
done
echo "  terminal shell stopped on close"

echo "15. import from t3code"
click 80 782; sleep 2                      # "Import from t3code…"
shot 28-imported
[ "$(sha1sum "$BLONGO_T3_DB" | cut -d' ' -f1)" = "$T3_SUM" ] || fail "import modified its source"
quit
PID=
left=$(ours)
[ -z "$left" ] || fail "agent processes outlived blongo: $left"
cp "$WORK/app.log" "$OUT/app.log"
rm -rf "$WORK"
echo "done: $OUT"
