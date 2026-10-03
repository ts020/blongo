#!/usr/bin/env python3
"""Resource profile of the headless server (`blongo-serve`), in the same
phases and units as tools/profile.py.

Usage:
  tools/profile_serve.py SERVE_BINARY DRIVE_BINARY OUTPUT_DIR
  tools/profile_serve.py SERVE_BINARY DRIVE_BINARY OUTPUT_DIR \\
      --app APP_BINARY            # also profile the app with this server
                                  # as a second (remote) environment

The server starts on 127.0.0.1 with an empty data dir and the fake Codex
(crates/blongo-harness/tests/fixtures/fake_codex.py). The headless client
(crates/blongo-server/examples/drive.rs) pairs, creates a project + thread
and, after the idle phase, sends "replay": the fake Codex streams the
fixture's reasoning and text deltas every 40 ms through the server's core,
hub and WebSocket to the client. Phases: `idle` 10 s with the client
connected, `stream` until the run ends, `settled` 15 s. The fake agent is
reported separately under `harness`.

With --app, the server keeps running and tools/profile.py runs the app
(local workload as usual) with an environments.json pairing it to this
server, so the app's extra memory for a connected remote environment can
be compared with a plain run.

Only processes started here are stopped (by PID / own process group).
"""
import argparse
import json
import os
import re
import signal
import subprocess
import sys
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from profile import descendants, stat, summarize  # noqa: E402

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))


def wait_line(path, pattern, timeout, proc):
    deadline = time.time() + timeout
    while time.time() < deadline:
        if proc.poll() is not None:
            sys.exit(f"{proc.args[0]} exited early ({proc.returncode}); see {path}")
        with open(path) as f:
            m = re.search(pattern, f.read())
        if m:
            return m
        time.sleep(0.1)
    sys.exit(f"timed out waiting for {pattern!r} in {path}")


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("serve")
    ap.add_argument("drive")
    ap.add_argument("output")
    ap.add_argument("--fixture", default="tools/fixtures/resource-stream.jsonl")
    ap.add_argument("--idle", type=float, default=10.0)
    ap.add_argument("--settled", type=float, default=15.0)
    ap.add_argument("--timeout", type=float, default=240.0)
    ap.add_argument("--app", help="also profile this app binary with the server as a remote environment")
    args = ap.parse_args()

    out = os.path.abspath(args.output)
    os.makedirs(out)
    for d in ("data", "project", "remote-project"):
        os.makedirs(os.path.join(out, d))
    env = dict(
        os.environ,
        BLONGO_CODEX_EXE=os.path.join(ROOT, "crates/blongo-harness/tests/fixtures/fake_codex.py"),
        FAKE_CODEX_REPLAY=os.path.abspath(args.fixture),
        FAKE_CODEX_DELAY_MS=os.environ.get("BLONGO_REPLAY_DELAY_MS", "40"),
    )
    env.pop("BLONGO_DATA_DIR", None)
    serve_log = os.path.join(out, "serve.log")
    log = open(serve_log, "w")
    server = subprocess.Popen(
        [args.serve, "--port", "0", "--data-dir", os.path.join(out, "data"), "--pair"],
        env=env, stdout=log, stderr=log, start_new_session=True,
    )
    procs = [server]
    try:
        addr = wait_line(serve_log, r"listening on ws://(\S+)/ws", 30, server).group(1)
        code = wait_line(serve_log, r"pairing code[^:]*: (\S+)", 30, server).group(1)
        drive_log = os.path.join(out, "drive.log")
        dlog = open(drive_log, "w")
        drive = subprocess.Popen(
            [args.drive, f"ws://{addr}/ws", code, os.path.join(out, "project"), "replay"],
            stdin=subprocess.PIPE, stdout=dlog, stderr=dlog, start_new_session=True,
        )
        procs.append(drive)
        # Idle starts once the client is paired and subscribed; it sends
        # the prompt when told to (a line on its stdin).
        wait_line(drive_log, r"drive: ready", 30, drive)
        samples = []
        t0 = time.time()
        phase = "idle"
        done_at = None
        while True:
            now = time.time()
            if server.poll() is not None:
                sys.exit(f"blongo-serve exited early; see {serve_log}")
            if phase == "idle" and now - t0 >= args.idle:
                phase = "stream"
                drive.stdin.write(b"go\n")
                drive.stdin.flush()
            if phase == "stream":
                dlog.flush()
                with open(drive_log) as f:
                    if "drive: done" in f.read():
                        phase, done_at = "settled", now
                if now - t0 > args.timeout:
                    sys.exit("the stream did not finish in time")
            if phase == "settled" and now - done_at >= args.settled:
                break
            samples.append({
                "at": now,
                "phase": phase,
                "serve": stat(server.pid),
                "harness": [s for s in map(stat, descendants(server.pid)) if s],
            })
            time.sleep(0.5)
        summary = {"binary": os.path.abspath(args.serve), "fixture": args.fixture, "phases": {}}
        for p in ("idle", "stream", "settled"):
            rows = [s for s in samples if s["phase"] == p]
            summary["phases"][p] = {
                "durationSeconds": rows[-1]["at"] - rows[0]["at"],
                "serve": summarize(rows, "serve"),
                "harnessPeakRssMiB": max((sum(h["rssMiB"] for h in r["harness"]) for r in rows), default=0),
            }
        with open(drive_log) as f:
            summary["client"] = f.read().strip().splitlines()[-1]
        json.dump(samples, open(os.path.join(out, "samples.json"), "w"))
        json.dump(summary, open(os.path.join(out, "summary.json"), "w"), indent=2)
        print(json.dumps(summary, indent=2))

        if args.app:
            # Pair the app's config with the same server, then run the usual
            # app profile with it as a second environment.
            code = subprocess.run(
                [args.serve, "pair", "--data-dir", os.path.join(out, "data")],
                env=env, capture_output=True, text=True, check=True,
            ).stdout.strip()
            config = os.path.join(out, "app-config")
            subprocess.run(
                [args.app, "env", "add", "devbox", f"ws://{addr}/ws", code],
                env=dict(env, BLONGO_CONFIG_DIR=config), check=True,
            )
            subprocess.run(
                [sys.executable, os.path.join(ROOT, "tools/profile.py"), args.app,
                 os.path.join(out, "app-with-remote"), "--fixture", args.fixture,
                 "--environments", os.path.join(config, "environments.json")],
                env=os.environ, check=True,
            )
    finally:
        for p in reversed(procs):
            if p.poll() is None:
                os.killpg(p.pid, signal.SIGTERM)
                try:
                    p.wait(10)
                except subprocess.TimeoutExpired:
                    os.killpg(p.pid, signal.SIGKILL)


if __name__ == "__main__":
    main()
