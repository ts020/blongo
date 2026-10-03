#!/usr/bin/env bash
# GUI end-to-end run of a remote environment: the app pairs with a local
# `blongo-serve` (through a TCP forwarder standing in for the network) as
# its second environment, runs turns there (approval, interrupt, streamed
# Markdown), survives a network cut (resume without gaps) and a server
# restart (fresh snapshots), and opens a terminal on the server.
#
# Usage: tools/e2e-gui-remote.sh APP_BINARY SERVE_BINARY OUT_DIR
#
# Needs an X server in $DISPLAY, xdotool and ImageMagick's `import`. Click
# coordinates assume the default 1280x800 window. Only processes started
# here are stopped (by PID).
set -euo pipefail

BIN=$(realpath "$1")
SERVE=$(realpath "$2")
OUT=$(realpath -m "$3")
ROOT=$(cd "$(dirname "$0")/.." && pwd)
WORK=$(mktemp -d)
mkdir -p "$OUT" "$WORK/data" "$WORK/srv" "$WORK/local" "$WORK/remote"
export VK_ICD_FILENAMES=${VK_ICD_FILENAMES:-/usr/share/vulkan/icd.d/lvp_icd.json}
export BLONGO_DATA_DIR="$WORK/data"
export BLONGO_CONFIG_DIR="$WORK/config"
FIX="$ROOT/crates/blongo-harness/tests/fixtures"
export BLONGO_CODEX_EXE="$FIX/fake_codex.py"
export FAKE_CODEX_DELAY_MS=60
export BLONGO_TERMINAL_SHELL=/bin/bash
for d in local remote; do
  git -C "$WORK/$d" init -q
  echo "# $d" >"$WORK/$d/README.md"
  git -C "$WORK/$d" add README.md
  git -C "$WORK/$d" -c user.email=e2e@blongo -c user.name=e2e commit -qm init
done
free_port() { python3 -c 'import socket; s=socket.socket(); s.bind(("127.0.0.1",0)); print(s.getsockname()[1])'; }
SPORT=$(free_port)
PPORT=$(free_port)
PID= SRV= PROXY= WIN=

start_server() {
  # The server's own data dir; SHELL for its terminals.
  (BLONGO_DATA_DIR= SHELL=/bin/bash exec "$SERVE" --port "$SPORT" --data-dir "$WORK/srv" --no-socket "$@" \
    >>"$WORK/serve.log" 2>&1) &
  SRV=$!
  for _ in $(seq 1 100); do
    grep -q "listening on ws://127.0.0.1:$SPORT" "$WORK/serve.log" 2>/dev/null && return 0
    sleep 0.1
  done
  fail "blongo-serve did not start"
}
stop_server() {
  kill "$SRV"; wait "$SRV" 2>/dev/null || true; SRV=
}
start_proxy() {
  python3 "$ROOT/tools/fixtures/tcp_proxy.py" "$PPORT" "$SPORT" >>"$WORK/proxy.log" 2>&1 &
  PROXY=$!
  sleep 0.3
}
stop_proxy() { kill "$PROXY"; wait "$PROXY" 2>/dev/null || true; PROXY=; }
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
cleanup() {
  for p in $PID $SRV $PROXY; do kill "$p" 2>/dev/null || true; done
}
trap cleanup EXIT
# Remote thread text as the server stored it (sqlite3 CLI not needed).
server_items() {
  python3 - "$WORK/srv/blongo.sqlite" "$1" <<'PY'
import sqlite3, sys
db = sqlite3.connect(f"file:{sys.argv[1]}?mode=ro", uri=True)
for (text,) in db.execute("select body from turn_items where kind = ? order by rowid", (sys.argv[2],)):
    print(text)
PY
}

echo "1. server with a pairing code; app with a local project"
start_server --pair
CODE=$(sed -n 's/.*pairing code[^:]*: \(.*\)$/\1/p' "$WORK/serve.log" | tail -1)
[ -n "$CODE" ] || fail "no pairing code"
start_proxy
launch
typ "$WORK/local"; xdotool key Return; sleep 1.5
shot r01-local-only

echo "2. add the server as an environment (pairing)"
click 186 782; sleep 0.5                  # "+ Environment"
typ "devbox ws://127.0.0.1:$PPORT $CODE"; sleep 0.3
shot r02-add-environment
xdotool key Return; sleep 2.5
shot r03-environment-connected
grep -q '"devbox"' "$BLONGO_CONFIG_DIR/environments.json" || fail "the environment was not saved"
[ "$(stat -c %a "$BLONGO_CONFIG_DIR/environments.json")" = 600 ] || fail "credentials file is not 0600"
grep -q "$CODE" "$BLONGO_CONFIG_DIR/environments.json" && fail "the pairing code was stored"
grep -q "paired device" "$WORK/serve.log" || true

echo "3. a project and thread on the server"
click 212 182; sleep 0.5                 # the environment's "+ Project"
typ "$WORK/remote"; xdotool key Return; sleep 2
shot r04-remote-thread

echo "4. approval over the wire"
typ "list files"; xdotool key Return; sleep 2
shot r05-remote-approval
click 433 611; sleep 2                    # Approve
shot r06-remote-approved

echo "5. interrupt over the wire"
typ "loop"; xdotool key Return; sleep 2
shot r07-remote-running
click 1140 757; sleep 1.5                 # Stop
shot r08-remote-interrupted

echo "6. network cut mid-stream: reconnect and resume without gaps"
typ "markdown please"; xdotool key Return; sleep 1.5
stop_proxy; sleep 1
shot r09-link-cut
start_proxy; sleep 6                      # first retry after 3 s
shot r10-resumed
grep -q "resumed after seq" "$WORK/serve.log" || fail "the client did not resume after the cut"
grep "resumed after seq" "$WORK/serve.log" | sed 's/^/  /'

echo "7. server restart: fresh snapshots"
stop_server; sleep 1
shot r11-server-down
start_server; sleep 9
shot r12-server-back
grep -q "cannot resume" "$WORK/serve.log" || fail "the restarted server did not send snapshots"
typ "echo: after the restart"; xdotool key Return; sleep 2.5
shot r13-after-restart
server_items assistant_message | grep -q "after the restart" || fail "the turn after the restart did not reach the server"

echo "8. terminal on the server"
xdotool key ctrl+grave; sleep 2
typ "echo remote-pty-\$PPID; pwd"; xdotool key Return; sleep 1.5
shot r14-remote-terminal
RPTY=$(for p in /proc/[0-9]*; do
  [ "$(awk '{print $4}' "$p/stat" 2>/dev/null)" = "$SRV" ] &&
    grep -q '^/bin/bash' "$p/cmdline" 2>/dev/null && echo "${p#/proc/}"; done; true)
[ -n "$RPTY" ] || fail "no terminal shell under the server"
echo "  terminal shell $RPTY runs under blongo-serve ($SRV)"
xdotool key ctrl+grave; sleep 1.5          # close: the server hangs it up
for t in $RPTY; do
  [ -e "/proc/$t" ] && [ "$(awk '{print $3}' "/proc/$t/stat")" != Z ] &&
    fail "remote terminal shell $t survived closing the panel"
done
echo "  remote terminal stopped on close"

quit
PID=
cp "$WORK/app.log" "$OUT/app-remote.log"
cp "$WORK/serve.log" "$OUT/serve.log"
stop_proxy
stop_server
rm -rf "$WORK"
echo "done: $OUT"
