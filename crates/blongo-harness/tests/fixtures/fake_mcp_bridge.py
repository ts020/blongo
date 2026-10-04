#!/usr/bin/env python3
"""Stand-in for `blongo mcp-bridge SOCKET TOKEN_FILE` in core tests (the
core crate has no binary): sends the token line, then copies stdin to the
socket and the socket to stdout. The real bridge is tested through
`blongo-serve mcp-bridge` in the server tests."""
import os
import socket
import sys
import threading

sock_path, token_file = sys.argv[1], sys.argv[2]
token = open(token_file).read().strip()
s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
s.connect(sock_path)
s.sendall(f"blongo-mcp {token}\n".encode())


def upstream():
    while True:
        data = os.read(0, 65536)
        if not data:
            break
        s.sendall(data)
    s.shutdown(socket.SHUT_WR)


threading.Thread(target=upstream, daemon=True).start()
while True:
    data = s.recv(65536)
    if not data:
        break
    os.write(1, data)
