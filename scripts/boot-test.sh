#!/bin/bash
# Boot galexy.os in headless QEMU, optionally send HMP commands to it, capture
# serial log + screen dump.
# usage: boot-test.sh <image> <duration> [qemu-extra-args...]
set -eu
IMG="$1"; DUR="$2"; shift 2
rm -f /tmp/opencode/mon.sock /tmp/opencode/shot.ppm /tmp/opencode/serial.log
qemu-system-x86_64 -drive format=raw,file="$IMG" -display none -no-reboot \
  -serial stdio -monitor unix:/tmp/opencode/mon.sock,server=on,wait=off "$@" \
  > /tmp/opencode/serial.log 2>&1 &
QPID=$!
trap "kill \$QPID 2>/dev/null || true" EXIT
# wait for the monitor socket
for i in $(seq 1 50); do [ -S /tmp/opencode/mon.sock ] && break; sleep 0.2; done
python3 "$(dirname "$0")/drive-mon.py"
sleep "$DUR"
exit 0
