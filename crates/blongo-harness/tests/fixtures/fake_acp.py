#!/usr/bin/env python3
"""Fake Antigravity ACP server (`agy_acp_server`) over stdio.

ACP v1 shapes (initialize / authenticate / session/new / session/prompt /
session/update / session/request_permission / session/cancel) with the
Antigravity quirks the harness must handle:
- non-JSON noise on stdout,
- FAKE_ACP_SIGNED_OUT=1: `authenticate` prints the OAuth URL line on stdout
  and never answers (the real server waits for the browser),
- tool payloads spell the command `CommandLine` and the output
  `combinedOutput`, plus a duplicated `formattedOutput` and a huge field.

Prompt "loop" streams chunks until `session/cancel`; others run a
permission round trip.
"""
import json
import os
import queue
import sys
import threading

SESSION = "sess-fake-1"
inbox = queue.Queue()


def reader():
    for line in sys.stdin:
        line = line.strip()
        if line:
            inbox.put(json.loads(line))
    inbox.put(None)


def send(obj):
    obj.setdefault("jsonrpc", "2.0")
    print(json.dumps(obj), flush=True)


def update(u):
    send({"method": "session/update", "params": {"sessionId": SESSION, "update": u}})


def chunk(kind, text):
    update({"sessionUpdate": kind, "content": {"type": "text", "text": text}})


def next_msg():
    msg = inbox.get()
    if msg is None:
        sys.exit(0)
    return msg


def is_cancel(msg):
    return msg.get("method") == "session/cancel" and "id" not in msg


def tool_turn(prompt_req):
    chunk("agent_thought_chunk", "Thinking about ls")
    chunk("agent_message_chunk", "Listing files.")
    update({"sessionUpdate": "tool_call", "toolCallId": "tc-1", "title": "Run command",
            "kind": "execute", "status": "pending",
            "rawInput": {"CommandLine": "ls", "Cwd": "/w"}})
    send({"id": 0, "method": "session/request_permission", "params": {
        "sessionId": SESSION,
        "toolCall": {"toolCallId": "tc-1", "title": "Run command", "rawInput": {"CommandLine": "ls"}},
        "options": [
            {"optionId": "allow-once", "name": "Allow once", "kind": "allow_once"},
            {"optionId": "allow-always", "name": "Allow always", "kind": "allow_always",
             "_meta": {"agy.security.warning": {"message": "prompt injection risk"}}},
            {"optionId": "reject", "name": "Deny", "kind": "reject_once"}]}})
    while True:
        msg = next_msg()
        if is_cancel(msg):
            # The client must answer the open permission request itself.
            continue
        if msg.get("id") == 0 and "method" not in msg:
            outcome = msg["result"]["outcome"]
            break
    if outcome.get("outcome") == "cancelled":
        send({"id": prompt_req["id"], "result": {"stopReason": "cancelled"}})
        return
    allowed = outcome.get("optionId", "").startswith("allow")
    out = "Cargo.toml\nsrc\n"
    update({"sessionUpdate": "tool_call_update", "toolCallId": "tc-1",
            "status": "completed" if allowed else "failed",
            "rawOutput": {"combinedOutput": out if allowed else "denied",
                          "formattedOutput": out if allowed else "denied",
                          "exitCode": 0 if allowed else 1,
                          "blob": "x" * 200000}})
    chunk("agent_message_chunk", f" option={outcome.get('optionId')}")
    send({"id": prompt_req["id"], "result": {"stopReason": "end_turn"}})


def loop_turn(prompt_req):
    n = 0
    while True:
        try:
            msg = inbox.get(timeout=0.02)
        except queue.Empty:
            msg = {}
        if msg is None:
            sys.exit(0)
        if is_cancel(msg):
            assert msg["params"] == {"sessionId": SESSION}, msg
            send({"id": prompt_req["id"], "result": {"stopReason": "cancelled"}})
            return
        n += 1
        chunk("agent_message_chunk", f"tick {n} ")


def main():
    threading.Thread(target=reader, daemon=True).start()
    print("agy_acp_server starting (non-JSON noise)", flush=True)
    while True:
        msg = next_msg()
        method = msg.get("method")
        if method == "initialize":
            assert msg["params"]["protocolVersion"] == 1
            send({"id": msg["id"], "result": {
                "protocolVersion": 1,
                "agentCapabilities": {"loadSession": True},
                "authMethods": [{"id": "oauth-personal", "name": "Google"},
                                {"id": "gemini-api-key", "name": "API key"}],
                "agentInfo": {"name": "fake-agy", "version": "1.3.0"}}})
        elif method == "authenticate":
            assert msg["params"] == {"methodId": "oauth-personal"}, msg
            if os.environ.get("FAKE_ACP_SIGNED_OUT") == "1":
                print("Open the following link to authenticate the ACP server: "
                      "https://accounts.google.com/o/oauth2/v2/auth?client_id=fake&state=s", flush=True)
                continue  # waits for the browser forever
            send({"id": msg["id"], "result": {}})
        elif method == "session/new":
            send({"id": msg["id"], "result": {"sessionId": SESSION}})
        elif method == "session/prompt":
            text = msg["params"]["prompt"][0]["text"]
            if text == "loop":
                loop_turn(msg)
            else:
                tool_turn(msg)
        elif "id" in msg and method:
            send({"id": msg["id"], "error": {"code": -32601, "message": "method not found"}})


if __name__ == "__main__":
    main()
