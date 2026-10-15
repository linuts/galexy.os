#!/usr/bin/env bash
# Cross-build a Cranelift rustc that runs on Galexy (Milestone 72).
#
# The rust checkout is not part of this repo. This script expects it at
# GALEXY_RUST_SRC (default: $HOME/galexy-rust), already checked out at the
# commit in rust-toolchain.toml (nightly-2026-10-08, 1d81eb4ad). It applies
# scripts/galexy-rustc.patch when the builtin target is missing, updates
# Cranelift's reported features when an older checkout still lists SSE,
# installs the PAL, writes bootstrap.toml, and builds stage 1 for the
# Galexy host.
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
# rustc loads codegen backends from beside `rustc --print target-libdir`.
# On a 64-bit host, a `lib64/rustlib` directory wins over `lib/rustlib`,
# even when it is empty. rustup always installs the cranelift component
# under `lib/`, so that empty directory hides it and stage 0 dies with
# "failed to find a codegen-backends folder".
TARGET_LIBDIR="$(rustc +"$TOOLCHAIN" --print target-libdir)"
HOST_TRIPLE="$(basename "$(dirname "$TARGET_LIBDIR")")"
CG_DIR="$(dirname "$TARGET_LIBDIR")/codegen-backends"
COMPONENT_CG="$SYSROOT/lib/rustlib/${HOST_TRIPLE}/codegen-backends"
# `component add` reports "up to date" when the files were deleted out
# from under an installed component. `component remove` then aborts,
# and rolls back, unless every path in the component manifest exists
# ("directory does not exist" — rustup's name for a missing file).
# Recreate those paths empty so remove can finish, then add fetches
# the real backend again.
cranelift_so=()
shopt -s nullglob
cranelift_so=("$COMPONENT_CG"/librustc_codegen_cranelift*)
shopt -u nullglob
if [[ ${#cranelift_so[@]} -eq 0 ]] || [[ ! -s "${cranelift_so[0]}" ]]; then
    echo "build-galexy-rustc: cranelift backend missing from ${COMPONENT_CG}; reinstalling the component"
    manifest="$SYSROOT/lib/rustlib/manifest-rustc-codegen-cranelift-preview-${HOST_TRIPLE}"
    if [[ ! -f "$manifest" ]]; then
        echo "build-galexy-rustc: no component manifest at $manifest" >&2
        exit 1
    fi
    while IFS= read -r line || [[ -n "$line" ]]; do
        case "$line" in
            file:*)
                rel="${line#file:}"
                dest="$SYSROOT/$rel"
                if [[ ! -e "$dest" ]]; then
                    mkdir -p "$(dirname "$dest")"
                    : > "$dest"
                fi
                ;;
            dir:*)
                rel="${line#dir:}"
                mkdir -p "$SYSROOT/$rel"
                ;;
        esac
    done < "$manifest"
    if ! rustup component remove rustc-codegen-cranelift --toolchain "$TOOLCHAIN"; then
        echo "build-galexy-rustc: could not remove rustc-codegen-cranelift on $TOOLCHAIN" >&2
        exit 1
    fi
    if ! rustup component add rustc-codegen-cranelift --toolchain "$TOOLCHAIN"; then
        echo "build-galexy-rustc: could not reinstall rustc-codegen-cranelift on $TOOLCHAIN" >&2
        exit 1
    fi
    shopt -s nullglob
    cranelift_so=("$COMPONENT_CG"/librustc_codegen_cranelift*)
    shopt -u nullglob
    if [[ ${#cranelift_so[@]} -eq 0 ]] || [[ ! -s "${cranelift_so[0]}" ]]; then
        echo "build-galexy-rustc: reinstall left no cranelift backend in $COMPONENT_CG" >&2
        exit 1
    fi
fi
PREFERRED_LIBDIR="$(dirname "$(dirname "$(dirname "$TARGET_LIBDIR")")")"
REAL_LIBDIR="$SYSROOT/lib/rustlib/${HOST_TRIPLE}/lib"
# `lib64/rustlib` with no rlibs is not a real sysroot. rustc still
# prefers it, so the component under `lib/` is invisible.
SHADOW="$(dirname "$(dirname "$TARGET_LIBDIR")")"
if [[ "$PREFERRED_LIBDIR" != "$SYSROOT/lib" ]] \
    && compgen -G "${REAL_LIBDIR}/lib*.rlib" >/dev/null \
    && [[ -z "$(find "$SHADOW" -name '*.rlib' -print -quit 2>/dev/null || true)" ]]
then
    echo "build-galexy-rustc: ${SHADOW} hides ${REAL_LIBDIR}; removing the shadow"
    rm -rf "$SHADOW"
    rmdir "$PREFERRED_LIBDIR" 2>/dev/null || true
    TARGET_LIBDIR="$(rustc +"$TOOLCHAIN" --print target-libdir)"
    CG_DIR="$(dirname "$TARGET_LIBDIR")/codegen-backends"
fi
# Same rule as the stage0 wrapper: a directory symlink is followed by
# the later copy, which then deletes the host backend.
if [[ -L "$CG_DIR" ]]; then
    rm -f "$CG_DIR"
fi
if [[ "$CG_DIR" != "$COMPONENT_CG" ]] \
    && compgen -G "${COMPONENT_CG}/librustc_codegen_cranelift*" >/dev/null \
    && ! compgen -G "${CG_DIR}/librustc_codegen_cranelift*" >/dev/null
then
    echo "build-galexy-rustc: hardlinking cranelift backend into ${CG_DIR}"
    mkdir -p "$CG_DIR"
    for so in "$COMPONENT_CG"/librustc_codegen_cranelift*; do
        base="$(basename "$so")"
        ln -f "$so" "$CG_DIR/$base" 2>/dev/null || cp -a "$so" "$CG_DIR/$base"
    done
fi
if [[ ! -d "$CG_DIR" ]] || ! compgen -G "${CG_DIR}/librustc_codegen_cranelift*" >/dev/null; then
    echo "build-galexy-rustc: $TOOLCHAIN has no cranelift backend in $CG_DIR" >&2
    echo "build-galexy-rustc: rustc target libdir is $TARGET_LIBDIR" >&2
    exit 1
fi
if ! printf '' | rustc +"$TOOLCHAIN" -Zcodegen-backend=cranelift --crate-name ___ \
    --print=file-names --crate-type lib - >/dev/null
then
    echo "build-galexy-rustc: host rustc still cannot load cranelift from $CG_DIR" >&2
    exit 1
fi
# Stop after the host backend is loadable. Used to prove a reinstall
# without starting x.py.
if [[ "${GALEXY_RUSTC_SETUP_ONLY:-}" == 1 ]]; then
    exit 0
fi
# A failed first run leaves a stage0 sysroot copied before the backend
# was visible. The next x.py wipes and recopies it; drop the stale tree
# so that copy includes the backend.
STAGE0_SYS="$RUST_SRC/build/x86_64-unknown-linux-gnu/stage0-sysroot"
STAGE0_CG="$STAGE0_SYS/lib/rustlib/${HOST_TRIPLE}/codegen-backends"
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

# A checkout that already has the builtin target skips the patch.
# Cranelift used to report SSE for every x86_64 OS, which is the
# issue #162235 warning on rustc-abi softfloat. Write the same report
# the patch carries when this tree still has the old arm.
python3 - "$RUST_SRC/compiler/rustc_codegen_cranelift/src/lib.rs" <<'PY'
import pathlib, sys
path = pathlib.Path(sys.argv[1])
text = path.read_text()
if "RustcAbi::Softfloat" in text and "sym::soft_float" in text:
    raise SystemExit(0)
old_import = "use rustc_target::spec::{Arch, CfgAbi, Env, Os};"
new_import = "use rustc_target::spec::{Arch, CfgAbi, Env, Os, RustcAbi};"
old_arm = """            Arch::X86_64 if sess.target.os != Os::None => {
                // x86_64 mandates SSE2 support and rustc requires the x87 feature to be enabled
                vec![sym::fxsr, sym::sse, sym::sse2, sym::x87]
            }"""
new_arm = """            Arch::X86_64 if sess.target.os != Os::None => {
                if sess.target.rustc_abi == Some(RustcAbi::Softfloat) {
                    // Softfloat ABI requires +soft-float and forbids sse.
                    // Reporting the hardfloat set is the issue #162235 warning.
                    vec![sym::soft_float]
                } else {
                    // x86_64 mandates SSE2 support and rustc requires the x87 feature to be enabled
                    vec![sym::fxsr, sym::sse, sym::sse2, sym::x87]
                }
            }"""
if old_import not in text or old_arm not in text:
    raise SystemExit(
        "build-galexy-rustc: cranelift target_config anchor missing; "
        "scripts/galexy-rustc.patch is out of date"
    )
text = text.replace(old_import, new_import, 1).replace(old_arm, new_arm, 1)
path.write_text(text)
print("build-galexy-rustc: cranelift reports soft-float for a softfloat ABI")
PY

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
rustc = "$ROOT/scripts/galexy-stage0-rustc.sh"
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
# The outer `cargo run` jobserver fds are not open in this process.
# Pass a clean environment into x.py so its cargo does not reuse them.
# The stage0 wrapper reads the three GALEXY_* variables.
export GALEXY_HOST_RUSTC="$RUSTC"
export GALEXY_HOST_TRIPLE="$HOST_TRIPLE"
export GALEXY_CODEGEN_BACKENDS="$CG_DIR"
env -u CARGO_MAKEFLAGS -u MAKEFLAGS -u MFLAGS \
    -u RUSTC_WRAPPER -u RUSTC_WORKSPACE_WRAPPER \
    -u RUSTFLAGS -u RUSTDOCFLAGS \
    GALEXY_HOST_RUSTC="$RUSTC" \
    GALEXY_HOST_TRIPLE="$HOST_TRIPLE" \
    GALEXY_CODEGEN_BACKENDS="$CG_DIR" \
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
