#!/usr/bin/env bash
# rustc wrapper for the stage 0 bootstrap.
#
# x.py's cargo probe runs
#   rustc --sysroot build/<host>/stage0-sysroot --target <galexy.json>
# before that sysroot has a codegen-backends directory. This nightly
# looks there first and does not use the host toolchain's Cranelift
# backend, so the probe dies with "failed to find a codegen-backends
# folder". Hardlink the host backend into the requested sysroot, then
# exec the real rustc.
#
# A directory symlink is not safe here. After the probe, bootstrap
# copies the host lib tree onto this sysroot. The destination path
# walks through that symlink, and remove_file deletes the host .so.
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

# Install each Cranelift backend file as its own hardlink. The
# destination has to be a real directory: bootstrap later copies the
# host tree onto it and unlinks the destination path.
install_backend() {
    local dest="$1"
    if [[ -L "$dest" ]]; then
        rm -f "$dest"
    fi
    if compgen -G "${dest}/librustc_codegen_cranelift*" >/dev/null; then
        return 0
    fi
    mkdir -p "$dest"
    local so base linked=0
    shopt -s nullglob
    for so in "$CG"/librustc_codegen_cranelift*; do
        base="$(basename "$so")"
        if ln -f "$so" "$dest/$base" 2>/dev/null || cp -a "$so" "$dest/$base"; then
            linked=1
        else
            echo "galexy-stage0-rustc: could not install $base into $dest" >&2
            exit 1
        fi
    done
    shopt -u nullglob
    if [[ "$linked" -ne 1 ]]; then
        echo "galexy-stage0-rustc: no cranelift backend in $CG" >&2
        exit 1
    fi
    echo "galexy-stage0-rustc: hardlinked cranelift backend into $dest" >&2
}

if [[ -n "$sysroot" ]]; then
    libdir="lib"
    if [[ -d "$sysroot/lib64/rustlib" ]]; then
        libdir="lib64"
    fi
    dest="$sysroot/$libdir/rustlib/$HOST/codegen-backends"
    if [[ "$dest" != "$CG" ]]; then
        install_backend "$dest"
    fi
fi

exec "$REAL" "$@"
