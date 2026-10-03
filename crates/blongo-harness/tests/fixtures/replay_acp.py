#!/usr/bin/env python3
"""Replays a recorded ACP agent session (t3code's ACP registry fixtures).

Frames in tests/fixtures/t3code/acp/*.ndjson are id-less:
  {"kind":"request"|"response"|"notification","method":...,"params"|"result"|"error":...}
This script plays the agent: `emit_inbound` frames are written in order
(responses get the id of the client's latest request with that method, agent
requests get fresh ids), and at every `expect_outbound` it waits for a client
frame of the same kind and method. Pairs are appended to ACP_REPLAY_LOG for
the test to compare; client frames the recording does not expect are logged
as unexpected (requests get a JSON-RPC error, `session/set_model` gets an
empty result).

Env:
  ACP_REPLAY_TRANSCRIPT  the .ndjson transcript (required)
  ACP_REPLAY_LOG         comparison log (optional)
  ACP_REPLAY_STATE       launch counter: the n-th launch plays segment n
"""
import json
import os
import sys

log_path = os.environ.get("ACP_REPLAY_LOG")


def log(entry):
    if log_path:
        with open(log_path, "a") as f:
            f.write(json.dumps(entry) + "\n")


def send(frame):
    frame = dict(frame, jsonrpc="2.0")
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
    state = os.environ.get("ACP_REPLAY_STATE")
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


def kind_of(frame):
    if "method" in frame:
        return "request" if "id" in frame else "notification"
    return "response"


def main():
    segs = segments(os.environ["ACP_REPLAY_TRANSCRIPT"])
    index = next_segment_index()
    seg = segs[index] if index < len(segs) else []
    log({"segment": index})
    request_ids = {}      # method -> id of the client's latest request
    agent_requests = {}   # our request id -> method
    agent_seq = 1000

    def matches(expected, actual):
        kind = expected["kind"]
        if kind_of(actual) != kind:
            return False
        if kind == "response":
            return agent_requests.get(actual.get("id")) == expected["method"]
        return actual.get("method") == expected["method"]

    def unexpected(frame):
        log({"unexpected": frame})
        if kind_of(frame) == "request":
            if frame["method"] == "session/set_model":
                send({"id": frame["id"], "result": {}})
            else:
                send({"id": frame["id"], "error": {
                    "code": -32601, "message": f"not in recording: {frame['method']}"}})

    last_expect = max((i for i, e in enumerate(seg) if e["type"] == "expect_outbound"),
                      default=-1)
    ended = False
    for pos, entry in enumerate(seg):
        if pos > last_expect and not ended:
            log({"end": index})
            ended = True
        frame = entry["frame"]
        if entry["type"] == "emit_inbound":
            body = {k: v for k, v in frame.items() if k in ("result", "error", "params")}
            if frame["kind"] == "response":
                send(dict(body, id=request_ids.get(frame["method"])))
            elif frame["kind"] == "request":
                agent_seq += 1
                agent_requests[agent_seq] = frame["method"]
                send(dict(body, id=agent_seq, method=frame["method"]))
            else:
                send(dict(body, method=frame["method"]))
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
                unexpected(got)
        if frame["kind"] == "request":
            request_ids[frame["method"]] = actual["id"]
        log({"label": entry["label"], "expected": frame, "actual": actual})
    if not ended:
        log({"end": index})
    while True:
        got = read_frame()
        if got is None:
            return
        unexpected(got)


if __name__ == "__main__":
    main()
