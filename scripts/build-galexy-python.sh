#!/usr/bin/env bash
# Build the pinned RustPython interpreter for Galexy (Milestone 91).
#
# The checkout is not part of this repo. This script expects it at
# GALEXY_RUSTPYTHON_SRC (default: $HOME/galexy-rustpython), at the
# commit in scripts/rustpython.rev. It applies
# scripts/galexy-rustpython.patch when the Galexy entry crate is
# missing, then links one static ELF with the galexy sysroot.
#
# The host build does not pass --sysroot of this repository.
# The target JSON is not modified.
#
# Output: $GALEXY_PYTHON_OUT/python
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
REV="$(tr -d '[:space:]' < "$ROOT/scripts/rustpython.rev")"
SRC="${GALEXY_RUSTPYTHON_SRC:-$HOME/galexy-rustpython}"
OUT="${GALEXY_PYTHON_OUT:-$ROOT/target/galexy-python}"
TOOLCHAIN="${RUSTUP_TOOLCHAIN:-nightly-2026-10-08}"
SYSROOT="$(rustc +"$TOOLCHAIN" --print sysroot)"
LLVM_BIN="$SYSROOT/lib/rustlib/x86_64-unknown-linux-gnu/bin"

if [[ ! -x "$SYSROOT/bin/rustc" ]]; then
    echo "build-galexy-python: nightly rustc missing at $SYSROOT/bin/rustc" >&2
    exit 1
fi
if ! rustup toolchain list | grep -q '^galexy'; then
    echo "build-galexy-python: rustup toolchain 'galexy' is missing (cargo xtask sysroot)" >&2
    exit 1
fi

if [[ ! -d "$SRC/.git" ]]; then
    git clone https://github.com/RustPython/RustPython.git "$SRC"
fi
if ! git -C "$SRC" cat-file -e "$REV^{commit}" 2>/dev/null; then
    git -C "$SRC" fetch --depth 1 origin "$REV"
fi
git -C "$SRC" checkout --detach "$REV"

if [[ ! -f "$SRC/crates/galexy-python/Cargo.toml" ]]; then
    patch -d "$SRC" -p1 < "$ROOT/scripts/galexy-rustpython.patch"
fi

export PATH="$LLVM_BIN:$PATH"
# Cranelift, same as the sysroot. LLVM 23 cannot legalize an f16
# operation in `half` for this soft-float target. The sysroot rlibs
# also have no LLVM bitcode, so LTO stays off.
export RUSTFLAGS="-Z codegen-backend=cranelift --cfg getrandom_backend=\"rdrand\" -C link-arg=--image-base=0xc8000000000 -C link-arg=--no-pie"
export CARGO_PROFILE_RELEASE_LTO=off
unset CARGO_ENCODED_RUSTFLAGS

cargo +galexy build --release \
    --target x86_64-unknown-galexy \
    --manifest-path "$SRC/Cargo.toml" \
    -p galexy-python --bin python

mkdir -p "$OUT"
cp "$SRC/target/x86_64-unknown-galexy/release/python" "$OUT/python"
echo "build-galexy-python: $OUT/python"
(
    cd "$ROOT"
    cargo test -p xtask python_elf::pinned_python_elf_is_loadable -- --exact
)
