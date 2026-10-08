#!/usr/bin/env bash
# One-command review smoke: the host suites, the no-secrets audit, and a
# focused subset of the QEMU boot suite that touches every axis a
# reviewer cares about (BIOS + UEFI, auth, sealed disk persist + recover,
# hostile ELF, negative suite, galfs capacity, SMP, typing e2e).
#
# usage: scripts/review-smoke.sh            # ~10 min under TCG
#        scripts/review-smoke.sh --full     # the whole boot suite instead
# env:   OVMF_FD   UEFI firmware (default: /usr/share/ovmf/OVMF.fd when
#                  the runner's distro default is missing)
#
# The full matrix is `cargo test -p runner --test boot -- --test-threads=1`
# (see README "CI"); this script is the subset to run before asking for a
# review, not a replacement for it.
set -euo pipefail
cd "$(dirname "$0")/.."

if [ -z "${OVMF_FD:-}" ] && [ ! -e /usr/share/ovmf/x64/OVMF.4m.fd ] && [ -e /usr/share/ovmf/OVMF.fd ]; then
  export OVMF_FD=/usr/share/ovmf/OVMF.fd
fi

say() { printf '\n== %s ==\n' "$*"; }

say "toolchain"
rustc -V
command -v qemu-system-x86_64 >/dev/null || { echo "qemu-system-x86_64 not found" >&2; exit 1; }

say "host suites"
cargo test -q -p galexy-abi -p galexy-core -p galexy-crypto -p galexy-galf -p galfs-fsck -p gxc -p gxld

say "no secrets on serial"
cargo test -q -p runner --test audit_strings

if [ "${1:-}" = "--full" ]; then
  say "full boot suite"
  exec cargo test -p runner --test boot -- --test-threads=1
fi

say "focused boots"
exec cargo test -p runner --test boot -- --test-threads=1 --exact \
  test_kernel_runs_and_passes \
  main_kernel_boots_and_timer_ticks \
  uefi_image_boots_and_timer_ticks \
  smp_test_passes \
  badelf_test_passes \
  negative_test_passes \
  blocks_test_passes \
  galfs_disk_persists_virtio_blk \
  galfs_disk_recovers_from_corrupt_slot \
  shell_must_change_typing_e2e \
  shell_run_hello_typing_e2e
