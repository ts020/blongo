#!/usr/bin/env python3
"""Minimal TCP forwarder for the GUI e2e: `tcp_proxy.py LISTEN_PORT
TARGET_PORT` relays 127.0.0.1:LISTEN_PORT to 127.0.0.1:TARGET_PORT.
Killing it drops every relayed connection at once (a network cut)."""
import socket
import sys
import threading


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


def main():
    listen, target = int(sys.argv[1]), int(sys.argv[2])
    server = socket.socket()
    server.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    server.bind(("127.0.0.1", listen))
    server.listen(16)
    print(f"proxy 127.0.0.1:{listen} -> 127.0.0.1:{target}", flush=True)
    while True:
        client, _ = server.accept()
        try:
            upstream = socket.create_connection(("127.0.0.1", target))
        except OSError:
            client.close()
            continue
        for a, b in ((client, upstream), (upstream, client)):
            threading.Thread(target=pump, args=(a, b), daemon=True).start()


if __name__ == "__main__":
    main()
