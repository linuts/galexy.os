#!/usr/bin/env python3
"""Reads one HMP command per line from stdin and sends them to the QEMU
monitor socket at /tmp/opencode/mon.sock, waiting for (gdb/qemu) prompt."""

import socket, sys, time

SOCK = '/tmp/opencode/mon.sock'

s = socket.socket(socket.AF_UNIX)
s.connect(SOCK)
time.sleep(0.5)
try:
    s.recv(65536)  # banner
except Exception:
    pass
for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    s.send((line + '\n').encode())
    time.sleep(0.3)
    try:
        s.settimeout(0.5)
        s.recv(65536)
    except Exception:
        pass
s.close()
