#!/usr/bin/env python3
"""Replays a recorded Claude session (t3code replay fixture) as the `claude`
CLI's stream-json stdio protocol.

t3code recorded these transcripts through the Claude Agent SDK
(claude-agent-sdk 0.2.111 driving claude 2.1.111), so the frames are the
SDK's view: `query.open` (process launch with options), `prompt.offer`
(a user message), `permission.request` / `permission.response`,
`query.interrupt`, `session.fork`, and the CLI's own stdout messages
(`system`, `assistant`, `user`, `stream_event`, `result`, ...), which the SDK
passes through unchanged. This script plays the CLI: stdout messages are
written verbatim, SDK-level frames are translated to the CLI wire the SDK
itself speaks:

  query.open {resume, resumeSessionAt}  -> argv `--resume X --resume-session-at=U`
  session.fork {sessionId, upToMessageId} (+ the next query.open)
                                        -> argv `--resume X --fork-session
                                           --resume-session-at=U`
                                           (the SDK's forkSession copies the
                                           session file instead; Blongo uses
                                           the CLI flags)
  prompt.offer {message}                -> stdin `{"type":"user",...}` line
  permission.request                    -> stdout control_request can_use_tool
  permission.response {result}          -> stdin control_response
  query.interrupt                       -> stdin control_request interrupt

Every (recorded, actual) pair is appended to CLAUDE_REPLAY_LOG for the test
to compare, plus unexpected client frames. Blongo's `initialize` control
request gets a canned answer listing one model.

After a recorded `query.interrupt` the recording simply ends (t3code closed
the query); the replay then exits, which the harness must read as the end of
an interrupted turn.

Env:
  CLAUDE_REPLAY_TRANSCRIPT  the .ndjson transcript (required)
  CLAUDE_REPLAY_LOG         comparison log (optional)
  CLAUDE_REPLAY_STATE       launch counter: the n-th launch plays segment n
"""
import json
import os
import sys

log_path = os.environ.get("CLAUDE_REPLAY_LOG")
SDK_ONLY = {"session.forked"}
INIT_RESPONSE = {"commands": [], "models": [
    {"value": "recorded-model", "displayName": "Recorded Model", "description": ""},
]}


def log(entry):
    if log_path:
        with open(log_path, "a") as f:
            f.write(json.dumps(entry) + "\n")


def send(frame):
    sys.stdout.write(json.dumps(frame) + "\n")
    sys.stdout.flush()


def segments(path):
    """Process segments. A `session.fork` belongs to the launch after it."""
    segs, cur = [], []
    with open(path) as f:
        for line in f:
            line = line.strip()
            if not line:
                continue
            entry = json.loads(line)
            if entry["type"] == "runtime_exit":
                segs.append(cur)
                cur = []
            elif entry["type"] in ("expect_outbound", "emit_inbound"):
                cur.append(entry)
    if cur:
        segs.append(cur)
    return segs


def next_segment_index():
    state = os.environ.get("CLAUDE_REPLAY_STATE")
    if not state:
        return 0
    try:
        with open(state) as f:
            n = int(f.read().strip() or "0")
    except FileNotFoundError:
        n = 0
    with open(state, "w") as f:
        f.write(str(n + 1))
    return n


def read_frame():
    while True:
        line = sys.stdin.readline()
        if not line:
            return None
        line = line.strip()
        if line:
            try:
                return json.loads(line)
            except ValueError:
                log({"unparsable": line})


def matches(expected, actual):
    kind = expected["type"]
    if kind == "prompt.offer":
        return actual.get("type") == "user"
    if kind == "permission.response":
        return actual.get("type") == "control_response"
    if kind == "query.interrupt":
        return (actual.get("type") == "control_request"
                and actual.get("request", {}).get("subtype") == "interrupt")
    return False


def answer_unexpected(frame):
    if frame.get("type") != "control_request":
        return
    rid = frame.get("request_id")
    if frame.get("request", {}).get("subtype") == "initialize":
        send({"type": "control_response",
              "response": {"subtype": "success", "request_id": rid, "response": INIT_RESPONSE}})
    else:
        send({"type": "control_response",
              "response": {"subtype": "error", "request_id": rid, "error": "not in recording"}})


def main():
    segs = segments(os.environ["CLAUDE_REPLAY_TRANSCRIPT"])
    index = next_segment_index()
    seg = segs[index] if index < len(segs) else []
    argv = sys.argv[1:]
    log({"segment": index, "argv": argv})
    perm_seq = 0
    last_perm = None
    interrupted = False
    last_expect = max((i for i, e in enumerate(seg) if e["type"] == "expect_outbound"),
                      default=-1)
    ended = False

    for pos, entry in enumerate(seg):
        if pos > last_expect and not ended:
            # Logged ahead of the trailing frames so the client never sees
            # the turn end before this line.
            log({"end": index})
            ended = True
        frame = entry["frame"]
        kind = frame.get("type")
        if entry["type"] == "emit_inbound":
            if kind in SDK_ONLY:
                log({"skipped": entry["label"]})
                continue
            if kind == "permission.request":
                perm_seq += 1
                last_perm = f"replay-perm-{perm_seq}"
                opts = frame.get("options", {})
                request = {"subtype": "can_use_tool", "tool_name": frame["toolName"],
                           "input": frame["input"],
                           "permission_suggestions": opts.get("suggestions"),
                           "tool_use_id": opts.get("toolUseID")}
                if "blockedPath" in opts:
                    request["blocked_path"] = opts["blockedPath"]
                send({"type": "control_request", "request_id": last_perm, "request": request})
                continue
            send(frame)
            continue
        # expect_outbound
        if kind in ("query.open", "session.fork"):
            log({"label": entry["label"], "expected": frame, "actual": {"argv": argv}})
            continue
        actual = None
        while actual is None:
            got = read_frame()
            if got is None:
                log({"eof_waiting_for": entry["label"]})
                return
            if matches(frame, got):
                actual = got
            else:
                log({"unexpected": got})
                answer_unexpected(got)
        log({"label": entry["label"], "expected": frame, "actual": actual})
        if kind == "permission.response":
            got_id = actual.get("response", {}).get("request_id")
            if got_id != last_perm:
                log({"wrong_request_id": got_id, "wanted": last_perm})
        if kind == "query.interrupt":
            interrupted = True
            send({"type": "control_response",
                  "response": {"subtype": "success", "request_id": actual.get("request_id"),
                               "response": {}}})
    if not ended:
        log({"end": index})
    if interrupted:
        return
    while True:
        got = read_frame()
        if got is None:
            return
        log({"unexpected": got})
        answer_unexpected(got)


if __name__ == "__main__":
    main()
