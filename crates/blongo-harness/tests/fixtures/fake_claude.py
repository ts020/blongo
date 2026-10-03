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
"""
import json
import queue
import sys
import threading
import time

SESSION = "fake-claude-session"
inbox = queue.Queue()


def reader():
    for line in sys.stdin:
        line = line.strip()
        if line:
            inbox.put(json.loads(line))
    inbox.put(None)


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
    result()


def loop_turn():
    n = 0
    while True:
        try:
            msg = inbox.get(timeout=0.02)
        except queue.Empty:
            msg = {}
        if msg is None:
            return
        if is_interrupt(msg):
            ack(msg)
            result("error_during_execution", True)
            return
        n += 1
        delta("text", f"tick {n} ")


def main():
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
            continue
        text = msg["message"]["content"]
        if text == "loop":
            loop_turn()
        else:
            tool_turn()


if __name__ == "__main__":
    main()
