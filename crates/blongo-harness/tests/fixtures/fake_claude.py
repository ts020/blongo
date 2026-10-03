#!/usr/bin/env python3
"""Fake `claude -p --input-format stream-json --output-format stream-json`.

Offline stand-in for the Claude Code CLI's stream-json + control protocol
(frame shapes follow zeron crates/harness/src/claude/wire.rs and CLI 2.1.x).

Per user prompt:
- "loop": stream text deltas until an `interrupt` control request, then
  `result` subtype `error_during_execution`.
- anything else: thinking + text deltas, an assistant `tool_use` (Bash),
  a `can_use_tool` control_request, then after the control_response a
  tool_result (ok on allow, is_error on deny) and `result` success.

Phase 2: `--resume X` continues session X (a new id with
`--fork-session`); every turn ends with an `assistant` frame whose uuid is
the turn id; the `initialize` control request lists two models; a
`"priority": "now"` user message during "loop"/"slow" ends the running part
with `error_during_execution` and answers "steered: <text>"; "slow" ends by
itself after a while; "plan" sends a TodoWrite; "write <file> <text>" writes
a file; a prompt containing "echo:" is answered with the prompt.
FAKE_CLAUDE_LOG gets the argv and every stdin frame.
"""
import json
import os
import queue
import sys
import threading
import time

SESSION = "fake-claude-session"
inbox = queue.Queue()
turn_seq = 0
LOG = os.environ.get("FAKE_CLAUDE_LOG")


def log(entry):
    if LOG:
        with open(LOG, "a") as f:
            f.write(json.dumps(entry) + "\n")


def reader():
    for line in sys.stdin:
        line = line.strip()
        if line:
            frame = json.loads(line)
            log({"frame": frame})
            inbox.put(frame)
    inbox.put(None)


def end_turn(text=""):
    """The turn's last top-level assistant message (its uuid is the turn id)."""
    global turn_seq
    turn_seq += 1
    emit({"type": "assistant", "parent_tool_use_id": None, "uuid": f"{SESSION}-t{turn_seq}-{os.getpid()}",
          "message": {"id": f"msg-{turn_seq}", "role": "assistant",
                      "content": [{"type": "text", "text": text}] if text else []}})


def emit(frame):
    print(json.dumps(frame), flush=True)


def delta(kind, text):
    body = {"type": "text_delta", "text": text} if kind == "text" else {
        "type": "thinking_delta", "thinking": text}
    emit({"type": "stream_event", "parent_tool_use_id": None,
          "event": {"type": "content_block_delta", "index": 0, "delta": body}})


def result(subtype="success", is_error=False):
    emit({"type": "result", "subtype": subtype, "is_error": is_error,
          "session_id": SESSION, "result": ""})


def is_interrupt(msg):
    return (msg.get("type") == "control_request"
            and msg.get("request", {}).get("subtype") == "interrupt")


def ack(msg):
    emit({"type": "control_response", "response": {
        "subtype": "success", "request_id": msg["request_id"], "response": {}}})


def tool_turn():
    delta("thinking", "Let me list files.")
    delta("text", "I'll run ls.")
    emit({"type": "assistant", "parent_tool_use_id": None, "message": {
        "role": "assistant", "content": [
            {"type": "text", "text": "I'll run ls."},
            {"type": "tool_use", "id": "toolu_1", "name": "Bash",
             "input": {"command": "ls", "description": "List files"}}]}})
    emit({"type": "control_request", "request_id": "perm-1", "request": {
        "subtype": "can_use_tool", "tool_name": "Bash",
        "input": {"command": "ls", "description": "List files"},
        "tool_use_id": "toolu_1",
        "permission_suggestions": [{"type": "addRules", "behavior": "allow",
                                    "destination": "session",
                                    "rules": [{"toolName": "Bash", "ruleContent": "ls"}]}]}})
    while True:
        msg = inbox.get()
        if msg is None:
            return
        if is_interrupt(msg):
            ack(msg)
            emit({"type": "control_cancel_request", "request_id": "perm-1"})
            result("error_during_execution", True)
            return
        if msg.get("type") == "control_response":
            response = msg["response"]
            assert response["request_id"] == "perm-1", response
            decision = response["response"]
            break
    allowed = decision.get("behavior") == "allow"
    content = "Cargo.toml\nsrc" if allowed else decision.get("message", "denied")
    emit({"type": "user", "parent_tool_use_id": None, "message": {
        "role": "user", "content": [{"type": "tool_result", "tool_use_id": "toolu_1",
                                     "content": content, "is_error": not allowed}]}})
    delta("text", " Done." if allowed else " Permission denied.")
    end_turn()
    result()


def loop_turn(limit=None, delay=0.02):
    n = 0
    while True:
        try:
            msg = inbox.get(timeout=delay)
        except queue.Empty:
            msg = {}
        if msg is None:
            return
        if is_interrupt(msg):
            ack(msg)
            result("error_during_execution", True)
            return
        if msg.get("type") == "user" and msg.get("priority") == "now":
            # The CLI cuts the running part short, then answers the steer.
            result("error_during_execution", True)
            text = msg["message"]["content"]
            delta("text", f"steered: {text}")
            end_turn()
            result()
            return
        if limit is not None and n >= limit:
            end_turn()
            result()
            return
        n += 1
        delta("text", f"tick {n} ")


def simple(text):
    delta("text", text)
    end_turn()
    result()


def main():
    global SESSION
    args = sys.argv[1:]
    log({"argv": args})
    if "--resume" in args:
        SESSION = args[args.index("--resume") + 1]
        if "--fork-session" in args:
            SESSION = f"{SESSION}-fork-{os.getpid()}"
    threading.Thread(target=reader, daemon=True).start()
    emit({"type": "system", "subtype": "init", "model": "fake", "tools": ["Bash"],
          "cwd": "", "session_id": SESSION})
    while True:
        msg = inbox.get()
        if msg is None:
            return
        if msg.get("type") != "user":
            if is_interrupt(msg):
                ack(msg)
            elif msg.get("request", {}).get("subtype") == "initialize":
                emit({"type": "control_response", "response": {
                    "subtype": "success", "request_id": msg["request_id"], "response": {
                        "commands": [], "models": [
                            {"value": "fake-default", "displayName": "Fake Default"},
                            {"value": "fake-large", "displayName": "Fake Large"}]}}})
            continue
        text = msg["message"]["content"]
        if "echo:" in text:
            simple(text)
        elif text == "loop":
            loop_turn()
        elif text == "slow":
            loop_turn(limit=60, delay=0.05)
        elif text == "plan":
            emit({"type": "assistant", "parent_tool_use_id": None, "message": {
                "role": "assistant", "content": [{"type": "tool_use", "id": "todo1",
                    "name": "TodoWrite", "input": {"todos": [
                        {"content": "Read the code", "status": "in_progress"},
                        {"content": "Write the fix", "status": "pending"}]}}]}})
            emit({"type": "user", "parent_tool_use_id": None, "message": {
                "role": "user", "content": [{"type": "tool_result", "tool_use_id": "todo1",
                                             "content": "ok"}]}})
            emit({"type": "assistant", "parent_tool_use_id": None, "message": {
                "role": "assistant", "content": [{"type": "tool_use", "id": "todo2",
                    "name": "TodoWrite", "input": {"todos": [
                        {"content": "Read the code", "status": "completed"},
                        {"content": "Write the fix", "status": "completed"}]}}]}})
            emit({"type": "user", "parent_tool_use_id": None, "message": {
                "role": "user", "content": [{"type": "tool_result", "tool_use_id": "todo2",
                                             "content": "ok"}]}})
            simple("planned")
        elif text.startswith("write "):
            _, name, body = text.split(" ", 2)
            with open(name, "w") as f:
                f.write(body)
            simple(f"wrote {name}")
        else:
            tool_turn()


if __name__ == "__main__":
    main()
