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
# The allow-list in this repo wins over the copy the patch planted.
cp "$ROOT/scripts/python-allow.txt" "$SRC/crates/galexy-python/allow.txt"
python3 - "$SRC" "$ROOT/scripts/python-frozen.txt" "$ROOT/scripts/python-allow.txt" <<'PY'
import pathlib, sys
src, frozen_path, allow_path = map(pathlib.Path, sys.argv[1:])
def names(path):
    out = []
    for line in path.read_text().splitlines():
        line = line.split("#", 1)[0].strip()
        if line:
            out.append(line)
    return out
frozen = set(names(frozen_path))
allow = set(names(allow_path))
missing = sorted(frozen - allow)
if missing:
    raise SystemExit(f"frozen names missing from the allow-list: {missing}")
stems = []
for folder in ("crates/vm/Lib/core_modules", "crates/vm/Lib/python_builtins"):
    for path in (src / folder).glob("*.py"):
        stems.append(path.stem)
extra = sorted(set(stems) - frozen)
if extra:
    raise SystemExit(f"checkout freezes names that are not in python-frozen.txt: {extra}")
print("build-galexy-python: allow-list covers the frozen modules")
PY

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
