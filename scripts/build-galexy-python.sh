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
# `toolchain list` still prints a dangling `rustup toolchain link`.
# `cargo +galexy` then says the toolchain is not installed. `which`
# follows the link and fails when the directory is gone.
if ! rustup which --toolchain galexy rustc >/dev/null 2>&1; then
    echo "build-galexy-python: rustup toolchain 'galexy' is missing or its directory is gone (cargo xtask sysroot)" >&2
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
# A checkout patched before this `lane.read` uses `PyStr::as_str`,
# which this pin does not have. A short hunk also drops the last lines
# of `main.rs`. Reset and apply the current patch.
main="$SRC/crates/galexy-python/src/main.rs"
if ! grep -q 'name.to_str()' "$main" 2>/dev/null \
    || ! grep -q 'fn install_socket' "$main" 2>/dev/null \
    || ! grep -q 'fn socket_bind' "$main" 2>/dev/null \
    || ! grep -q 'fn socket_recv' "$main" 2>/dev/null \
    || ! grep -q 'value == 2' "$main" 2>/dev/null \
    || ! tail -n 1 "$main" | grep -q '^}'; then
    echo "build-galexy-python: re-applying scripts/galexy-rustpython.patch" >&2
    # checkout keeps edits to tracked files. The previous patch is those edits.
    git -C "$SRC" reset --hard "$REV"
    git -C "$SRC" clean -fd
    patch -d "$SRC" -p1 < "$ROOT/scripts/galexy-rustpython.patch"
fi
# The REPL calls `stdin.read_line`. That line is cooked only after
# `cargo xtask sysroot` has rebuilt libstd. A checkout that still has
# its own byte editor (`fn py_key`) will echo twice against that sysroot.
if [[ -f "$SRC/crates/galexy-python/src/main.rs" ]] && grep -q 'fn py_key' "$SRC/crates/galexy-python/src/main.rs"; then
    echo "build-galexy-python: checkout still has the old REPL editor; re-apply scripts/galexy-rustpython.patch before building against a cooked sysroot" >&2
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
# This nightly's cargo default is `-Zembed-metadata=no`: the rlib is a
# metadata stub and the full metadata is a sibling `.rmeta`. A
# dependent that starts before that file is ready reports
# `can't find crate` (`parking_lot`, `ascii`, and the rest of this
# graph). Embed metadata so the rlib is loadable on its own and
# dependents wait for it. The flag changes the unit fingerprint, so
# a target directory left by that failure rebuilds. Incremental stays
# off so a session from the failure is not resumed. The guest rustc
# bootstrap still splits metadata; this is only the host build of python.
export CARGO_INCREMENTAL=0
unset CARGO_ENCODED_RUSTFLAGS
# This nightly's rustc worker stack is 17 MiB (`DEFAULT_STACK_SIZE` in
# rustc_session). Compiling `icu_normalizer` while `icu_casemap` and
# `rustpython-unicode` are also in codegen faults that stack. rustc's
# own SIGSEGV handler prints
# `…/bin/../lib/librustc_driver-*.so(+offset)` and truncates the trace
# at 256 frames. That is this host compiler, not the OOM killer
# (SIGKILL, no handler, and the other jobs would not still be
# running) and not a cranelift rejection of the crate: the same
# compiler and target compile `icu_normalizer` 2.2.0 when it is alone.
# The handler's advice is to double the stack. A smaller inherited
# value would shrink the 17 MiB default, so raise it. One cargo job,
# and no inherited jobserver, so those three rustc processes are not
# in codegen together. Finished crates stay. No `cargo clean`.
case ${RUST_MIN_STACK:-} in
    ''|*[!0-9]*) export RUST_MIN_STACK=35651584 ;;
    *)
        if [[ "$RUST_MIN_STACK" -lt 35651584 ]]; then
            export RUST_MIN_STACK=35651584
        fi
        ;;
esac
export CARGO_BUILD_JOBS=1
unset CARGO_MAKEFLAGS MAKEFLAGS MFLAGS

cargo +galexy build --release \
    -Zembed-metadata=yes \
    --target x86_64-unknown-galexy \
    --manifest-path "$SRC/Cargo.toml" \
    -p galexy-python --bin python

mkdir -p "$OUT"
cp "$SRC/target/x86_64-unknown-galexy/release/python" "$OUT/python"
echo "build-galexy-python: $OUT/python"
# `cargo run` sets this. That cargo already holds the workspace lock,
# so a nested `cargo test` here would wait forever.
if [[ "${GALEXY_SKIP_PYTHON_CHECK:-}" != "1" ]]; then
(
    cd "$ROOT"
    # The Galexy link args above are for the target ELF. A host
    # `cargo test` must not inherit `--image-base`.
    unset RUSTFLAGS CARGO_ENCODED_RUSTFLAGS CARGO_PROFILE_RELEASE_LTO CARGO_INCREMENTAL \
        RUST_MIN_STACK CARGO_BUILD_JOBS
    cargo test -p xtask python_elf::pinned_python_elf_is_loadable -- --exact
)
fi
