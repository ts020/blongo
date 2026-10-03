#!/usr/bin/env bash
# GUI end-to-end run of blongo against the fake Codex, driven with xdotool.
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
export BLONGO_CODEX_EXE="$ROOT/crates/blongo-harness/tests/fixtures/fake_codex.py"
export FAKE_CODEX_DELAY_MS=60
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
# PIDs of live fake-Codex processes started by this run's blongo (they
# inherit its BLONGO_DATA_DIR).
ours() {
  for p in /proc/[0-9]*; do
    grep -q fake_codex.py "$p/cmdline" 2>/dev/null &&
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
quit
PID=
cp "$WORK/app.log" "$OUT/app.log"
rm -rf "$WORK"
echo "done: $OUT"
