#!/usr/bin/env python3
"""Fake Antigravity ACP server (`agy_acp_server`) over stdio.

ACP v1 shapes (initialize / authenticate / session/new / session/prompt /
session/update / session/request_permission / session/cancel) with the
Antigravity quirks the harness must handle:
- non-JSON noise on stdout,
- FAKE_ACP_SIGNED_OUT=1: `authenticate` prints the OAuth URL line on stdout
  and never answers (the real server waits for the browser); with
  FAKE_ACP_AUTH_STDERR=1 the line goes to stderr and the URL is over 500
  bytes, as the real 1.2.1 / 1.3.0 servers print it,
- tool payloads spell the command `CommandLine` and the output
  `combinedOutput`, plus a duplicated `formattedOutput` and a huge field.

Prompt "loop" streams chunks until `session/cancel`; "markdown" answers
with markdown and a fenced code block; "plan" sends plan updates; "model?"
names the selected model; a prompt containing "echo:" is answered with the
prompt; "slow" streams ticks and ends by itself; others run a permission round
trip.

Phase 2 additions:
- `session/new` offers two models; `session/set_model` switches;
- `session/load` (loadSession) replays a history chunk before answering,
  which the client must drop;
- FAKE_ACP_LOGIN_FILE with FAKE_ACP_SIGNED_OUT=1: `authenticate` prints the
  URL, then answers once that file exists (the browser sign-in finished).
"""
import json
import os
import queue
import sys
import threading
import time

SESSION = "sess-fake-1"
MODELS = {"currentModelId": "fake-model-a", "availableModels": [
    {"modelId": "fake-model-a", "name": "Fake Model A"},
    {"modelId": "fake-model-b", "name": "Fake Model B"}]}
current_model = "fake-model-a"
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


def loop_turn(prompt_req, limit=None, delay=0.02):
    n = 0
    while True:
        try:
            msg = inbox.get(timeout=delay)
        except queue.Empty:
            msg = {}
        if msg is None:
            sys.exit(0)
        if is_cancel(msg):
            assert msg["params"] == {"sessionId": SESSION}, msg
            send({"id": prompt_req["id"], "result": {"stopReason": "cancelled"}})
            return
        if limit is not None and n >= limit:
            send({"id": prompt_req["id"], "result": {"stopReason": "end_turn"}})
            return
        n += 1
        chunk("agent_message_chunk", f"tick {n} ")


MARKDOWN = """Here is **bold**, `inline`, and a list:

- one
- two

```rust
fn main() {
    let answer = 42; // comment
    println!("{answer}");
}
```
"""


def simple_turn(prompt_req, text):
    chunk("agent_message_chunk", text)
    send({"id": prompt_req["id"], "result": {"stopReason": "end_turn"}})


def plan_turn(prompt_req):
    entries = [{"content": "Read the code", "priority": "high", "status": "in_progress"},
               {"content": "Write the fix", "priority": "high", "status": "pending"}]
    update({"sessionUpdate": "plan", "entries": entries})
    for e in entries:
        e["status"] = "completed"
    update({"sessionUpdate": "plan", "entries": entries})
    simple_turn(prompt_req, "plan done")


def main():
    global SESSION, current_model
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
                # Real 1.2.1 / 1.3.0 builds print it on stderr, ~560 bytes.
                real = os.environ.get("FAKE_ACP_AUTH_STDERR") == "1"
                url = "https://accounts.google.com/o/oauth2/v2/auth?client_id=fake&state=s"
                if real:
                    url += "&scope=" + "x" * 500
                print("Open the following link to authenticate the ACP server: " + url,
                      file=sys.stderr if real else sys.stdout, flush=True)
                done = os.environ.get("FAKE_ACP_LOGIN_FILE")
                if not done:
                    continue  # waits for the browser forever
                while not os.path.exists(done):
                    time.sleep(0.02)
            send({"id": msg["id"], "result": {}})
        elif method == "session/new":
            send({"id": msg["id"], "result": {"sessionId": SESSION, "models": MODELS}})
        elif method == "session/load":
            SESSION = msg["params"]["sessionId"]
            # History replay: the client already has it.
            chunk("agent_message_chunk", "REPLAYED HISTORY")
            send({"id": msg["id"], "result": {"models": dict(MODELS, currentModelId=current_model)}})
        elif method == "session/set_model":
            current_model = msg["params"]["modelId"]
            send({"id": msg["id"], "result": {}})
        elif method == "session/prompt":
            text = msg["params"]["prompt"][0]["text"]
            if text == "loop":
                loop_turn(msg)
            elif text == "markdown":
                simple_turn(msg, MARKDOWN)
            elif text == "plan":
                plan_turn(msg)
            elif text == "model?":
                simple_turn(msg, f"model={current_model}")
            elif "echo:" in text:
                simple_turn(msg, text)
            elif text == "slow":
                loop_turn(msg, limit=60, delay=0.05)
            else:
                tool_turn(msg)
        elif "id" in msg and method:
            send({"id": msg["id"], "error": {"code": -32601, "message": "method not found"}})


if __name__ == "__main__":
    main()
