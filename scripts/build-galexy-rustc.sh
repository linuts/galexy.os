#!/usr/bin/env bash
# Cross-build a Cranelift rustc that runs on Galexy (Milestone 72).
#
# The rust checkout is not part of this repo. This script expects it at
# GALEXY_RUST_SRC (default: $HOME/galexy-rust), already checked out at the
# commit in rust-toolchain.toml (nightly-2026-10-08, 1d81eb4ad). It applies
# scripts/galexy-rustc.patch when the builtin target is missing, installs
# the PAL, writes bootstrap.toml, and builds stage 1 for the Galexy host.
# Stage 2 is not run: that would try to execute the Galexy ELF on Linux.
#
# Outputs:
#   $GALEXY_RUSTC_OUT/rustc
#   $GALEXY_RUSTC_OUT/sysroot/   (rlibs the host and the guest both use)
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
RUST_SRC="${GALEXY_RUST_SRC:-$HOME/galexy-rust}"
OUT="${GALEXY_RUSTC_OUT:-$ROOT/target/galexy-rustc}"
TOOLCHAIN="${RUSTUP_TOOLCHAIN:-nightly-2026-10-08}"
SYSROOT="$(rustc +"$TOOLCHAIN" --print sysroot)"
RUSTC="$SYSROOT/bin/rustc"
CARGO="$SYSROOT/bin/cargo"
LLVM_BIN="$SYSROOT/lib/rustlib/x86_64-unknown-linux-gnu/bin"
JSON_NAME="x86_64-unknown-galexy.json"

if [[ ! -x "$RUSTC" ]]; then
    echo "build-galexy-rustc: nightly rustc missing at $RUSTC" >&2
    exit 1
fi
if [[ ! -f "$RUST_SRC/x.py" ]]; then
    echo "build-galexy-rustc: rust checkout missing at $RUST_SRC" >&2
    exit 1
fi

if [[ ! -f "$RUST_SRC/compiler/rustc_target/src/spec/targets/x86_64_unknown_galexy.rs" ]]; then
    patch -d "$RUST_SRC" -p1 < "$ROOT/scripts/galexy-rustc.patch"
fi

cp "$ROOT/targets/$JSON_NAME" "$RUST_SRC/$JSON_NAME"
python3 "$ROOT/scripts/galexy-std-sysroot.py" --patch-library "$RUST_SRC/library"

# Cranelift's target-lexicon build script panics on x86_64-unknown-galexy.
# The rust tree patches crates.io to compiler/target-lexicon. Recreate that
# tree from the local cargo registry and apply the one-triple patch when
# the checkout does not already contain it.
LEXICON="$RUST_SRC/compiler/target-lexicon"
if ! grep -q 'x86_64-unknown-galexy' "$LEXICON/src/triple.rs" 2>/dev/null; then
    REGISTRY="$(find /usr/local/cargo/registry/src "$HOME/.cargo/registry/src" \
        -maxdepth 2 -type d -name 'target-lexicon-0.13.5' 2>/dev/null | head -1)"
    if [[ -z "${REGISTRY}" ]]; then
        echo "build-galexy-rustc: target-lexicon 0.13.5 not in the cargo registry" >&2
        exit 1
    fi
    rm -rf "$LEXICON"
    mkdir -p "$LEXICON"
    cp "$REGISTRY/Cargo.toml" "$REGISTRY/build.rs" "$REGISTRY/LICENSE" "$REGISTRY/README.md" "$LEXICON/"
    cp -a "$REGISTRY/src" "$LEXICON/src"
    python3 - "$LEXICON/src/triple.rs" <<'PY'
import pathlib, sys
path = pathlib.Path(sys.argv[1])
text = path.read_text()
old = '''        match s {\n            "armv6k-nintendo-3ds" => {'''
new = '''        match s {\n            // Galexy is not an LLVM OS. HOST becomes none+elf; codegen\n            // uses the llvm triple x86_64-unknown-none-elf.\n            "x86_64-unknown-galexy" => {\n                triple.architecture = Architecture::X86_64;\n                triple.vendor = Vendor::Unknown;\n                triple.operating_system = OperatingSystem::None_;\n                triple.environment = Environment::Unknown;\n                triple.binary_format = BinaryFormat::Elf;\n            }\n            "armv6k-nintendo-3ds" => {'''
if old not in text:
    raise SystemExit("target-lexicon triple.rs anchor missing")
path.write_text(text.replace(old, new, 1))
PY
fi

cat > "$RUST_SRC/bootstrap.toml" <<EOF
change-id = "ignore"

[build]
build = "x86_64-unknown-linux-gnu"
host = ["$JSON_NAME"]
target = ["$JSON_NAME"]
rustc = "$RUSTC"
cargo = "$CARGO"
jobs = 1
full-bootstrap = true
allocator = "system"
docs = false
extended = false

[rust]
codegen-backends = ["cranelift"]
llvm-tools = false
download-rustc = false
deny-warnings = false
debug = false
debug-assertions = false
optimize = true
channel = "dev"
# Static image, no dynamic loader. The default rpath flags are -Wl,
# which rust-lld rejects.
rpath = false

[llvm]
download-ci-llvm = false

# Keyed by the JSON path. Bootstrap's TargetSelection includes the
# spec file, so a bare triple would not match and these flags would drop.
[target.'x86_64-unknown-galexy.json']
cc = "clang"
cxx = "clang++"
ar = "$LLVM_BIN/llvm-ar"
linker = "$LLVM_BIN/rust-lld"
optimized-compiler-builtins = false
allocator = "system"
codegen-backends = ["cranelift"]
no-std = false
rpath = false
rustflags = [
    "-Z", "codegen-backend=cranelift",
    "-C", "link-arg=--image-base=0xc8000000000",
    "-C", "link-arg=--no-pie",
    "-C", "panic=abort",
    "--cfg", "getrandom_backend=\\"rdrand\\"",
]
EOF

export RUSTUP_TOOLCHAIN="$TOOLCHAIN"
export CC_x86_64_unknown_galexy=clang
export CXX_x86_64_unknown_galexy=clang++
export AR_x86_64_unknown_galexy="$LLVM_BIN/llvm-ar"
export CFLAGS_x86_64_unknown_galexy="--target=x86_64-unknown-none-elf"
export CARGO_BUILD_JOBS=1
# psm's build script, if a crate pulls it in. rustc itself does not.
export CC_x86_64_unknown_galexy

cd "$RUST_SRC"
./x.py build --stage 1 compiler/rustc \
    --host "$JSON_NAME" \
    --target "$JSON_NAME"

# Stage 1 for a cross host lands under build/<host>/stage1.
STAGE=""
for candidate in \
    "$RUST_SRC/build/x86_64-unknown-galexy/stage1" \
    "$RUST_SRC/build/host/stage1"
do
    if [[ -x "$candidate/bin/rustc" ]]; then
        STAGE="$candidate"
        break
    fi
done
if [[ -z "$STAGE" ]]; then
    echo "build-galexy-rustc: stage1 rustc not found under $RUST_SRC/build" >&2
    find "$RUST_SRC/build" -name rustc -type f | head
    exit 1
fi

mkdir -p "$OUT"
cp -a "$STAGE/bin/rustc" "$OUT/rustc"
"$LLVM_BIN/llvm-strip" --strip-unneeded "$OUT/rustc" || true
rm -rf "$OUT/sysroot"
mkdir -p "$OUT/sysroot/lib/rustlib/x86_64-unknown-galexy/lib"
# Stage 1's own lib dir is the compiler sysroot and has no target rlibs.
# The rlibs the guest compiles against were built by stage 0.
RLIB_SRC=""
for dir in \
    "$STAGE/lib/rustlib/x86_64-unknown-galexy/lib" \
    "$RUST_SRC/build/x86_64-unknown-linux-gnu/stage0-sysroot/lib/rustlib/x86_64-unknown-galexy/lib"
do
    if compgen -G "$dir/*.rlib" > /dev/null; then
        RLIB_SRC="$dir"
        break
    fi
done
if [[ -z "$RLIB_SRC" ]]; then
    echo "build-galexy-rustc: no galexy rlibs under $STAGE or stage0-sysroot" >&2
    exit 1
fi
# Bootstrap writes metadata beside the rlib (`-Zembed-metadata=no`).
# A no_std hello only loads core and compiler_builtins. core's
# metadata is about 66 MiB, which is why the RAM store is 80 MiB.
copied=0
for base in "$RLIB_SRC"/libcore-*.rlib "$RLIB_SRC"/libcompiler_builtins-*.rlib; do
    [[ -f "$base" ]] || continue
    cp -a "$base" "${base%.rlib}.rmeta" "$OUT/sysroot/lib/rustlib/x86_64-unknown-galexy/lib/"
    copied=$((copied + 1))
done
if [[ "$copied" -ne 2 ]]; then
    echo "build-galexy-rustc: expected core and compiler_builtins in $RLIB_SRC" >&2
    exit 1
fi
echo "build-galexy-rustc: $(wc -c < "$OUT/rustc") bytes -> $OUT/rustc"
echo "build-galexy-rustc: core + compiler_builtins from $RLIB_SRC"
