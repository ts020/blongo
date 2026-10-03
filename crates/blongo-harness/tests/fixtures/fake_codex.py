#!/usr/bin/env python3
"""Fake `codex app-server` (JSON-RPC 2.0 over stdio).

Message names and shapes follow `codex app-server generate-json-schema`
from codex-cli 0.160.0 and its real responses (notifications carry no
"jsonrpc" field and an "emittedAtMs", like the real server).

Prompt "loop" streams deltas until `turn/interrupt`; a prompt starting with
"markdown" streams a Markdown reply with a file change; "replay" streams the
zeron-format journal named by FAKE_CODEX_REPLAY (reasoningDelta / textDelta
lines) every FAKE_CODEX_DELAY_MS (default 40); any other prompt runs a
command-execution approval round trip. FAKE_CODEX_SIGNED_OUT=1 makes
`account/read` report no account.
"""
import json
import os
import queue
import sys
import threading
import time

THREAD = "thread-fake-1"
inbox = queue.Queue()
turn_seq = 0


def reader():
    for line in sys.stdin:
        line = line.strip()
        if line:
            inbox.put(json.loads(line))
    inbox.put(None)


def send(obj):
    print(json.dumps(obj), flush=True)


def notify(method, params):
    send({"method": method, "params": params, "emittedAtMs": int(time.time() * 1000)})


def respond(req, result):
    send({"id": req["id"], "result": result})


def turn_obj(turn_id, status, error=None):
    return {"id": turn_id, "items": [], "itemsView": "notLoaded", "status": status,
            "error": error, "startedAt": None, "completedAt": None, "durationMs": None}


def complete(turn_id, status):
    notify("turn/completed", {"threadId": THREAD, "turn": turn_obj(turn_id, status)})


def handle_setup(msg):
    method = msg.get("method")
    if method == "initialize":
        respond(msg, {"userAgent": "fake/0.160.0", "codexHome": "/tmp/fake",
                      "platformFamily": "unix", "platformOs": "linux"})
    elif method == "account/read":
        signed_out = os.environ.get("FAKE_CODEX_SIGNED_OUT") == "1"
        respond(msg, {"account": None if signed_out else {"type": "apiKey"},
                      "requiresOpenaiAuth": True})
    elif method == "thread/start":
        respond(msg, {"thread": {"id": THREAD, "status": {"type": "idle"}, "turns": []},
                      "model": "fake-model", "approvalPolicy": msg["params"].get("approvalPolicy")})
        notify("thread/started", {"thread": {"id": THREAD}})
    elif method == "thread/resume":
        if msg["params"].get("excludeTurns") is not True:
            # Real Codex would hydrate the whole history into one line.
            send({"id": msg["id"], "error": {"code": -32602,
                                             "message": "fake: resume without excludeTurns"}})
        elif msg["params"].get("threadId") == THREAD:
            respond(msg, {"thread": {"id": THREAD, "status": {"type": "idle"}, "turns": []},
                          "model": "fake-model"})
        else:
            send({"id": msg["id"], "error": {"code": -32600, "message": "no rollout found"}})
    elif method == "turn/interrupt":
        send({"id": msg["id"], "error": {"code": -32600, "message": "no active turn"}})
    elif "id" in msg and "method" in msg:
        send({"id": msg["id"], "error": {"code": -32601, "message": f"unknown {method}"}})


def wait_interrupt_or(pred, turn_id):
    """Pull messages until pred(msg) is true; handle turn/interrupt."""
    while True:
        msg = inbox.get()
        if msg is None:
            sys.exit(0)
        if msg.get("method") == "turn/interrupt":
            respond(msg, {})
            return "interrupted", msg
        if pred(msg):
            return "ok", msg
        if msg.get("method") == "turn/start":
            send({"id": msg["id"], "error": {"code": -32600, "message": "turn already running"}})


def tool_turn(turn_id):
    notify("item/reasoning/summaryTextDelta",
           {"threadId": THREAD, "turnId": turn_id, "itemId": "r1", "delta": "Planning", "summaryIndex": 0})
    notify("item/agentMessage/delta",
           {"threadId": THREAD, "turnId": turn_id, "itemId": "m1", "delta": "Running ls."})
    notify("item/completed", {"threadId": THREAD, "turnId": turn_id,
                              "item": {"type": "agentMessage", "id": "m1", "text": "Running ls."}})
    item = {"type": "commandExecution", "id": "c1", "command": "ls", "cwd": "/w",
            "status": "inProgress", "commandActions": [], "aggregatedOutput": None, "exitCode": None}
    notify("item/started", {"threadId": THREAD, "turnId": turn_id, "item": item})
    send({"id": 0, "method": "item/commandExecution/requestApproval", "params": {
        "threadId": THREAD, "turnId": turn_id, "itemId": "c1", "command": "ls", "cwd": "/w",
        "reason": "needs approval", "startedAtMs": 1}})
    state, msg = wait_interrupt_or(lambda m: m.get("id") == 0 and "method" not in m, turn_id)
    if state == "interrupted":
        complete(turn_id, "interrupted")
        return
    decision = msg.get("result", {}).get("decision")
    notify("serverRequest/resolved", {"threadId": THREAD, "requestId": 0})
    if decision == "cancel":
        # "The turn will also be immediately interrupted."
        notify("item/completed", {"threadId": THREAD, "turnId": turn_id,
                                  "item": dict(item, status="declined")})
        complete(turn_id, "interrupted")
        return
    accepted = decision in ("accept", "acceptForSession")
    done = dict(item, status="completed" if accepted else "declined",
                aggregatedOutput="Cargo.toml\nsrc\n" if accepted else "",
                exitCode=0 if accepted else None)
    notify("item/completed", {"threadId": THREAD, "turnId": turn_id, "item": done})
    notify("item/agentMessage/delta",
           {"threadId": THREAD, "turnId": turn_id, "itemId": "m2", "delta": f" decision={decision}"})
    complete(turn_id, "completed")


def loop_turn(turn_id):
    n = 0
    while True:
        try:
            msg = inbox.get(timeout=0.02)
        except queue.Empty:
            msg = {}
        if msg is None:
            sys.exit(0)
        if msg.get("method") == "turn/interrupt":
            assert msg["params"] == {"threadId": THREAD, "turnId": turn_id}, msg
            respond(msg, {})
            complete(turn_id, "interrupted")
            return
        n += 1
        notify("item/agentMessage/delta",
               {"threadId": THREAD, "turnId": turn_id, "itemId": "m1", "delta": f"tick {n} "})


MARKDOWN = """## Plan

I looked at the project layout first. The change is small:

1. Add a `greet` function.
2. Call it from `main`.

```rust
fn greet(name: &str) -> String {
    format!("Hello, {name}!")
}
```

Done. The file `src/main.rs` was updated and the build is green.
"""


def interruptible_sleep(seconds):
    """Sleep, but return True if a turn/interrupt arrived meanwhile."""
    try:
        msg = inbox.get(timeout=seconds)
    except queue.Empty:
        return False, None
    if msg is None:
        sys.exit(0)
    if msg.get("method") == "turn/interrupt":
        respond(msg, {})
        return True, msg
    return False, msg


def stream(turn_id, deltas, delay):
    """Stream (kind, text) deltas; True if interrupted."""
    for kind, text in deltas:
        if kind == "reasoning":
            notify("item/reasoning/summaryTextDelta", {"threadId": THREAD, "turnId": turn_id,
                                                       "itemId": "r1", "delta": text, "summaryIndex": 0})
        else:
            notify("item/agentMessage/delta",
                   {"threadId": THREAD, "turnId": turn_id, "itemId": "m1", "delta": text})
        interrupted, _ = interruptible_sleep(delay)
        if interrupted:
            complete(turn_id, "interrupted")
            return True
    return False


def markdown_turn(turn_id):
    delay = int(os.environ.get("FAKE_CODEX_DELAY_MS", "30")) / 1000
    words = MARKDOWN.split(" ")
    deltas = [("reasoning", "Reading the project layout, "), ("reasoning", "then editing main.rs.")]
    deltas += [("text", w + " ") for w in words[:-1]] + [("text", words[-1])]
    half = len(deltas) // 2
    if stream(turn_id, deltas[:half], delay):
        return
    item = {"type": "fileChange", "id": "f1", "status": "inProgress",
            "changes": [{"path": "src/main.rs", "kind": {"type": "update"}, "diff": "+fn greet"}]}
    notify("item/started", {"threadId": THREAD, "turnId": turn_id, "item": item})
    notify("item/completed", {"threadId": THREAD, "turnId": turn_id, "item": dict(item, status="completed")})
    if stream(turn_id, deltas[half:], delay):
        return
    complete(turn_id, "completed")


def replay_turn(turn_id):
    delay = int(os.environ.get("FAKE_CODEX_DELAY_MS", "40")) / 1000
    deltas = []
    with open(os.environ["FAKE_CODEX_REPLAY"]) as f:
        for line in f:
            if not line.strip():
                continue
            event = json.loads(line)["event"]
            if event["type"] == "reasoningDelta":
                deltas.append(("reasoning", event["text"]))
            elif event["type"] == "textDelta":
                deltas.append(("text", event["text"]))
    if not stream(turn_id, deltas, delay):
        complete(turn_id, "completed")


def main():
    global turn_seq
    threading.Thread(target=reader, daemon=True).start()
    while True:
        msg = inbox.get()
        if msg is None:
            return
        if msg.get("method") != "turn/start":
            handle_setup(msg)
            continue
        turn_seq += 1
        turn_id = f"turn-{turn_seq}"
        respond(msg, {"turn": turn_obj(turn_id, "inProgress")})
        notify("turn/started", {"threadId": THREAD, "turn": turn_obj(turn_id, "inProgress")})
        text = msg["params"]["input"][0]["text"]
        if text == "loop":
            loop_turn(turn_id)
        elif text.startswith("markdown"):
            markdown_turn(turn_id)
        elif text.startswith("replay"):
            replay_turn(turn_id)
        else:
            tool_turn(turn_id)


if __name__ == "__main__":
    main()
