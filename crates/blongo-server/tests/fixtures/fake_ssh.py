#!/usr/bin/env python3
"""A stand-in for `ssh` in tests: no network, no authentication.

`-L 127.0.0.1:L:HOST:R` (or `-L /path.sock:HOST:R`) forwards local port L to HOST:R on this machine
(like a real tunnel whose far end is local); otherwise the remote command
runs locally through `sh -c`. Options taking a value (-o, -p, -L, -i, -l)
and flags (-N, -T, -q) are accepted; `--` ends the options; the first
other word is the host.
FAKE_SSH_LOG appends the argument vector (one JSON array per line).
"""
import json
import os
import socket
import sys
import threading

args = sys.argv[1:]
if os.environ.get("FAKE_SSH_LOG"):
    with open(os.environ["FAKE_SSH_LOG"], "a") as f:
        f.write(json.dumps(args) + "\n")

forward = None
no_command = False
host = None
command = []
i = 0
while i < len(args):
    a = args[i]
    if host is None and a in ("-o", "-p", "-L", "-i", "-l"):
        if a == "-L":
            forward = args[i + 1]
        i += 2
        continue
    if host is None and a == "--":
        host = args[i + 1]
        i += 2
        continue
    if host is None and a in ("-N", "-T", "-q"):
        no_command = no_command or a == "-N"
        i += 1
        continue
    if host is None:
        host = a
    else:
        command.append(a)
    i += 1

if host is None:
    sys.exit("fake ssh: no host")


def pump(src, dst):
    try:
        while True:
            data = src.recv(65536)
            if not data:
                break
            dst.sendall(data)
    except OSError:
        pass
    finally:
        for s in (src, dst):
            try:
                s.shutdown(socket.SHUT_RDWR)
            except OSError:
                pass


if forward:
    parts = forward.split(":")
    if len(parts) == 3:  # /local/socket:host:port
        local_path, far_host, far = parts
        server = socket.socket(socket.AF_UNIX)
        old = os.umask(0o177)
        server.bind(local_path)
        os.umask(old)
    else:
        bind_host, local, far_host, far = parts
        server = socket.socket()
        server.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
        server.bind((bind_host, int(local)))
    server.listen(16)
    while True:
        client, _ = server.accept()
        try:
            upstream = socket.create_connection((far_host, int(far)))
        except OSError:
            client.close()
            continue
        threading.Thread(target=pump, args=(client, upstream), daemon=True).start()
        threading.Thread(target=pump, args=(upstream, client), daemon=True).start()

if no_command or not command:
    threading.Event().wait()
os.execvp("sh", ["sh", "-c", " ".join(command)])
