#!/usr/bin/env python3
"""Resource profile for the blongo binary, phase-compatible with zeron's
scripts/resource-profile.mjs so the two can be compared directly.

Usage: tools/profile.py BINARY OUTPUT_DIR [--fixture tools/fixtures/resource-stream.jsonl]

The workload goes through the real path: blongo starts with an empty data
dir, creates a project + thread, and after the idle phase sends the prompt
"replay" to the fake `codex app-server` (crates/blongo-harness/tests/
fixtures/fake_codex.py), which streams the fixture's reasoning and text
deltas every 40 ms. Deltas flow harness -> core (SQLite, coalesced) ->
timeline, exactly like a real Codex turn.

Phases (500 ms samples, like zeron): `idle` 10 s after the window is up,
`stream` until the run finishes, `settled` 15 s. Reports peak/end RSS, PSS
(BLONGO_PROFILE_PSS=0 to skip) and CPU% (100% = one core) for the blongo
process; agent children (the fake codex) are reported separately under
`harness`.

Linux only. DISPLAY must name a working X server (e.g. Xvfb with lavapipe).
Thresholds: BLONGO_MAX_RSS_MIB fails the run if any phase peak exceeds it.

The app gets its own config dir (OUTPUT_DIR/config), so saved remote
environments of the machine are never picked up. `--environments FILE`
copies an environments.json there (tools/profile_serve.py --app makes one
for a running `blongo serve`): the profile then includes a connected
remote environment.
"""
import argparse
import json
import os
import shutil
import signal
import subprocess
import sys
import time

HZ = os.sysconf("SC_CLK_TCK")


def stat(pid):
    try:
        fields = open(f"/proc/{pid}/stat").read().rsplit(") ", 1)[1].split()
        status = open(f"/proc/{pid}/status").read()
        rss = int(next(l.split()[1] for l in status.splitlines() if l.startswith("VmRSS:")))
        threads = int(next(l.split()[1] for l in status.splitlines() if l.startswith("Threads:")))
        out = {
            "pid": pid,
            "cpuSeconds": (int(fields[11]) + int(fields[12])) / HZ,
            "rssMiB": rss / 1024,
            "threads": threads,
        }
        if os.environ.get("BLONGO_PROFILE_PSS", "1") == "1":
            for line in open(f"/proc/{pid}/smaps_rollup"):
                if line.startswith("Pss:"):
                    out["pssMiB"] = int(line.split()[1]) / 1024
        return out
    except (OSError, StopIteration, IndexError):
        return None


def descendants(pid):
    out = []
    try:
        for task in os.listdir(f"/proc/{pid}/task"):
            try:
                kids = open(f"/proc/{pid}/task/{task}/children").read().split()
            except OSError:
                continue
            for kid in map(int, kids):
                out += [kid, *descendants(kid)]
    except OSError:
        pass
    return out


def summarize(rows, key):
    valid = [r for r in rows if r[key]]
    if len(valid) < 2:
        return None
    first, last = valid[0], valid[-1]
    span = last["at"] - first["at"]
    out = {
        "peakRssMiB": max(r[key]["rssMiB"] for r in valid),
        "endRssMiB": last[key]["rssMiB"],
        "cpuPercent": 100 * (last[key]["cpuSeconds"] - first[key]["cpuSeconds"]) / span,
        "threads": last[key]["threads"],
    }
    if "pssMiB" in first[key]:
        out["peakPssMiB"] = max(r[key]["pssMiB"] for r in valid)
    return out


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("binary")
    ap.add_argument("output")
    ap.add_argument("--fixture", default="tools/fixtures/resource-stream.jsonl")
    ap.add_argument("--idle", type=float, default=10.0)
    ap.add_argument("--settled", type=float, default=15.0)
    ap.add_argument("--timeout", type=float, default=240.0)
    ap.add_argument("--environments", help="environments.json to give the app")
    ap.add_argument("--prompt", default="replay",
                    help="what to send (\"\" for nothing; \"bigdiff\" writes 200 x 500 lines)")
    ap.add_argument("--view", choices=["diff", "files"],
                    help="open this view after the run (or after the idle phase without a prompt)")
    ap.add_argument("--project", help="project folder (default: a new git repo in OUTPUT)")
    args = ap.parse_args()
    # The stream phase ends at this line of the app's log.
    marker = "view opened" if args.view and not args.prompt else "replay done"
    if args.view == "files" and not args.prompt:
        marker = "search done"

    os.makedirs(args.output)
    binary = os.path.join(args.output, "blongo-profiled")
    shutil.copy(args.binary, binary)
    project = args.project or os.path.join(args.output, "project")
    if not args.project:
        os.makedirs(project)
    if not args.project and args.view == "diff":
        # A repository, so runs get checkpoints and the diff panel has data
        # (the default profile keeps a plain folder, like earlier phases).
        subprocess.run(["git", "init", "-q", project], check=True)
        with open(os.path.join(project, "README.md"), "w") as f:
            f.write("# profile\n")
        git = ["git", "-C", project, "-c", "user.email=p@blongo", "-c", "user.name=p"]
        subprocess.run(git + ["add", "README.md"], check=True)
        subprocess.run(git + ["commit", "-qm", "init"], check=True)
    root = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
    startup = 2.0
    config = os.path.join(os.path.abspath(args.output), "config")
    os.makedirs(config)
    if args.environments:
        shutil.copy(args.environments, os.path.join(config, "environments.json"))
        os.chmod(os.path.join(config, "environments.json"), 0o600)
    env = dict(
        os.environ,
        BLONGO_DATA_DIR=os.path.join(os.path.abspath(args.output), "data"),
        BLONGO_CONFIG_DIR=config,
        BLONGO_CODEX_EXE=os.path.join(root, "crates/blongo-harness/tests/fixtures/fake_codex.py"),
        BLONGO_PROFILE_PROMPT=args.prompt,
        BLONGO_PROFILE_VIEW=args.view or "",
        BLONGO_PROFILE_PROJECT=os.path.abspath(project),
        BLONGO_PROFILE_START_MS=str(int((startup + args.idle) * 1000)),
        FAKE_CODEX_REPLAY=os.path.abspath(args.fixture),
        FAKE_CODEX_DELAY_MS=os.environ.get("BLONGO_REPLAY_DELAY_MS", "40"),
    )
    # Only lavapipe, as in every recorded baseline: without this the Vulkan
    # loader maps every installed ICD (about +12 MiB PSS on Ubuntu 24.04).
    lvp = "/usr/share/vulkan/icd.d/lvp_icd.json"
    if "VK_ICD_FILENAMES" not in env and os.path.exists(lvp):
        env["VK_ICD_FILENAMES"] = lvp
    log = open(os.path.join(args.output, "blongo.log"), "w+")
    child = subprocess.Popen([binary], env=env, stdout=log, stderr=log, start_new_session=True)
    samples = []
    t0 = time.time()
    phase = "startup"
    done_at = None
    try:
        while True:
            now = time.time()
            if child.poll() is not None:
                sys.exit(f"blongo exited early ({child.returncode}); see {log.name}")
            if phase == "startup" and now - t0 >= startup:
                phase = "idle"
            if phase == "idle" and now - t0 >= startup + args.idle:
                phase = "stream"
            if phase == "stream":
                log.flush()
                with open(log.name) as f:
                    if marker in f.read():
                        phase, done_at = "settled", now
                if now - t0 > args.timeout:
                    sys.exit("replay did not finish in time")
            if phase == "settled" and now - done_at >= args.settled:
                break
            samples.append({
                "at": now,
                "phase": phase,
                "ui": stat(child.pid),
                "harness": [s for s in map(stat, descendants(child.pid)) if s],
            })
            time.sleep(0.5)
    finally:
        os.killpg(child.pid, signal.SIGTERM)

    summary = {
        "binary": os.path.abspath(args.binary),
        "fixture": args.fixture,
        "prompt": args.prompt,
        "view": args.view,
        "project": os.path.abspath(project),
        "phases": {},
    }
    for p in ("idle", "stream", "settled"):
        rows = [s for s in samples if s["phase"] == p]
        summary["phases"][p] = {
            "durationSeconds": rows[-1]["at"] - rows[0]["at"],
            "blongo": summarize(rows, "ui"),
            "harnessPeakRssMiB": max((sum(h["rssMiB"] for h in r["harness"]) for r in rows), default=0),
        }
    json.dump(samples, open(os.path.join(args.output, "samples.json"), "w"))
    json.dump(summary, open(os.path.join(args.output, "summary.json"), "w"), indent=2)
    print(json.dumps(summary, indent=2))

    budget = os.environ.get("BLONGO_MAX_RSS_MIB")
    if budget:
        peak = max(p["blongo"]["peakRssMiB"] for p in summary["phases"].values())
        if peak > float(budget):
            sys.exit(f"peak RSS {peak:.1f} MiB exceeds budget {budget} MiB")


if __name__ == "__main__":
    main()
