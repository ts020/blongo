#!/usr/bin/env python3
"""Replays a recorded `codex app-server` session (t3code replay fixture).

The transcripts in tests/fixtures/t3code/*.ndjson were recorded by t3code
against the real codex-cli 0.156.1. Each line is one of

  {"type":"expect_outbound","label":...,"frame":{...}}  client -> codex
  {"type":"emit_inbound","label":...,"frame":{...}}     codex -> client
  {"type":"runtime_exit",...}                           end of one process

This script plays the codex side: inbound frames are written in order, and
at every `expect_outbound` it waits for the client to send a frame with the
same method (or, for a response to a server request, the same id) before
going on. Ids of the client's requests are mapped onto the recorded ones so
recorded responses reach the right request.

Blongo is not t3code, so the comparison is done by the test, not here:
every matched pair (recorded frame, actual frame) is appended to
CODEX_REPLAY_LOG as JSON, together with client frames the recording does
not expect ("unexpected") and recorded t3code-only requests Blongo does not
send ("skipped"). Unexpected requests get a canned answer (`account/read`)
or a JSON-RPC error so the client never hangs.

Env:
  CODEX_REPLAY_TRANSCRIPT  path of the .ndjson transcript (required)
  CODEX_REPLAY_LOG         where to append the comparison log (optional)
  CODEX_REPLAY_STATE       counter file: the n-th launch plays the n-th
                           process segment (for restart/resume fixtures)
"""
import json
import os
import sys

# Requests t3code sends that Blongo deliberately does not.
T3CODE_ONLY = {"thread/backgroundTerminals/terminate"}

log_path = os.environ.get("CODEX_REPLAY_LOG")


def log(entry):
    if log_path:
        with open(log_path, "a") as f:
            f.write(json.dumps(entry) + "\n")


def send(frame):
    sys.stdout.write(json.dumps(frame) + "\n")
    sys.stdout.flush()


def segments(path):
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
    state = os.environ.get("CODEX_REPLAY_STATE")
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
            return json.loads(line)


def is_request(frame):
    return "method" in frame and "id" in frame


def matches(expected, actual):
    if "method" in expected:
        return actual.get("method") == expected["method"]
    # A response to one of the recorded server requests.
    return "method" not in actual and actual.get("id") == expected.get("id")


def answer_unexpected(frame):
    if not is_request(frame):
        return
    if frame["method"] == "account/read":
        send({"id": frame["id"], "result": {"account": {"type": "chatgpt"},
                                            "requiresOpenaiAuth": True}})
    else:
        send({"id": frame["id"], "error": {"code": -32601,
                                           "message": f"not in recording: {frame['method']}"}})


def main():
    segs = segments(os.environ["CODEX_REPLAY_TRANSCRIPT"])
    index = next_segment_index()
    seg = segs[index] if index < len(segs) else []
    log({"segment": index})
    id_map = {}
    dropped = set()
    stash = []

    def later_expects(pos, frame):
        return any(e["type"] == "expect_outbound" and matches(e["frame"], frame)
                   for e in seg[pos:])

    last_expect = max((i for i, e in enumerate(seg) if e["type"] == "expect_outbound"),
                      default=-1)
    ended = False
    for pos, entry in enumerate(seg):
        if pos > last_expect and not ended:
            # Every expectation is met; log before the trailing frames so the
            # client never sees the turn end ahead of this line.
            log({"end": index})
            ended = True
        frame = entry["frame"]
        if entry["type"] == "emit_inbound":
            if "method" not in frame and "id" in frame:
                if frame["id"] in dropped:
                    continue
                if frame["id"] in id_map:
                    frame = dict(frame, id=id_map[frame["id"]])
            send(frame)
            continue
        if frame.get("method") in T3CODE_ONLY:
            log({"skipped": entry["label"]})
            if "id" in frame:
                dropped.add(frame["id"])
            continue
        actual = next((f for f in stash if matches(frame, f)), None)
        if actual is not None:
            stash.remove(actual)
        while actual is None:
            got = read_frame()
            if got is None:
                log({"eof_waiting_for": entry["label"]})
                return
            if matches(frame, got):
                actual = got
            elif later_expects(pos + 1, got):
                stash.append(got)
            else:
                log({"unexpected": got})
                answer_unexpected(got)
        if "id" in frame and "method" in frame:
            id_map[frame["id"]] = actual.get("id")
        log({"label": entry["label"], "expected": frame, "actual": actual})
    if not ended:
        log({"end": index})
    # The real server stays up until stdin closes.
    while True:
        got = read_frame()
        if got is None:
            return
        log({"unexpected": got})
        answer_unexpected(got)


if __name__ == "__main__":
    main()
