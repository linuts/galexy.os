#!/usr/bin/env bash
# Cross-build a Cranelift rustc that runs on Galexy (Milestone 72).
#
# The rust checkout is not part of this repo. This script expects it at
# GALEXY_RUST_SRC (default: $HOME/galexy-rust), already checked out at the
# commit in rust-toolchain.toml (nightly-2026-10-08, 1d81eb4ad). It applies
# scripts/galexy-rustc.patch when the builtin target is missing, installs
# the PAL, writes bootstrap.toml, and builds stage 1 for the Galexy host.
# The host nightly must have rustc-codegen-cranelift. This date's rustc
# links LLVM in-process and creates lib/rustlib/.../codegen-backends only
# when that component is installed. Stage 0 compiles std with
# -Zcodegen-backend=cranelift, so a missing directory is fatal.
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
# `cargo run` inherits CARGO_MAKEFLAGS pointing at jobserver fds that are
# not open in this process. x.py's cargo then reports a misconfigured
# build and its rustc probe dies. Drop those before bootstrap starts.
# RUSTC is assigned above to the nightly binary. That assignment
# replaces a value inherited from cargo. Do not unset it afterwards.
unset CARGO_MAKEFLAGS MAKEFLAGS MFLAGS RUSTDOC RUSTC_WRAPPER \
    RUSTC_WORKSPACE_WRAPPER RUSTFLAGS RUSTDOCFLAGS || true
# An already-installed nightly does not grow this component just because
# rust-toolchain.toml lists it, until the next rustup sync. Install it here.
if ! rustup component add rustc-codegen-cranelift --toolchain "$TOOLCHAIN"; then
    echo "build-galexy-rustc: could not install rustc-codegen-cranelift on $TOOLCHAIN" >&2
    exit 1
fi
CG_DIR="$SYSROOT/lib/rustlib/x86_64-unknown-linux-gnu/codegen-backends"
if [[ ! -d "$CG_DIR" ]] || ! compgen -G "${CG_DIR}/librustc_codegen_cranelift*" >/dev/null; then
    echo "build-galexy-rustc: $TOOLCHAIN has no cranelift backend in $CG_DIR" >&2
    exit 1
fi
# A failed first run copies the host sysroot before the component exists.
# Stage 0 prefers that copy. Drop it when the backend .so is absent so the
# next x.py copies the toolchain that has it.
STAGE0_SYS="$RUST_SRC/build/x86_64-unknown-linux-gnu/stage0-sysroot"
STAGE0_CG="$STAGE0_SYS/lib/rustlib/x86_64-unknown-linux-gnu/codegen-backends"
if [[ -d "$STAGE0_SYS" ]] && ! compgen -G "${STAGE0_CG}/librustc_codegen_cranelift*" >/dev/null; then
    echo "build-galexy-rustc: removing stage0 sysroot that has no cranelift backend"
    rm -rf "$STAGE0_SYS"
fi
# nightly-2026-10-08. `cargo run` clones this when the checkout is absent.
RUST_REV="1d81eb4ad9cd207e3e638bd32b17ec4fce8412a6"
if [[ ! -f "$RUST_SRC/x.py" ]]; then
    echo "build-galexy-rustc: cloning $RUST_REV into $RUST_SRC"
    mkdir -p "$RUST_SRC"
    if [[ ! -d "$RUST_SRC/.git" ]]; then
        git -C "$RUST_SRC" init
    fi
    if ! git -C "$RUST_SRC" remote get-url origin >/dev/null 2>&1; then
        git -C "$RUST_SRC" remote add origin https://github.com/rust-lang/rust.git
    fi
    git -C "$RUST_SRC" fetch --depth 1 origin "$RUST_REV"
    git -C "$RUST_SRC" checkout --detach FETCH_HEAD
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
# Stage 0 stamps core metadata as 1.101.0-nightly (1d81eb4ad 2026-10-07).
# A dev channel omits the git hash and reports 1.101.0-dev, which the
# guest rejects (E0514). Nightly keeps the hash and matches that stamp.
channel = "nightly"
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
# A std hello loads this set (rmeta for typeck, rlib for the link).
# core's metadata is about 66 MiB; the whole set is about 90 MiB.
STD_CRATES=(
    adler2 alloc cfg_if compiler_builtins core dlmalloc hashbrown libc
    miniz_oxide panic_abort rustc_demangle rustc_std_workspace_alloc
    rustc_std_workspace_core std std_detect unwind
)
copied=0
dest="$OUT/sysroot/lib/rustlib/x86_64-unknown-galexy/lib"
for crate in "${STD_CRATES[@]}"; do
    found=0
    for base in "$RLIB_SRC"/lib${crate}-*.rlib; do
        [[ -f "$base" ]] || continue
        cp -a "$base" "$dest/"
        meta="${base%.rlib}.rmeta"
        if [[ ! -f "$meta" ]]; then
            echo "build-galexy-rustc: missing $meta" >&2
            exit 1
        fi
        cp -a "$meta" "$dest/"
        found=1
        copied=$((copied + 1))
    done
    if [[ "$found" -ne 1 ]]; then
        echo "build-galexy-rustc: expected one lib${crate} rlib in $RLIB_SRC" >&2
        exit 1
    fi
done
echo "build-galexy-rustc: $(wc -c < "$OUT/rustc") bytes -> $OUT/rustc"
echo "build-galexy-rustc: $copied std crates from $RLIB_SRC"
