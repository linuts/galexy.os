#!/usr/bin/env python3
"""Reads one HMP command per line from stdin and sends them to the QEMU
monitor socket at $WORKDIR/mon.sock (default /tmp/opencode).

A bare `screendump` line is rewritten to use $DUMPPATH (default
$WORKDIR/shot.ppm), matching boot-test.sh's conventions."""

import os
import socket
import sys
import time

WORKDIR = os.environ.get('WORKDIR', '/tmp/opencode')
DUMPPATH = os.environ.get('DUMPPATH', os.path.join(WORKDIR, 'shot.ppm'))
SOCK = os.path.join(WORKDIR, 'mon.sock')

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
    if line == 'screendump':
        line = 'screendump ' + DUMPPATH
    s.send((line + '\n').encode())
    time.sleep(0.3)
    try:
        s.settimeout(0.5)
        s.recv(65536)
    except Exception:
        pass
s.close()
