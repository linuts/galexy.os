#!/usr/bin/env bash
# rustc wrapper for the stage 0 bootstrap.
#
# x.py's cargo probe runs
#   rustc --sysroot build/<host>/stage0-sysroot --target <galexy.json>
# before that sysroot has a codegen-backends directory. This nightly
# looks there first and does not use the host toolchain's Cranelift
# backend, so the probe dies with "failed to find a codegen-backends
# folder". Link the host backend into the requested sysroot, then exec
# the real rustc.
#
# The outer `cargo run` also leaks CARGO_MAKEFLAGS with jobserver fds
# this process cannot open. Drop those before the real compiler starts.
set -euo pipefail
unset CARGO_MAKEFLAGS MAKEFLAGS MFLAGS || true

REAL="${GALEXY_HOST_RUSTC:?}"
HOST="${GALEXY_HOST_TRIPLE:?}"
CG="${GALEXY_CODEGEN_BACKENDS:?}"

sysroot=""
prev=""
for arg in "$@"; do
    if [[ "$prev" == "--sysroot" ]]; then
        sysroot="$arg"
    elif [[ "$arg" == --sysroot=* ]]; then
        sysroot="${arg#--sysroot=}"
    fi
    prev="$arg"
done

if [[ -n "$sysroot" ]]; then
    libdir="lib"
    if [[ -d "$sysroot/lib64/rustlib" ]]; then
        libdir="lib64"
    fi
    dest="$sysroot/$libdir/rustlib/$HOST/codegen-backends"
    if [[ "$dest" != "$CG" ]] && ! compgen -G "${dest}/librustc_codegen_cranelift*" >/dev/null; then
        mkdir -p "$(dirname "$dest")"
        if [[ -d "$dest" && ! -L "$dest" ]]; then
            rm -rf "$dest"
        fi
        ln -sfn "$CG" "$dest"
    fi
fi

exec "$REAL" "$@"
