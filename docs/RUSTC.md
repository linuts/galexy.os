# Rust on Galexy — making `rustc` a tenant, not a rewrite

Plan for getting the real Rust toolchain onto Galexy.OS by the lowest
path of resistance: reuse what upstream already built for new operating
systems, and grow the kernel only where a compiler actually hits a wall.
This is the Phase 11 plan; checkboxes live in `TODO.md` Milestones
**68–73**; the linker has its own plan in `LINKER.md`. `gxc`
(`COMPILER.md`) is frozen as a fixture; this document is about running
*upstream* `rustc` on Galexy.

## What "migrate rustc" means here

Two different goals hide behind the phrase. They are taken in order,
because the second is impossible without the first:

1. **Galexy is a Rust target.** `cargo build --target x86_64-unknown-galexy`
   produces a Galexy ELF for any pure-Rust crate that uses `std`. This
   is what every OS with a Rust port actually did first (Redox, Hermit,
   Xous, Theseus, UEFI). It replaces today's `x86_64-unknown-none` +
   `galexy-rt` hand-rolled runtime with a real `std`.
2. **`rustc` runs on Galexy.** The compiler itself — cross-built on the
   host for the Galexy target, with the Cranelift backend — compiles
   `hello.rs` in ring 3, links it, and the shell runs the result.

Self-hosting in the strong sense (Galexy rebuilding `rustc` on Galexy) is
**not** a goal of this plan. A Cranelift-built `rustc` compiling `rustc`
takes hours on a fast Linux host; under QEMU it is a stunt, not a
feature. We ship the sysroot and the compiler prebuilt on the ramdisk.

## How much is already done — honestly

The toolchain side is largely done upstream, and that is the lever:

| Already exists | Where | What it saves us |
|---|---|---|
| Custom targets from a JSON spec + `-Zbuild-std` | rustc / cargo nightly | No toolchain fork to *compile for* Galexy |
| `std` platform abstraction layer (PAL) with an `unsupported` fallback | `library/std/src/sys/pal/` | `std` builds for an unknown OS today (`restricted_std`); a real port is one directory modeled on `uefi` / `xous` |
| Generic `std` sync over a futex pair | `library/std/src/sys/sync/*/futex.rs` | Mutex / Condvar / RwLock / parking for free once the kernel has `Futex` |
| Cranelift codegen backend, pure Rust | `rustc_codegen_cranelift` in-tree | No LLVM, no C++, no libstdc++ port |
| Pure-Rust object / archive writers | `object`, `ar_archive_writer`, `gimli` | rlibs and `.o` files without binutils |
| `rustc` builds with only the Cranelift backend | `cg_clif/scripts/setup_rust_fork.sh` config | A documented bootstrap configuration to copy |
| Pure-Rust ELF reader for objects and archives | `object` (`read_core`, `no_std`) | A static linker (`gxld`, `LINKER.md`) is a few thousand lines, not a port of `ld` / `lld`; `wild` (pure Rust, Linux-targeted) stays the fallback |
| Pure-Rust unwinder | `unwinding` crate (used by cg_clif, Hermit-class targets) | `panic=unwind` without libgcc |
| Hardware RNG backend | `getrandom` `rdrand` backend | No `/dev/urandom` emulation |

Stage 4 (Milestone 71) is the capacity a compiler process needs. What
`rustc` needs, and where Galexy stands:

| `rustc` needs | Galexy today | Gap |
|---|---|---|
| Hundreds of MB of heap | `Map` up to 1 GiB per task; QEMU `-m 2G`; frame bitmap covers 4 GiB | — |
| At least one spawned thread with an 8 MiB stack, TLS, locks | `ThreadSpawn` shares CR3; `Futex`; key TLS via `wrfsbase`. A spawned thread may ask for 2048 pages (8 MiB); the default stays 16 pages | — |
| Read a 30–60 MB sysroot | Ramdisk `open` + `read` + `seek`; sysroot `.rlib`s are packed under `rust/` | The packed set is the crates the PAL links, not a full 60 MB sysroot |
| Write multi-MB `.o` / `.rlib` / ELF outputs | 64 open files. Writes past 32 KiB promote into a RAM store: 4 KiB blocks, double indirection, 64 MiB per file | Those bytes are not sealed into a v12 disk. Host v13 (`check_image`) is the on-disk large format; the kernel still syncs v12 |
| Long command lines | `SPAWN_ARG_MAX` = 4096 | — |
| Spawn the linker, capture its exit status / stderr | `gxldos` receives input and output Caps at spawn and does not open a path; `test-gxld` links two objects into a program that exits `0x71` | stderr capture is the existing pipe path; `test-gxld` checks the exit code |
| `fs`: open / read / write / seek / stat / create / remove / rename / readdir | All present. `write` is positional; readdir is the files snapshot; files past 32 KiB use the RAM store | — |
| Clock | `Clock` reads `timer_ticks` | — |
| Environment variables, cwd | None | Waive: PAL returns empty env; cwd lives in the process |

The remaining work is the toolchain: cross-building Cranelift-only
`rustc` (Stage 5) and linking its output on Galexy (Stage 6).

## Paths considered

| Path | Verdict |
|---|---|
| **A. Target + PAL + Cranelift-only `rustc` + Rust linker** | **Chosen.** Everything is Rust, every piece exists, each stage is testable alone |
| B. `rustc` via a WASI runtime on Galexy (`rustc.wasm` on `wasmi` / `wasmtime`) | Rejected: `rustc` has no WASI host build (no threads, no `dlopen`, LLVM/cg_clif not on wasm32 hosts); we would be porting the runtime *and* the compiler |
| C. `mrustc` / `gccrs` | Rejected: C++ codebases, need a C/C++ toolchain port and a libc first |
| D. cg_clif JIT mode (`-Cllvm-args=jit-mode`) to skip linking | Rejected: requires every dependency as a dylib + `libloading`; Galexy has no dynamic loader |
| E. Grow `gxc` into Rust | Rejected for this goal: it is a learning compiler; Rust parity is years of frontend work. `gxc` stays as-is |
| F. Native `std` via a libc shim (`relibc` / musl port, `unix` PAL) | Rejected: a libc is a bigger port than a PAL and would import POSIX into a capability OS |

## The plan, in six stages

Each stage leaves the suite green and is useful on its own. The first
two need no fork of the Rust repository and no kernel change.

### Stage 1 — `x86_64-unknown-galexy` target (Milestone 68) ✅

`targets/x86_64-unknown-galexy.json`: `os = "galexy"`, no
`target_family`, `panic-strategy = "abort"`, `relocation-model =
"static"`, `code-model = "large"`, `linker-flavor = "gnu-lld"` with
`rust-lld`, `executables = true`, `has-thread-local = false`,
`singlethread = false` (Milestone 70; Milestone 68 shipped `true`),
`disable-redzone = false` (SYSCALL switches to
the kernel stack; the red zone is the user's). The large code model is
required: a static `R_X86_64_32S` cannot name `USER_IMAGE_BASE`
(`0xc8000000000`). The kernel stays on the prebuilt
`x86_64-unknown-none` target.

`hello`, `init`, `shell` and `util` build with
`--target targets/x86_64-unknown-galexy.json -Zbuild-std=core,alloc
-Zbuild-std-features=compiler-builtins-mem -Zjson-target-spec`.
`galexy-rt` refuses any other `target_os`. The runner's `build.rs`
invokes that cargo itself: a workspace `-Zbuild-std` would also rebuild
`core` for the host and break the prebuilt `std` the host crates link.

`-Zbuild-std=std,panic_abort` is `stdmin`. On nightly-2026-10-08,
upstream `std` does not compile for an OS it has never listed: `cfg_select`
in `sys/alloc`, `sys/io/error`, and `sys/random` has no default, and the
single-thread TLS path is not selected by `not(target_has_threads)`.
`scripts/galexy-std-sysroot.py` copies the toolchain `library/` and, as
of Milestone 70, installs `std-pal/` over it. Milestone 68 used that
script for four unsupported fallbacks only. `stdmin` is a plain
`fn main`: `println!`, `String`, `Vec`, `HashMap`, and `format!` work,
and so do `std::fs`, threads, and the rest of the PAL (Stage 3).

Exit (met): ramdisk program `stdmin` builds a `HashMap<String, Vec<u32>>`,
formats it, and writes through the console Cap. `shell_stdmin_typing_e2e`
types the name and checks the line.

### Stage 2 — `gxld`, the static linker (Milestone 69) ✅

Full plan and status: `LINKER.md`. A `no_std + alloc` library that
turns ELF64 relocatable objects and `ar` archives into the `ET_EXEC`
the loader already accepts, reading inputs with the `object` crate and
taking the layout rules from `galexy-abi`. It absorbed `gxc::elf`, so
`gxc` is a producer of relocatable objects and the linker is the one
place that knows the loader contract. Proof is the differential image:
`rustc -Clinker=gxld -Clinker-flavor=ld` re-links `hello`, `init`,
`shell` and `util` from the exact objects and rlibs it hands
`rust-lld`; `galexy-os-gxld` boots and passes the same typing tests.

It came this early because it is the only Phase 11 item with no
kernel dependency and it de-risks Stage 6, the step most likely to fail.
What Stage 6 adds on top is cg_clif's output shape and the ring-3
wrapper (Stage 4). TLS relocations landed with Stage 3.

Exit (met): `test-hellogxc` green through `gxld`; `gxld_image_*_e2e`
green; hostile-input tests return `Err`.

### Stage 3 — Real PAL (Milestone 70) ✅

The platform layer lives in `std-pal/` at the repo root, one file per
`std::sys` module, laid out as it lands inside `library/std/src/`.
`scripts/galexy-std-sysroot.py` is the in-tree form of "fork
`rust-lang/rust`": it copies the pinned nightly's `library/`, installs
`std-pal/`, generates `sys/pal/galexy/abi.rs` from `galexy-abi` (so std
and the kernel cannot disagree about a syscall number), and patches
the `cfg_select` dispatchers (`build.rs` allowlist, `pal/mod.rs`, the
futex lists, the `thread_local` key). The overlay is content-addressed
under the `--out` directory. Cargo records no build-script dep-info
for paths under `target/`, so a regenerated overlay at a fixed path
would reuse std's stale build script; the directory name is a hash of
the script, `std-pal/`, and the ABI crate. Cargo also caches
`rustc --print sysroot` in `<target-dir>/.rustc_info.json`, keyed on
the compiler and not on `GALEXY_SYSROOT`. The runner deletes that file
before its nested std build. A developer re-running the CI std clippy
after a PAL change should `rm target/.rustc_info.json` too.

| Module | Backed by |
|---|---|
| `alloc` | dlmalloc over `Map` (Milestone 66) |
| `stdio` | console Cap `write`; stdin from the keyboard / pipe Cap handed at spawn |
| `time` | `Clock` |
| `args` | argv v2 blob |
| `os` | env empty; cwd in-process; `Exit`; `SysError` → `io::Error` |
| `fs` | `Open` / `Read` / `Write` / `Seek` / `Stat` / `Create` / `Remove` / `Rename` / `Truncate`; `read_dir` from the files snapshot. galfs `Write` is positional at the cursor |
| `process` | `spawn_with` + `pipe` Caps for stdio + `Wait` for status |
| `thread` + `thread_local_key` | `ThreadSpawn` (new slot, same CR3, fresh user stack, kernel stack); per-thread pointer in the FS base via user-mode `wrfsbase` (CR4.FSGSBASE is already on; the kernel never touches FS in ring 3). Join is process-Cap `Wait` |
| `sync` | `std`'s generic futex implementations over `Futex` |
| `random` | `rdrand` in the PAL |
| `net` | `unsupported` until Phase 10 |

Key-based TLS stays. `has-thread-local` is still false, so compiled
code does not emit `#[thread_local]`. `gxld` does link `.tdata`/`.tbss`
and resolves `TPOFF32` / `TPOFF64` / `GOTTPOFF`, and it emits `PT_TLS`
after the three `PT_LOAD`s. The loader maps only `PT_LOAD`s and skips
that header. No program shipped today carries a TLS block.

Every PAL and `galexy-rt` stub that contains `asm!` is
`#[inline(never)] extern "C"`, so a code generator without inline
assembly links an ordinary function.

Ship the way Xous did, without a separate compiler checkout:
`cargo xtask sysroot` builds `library/sysroot` for
`x86_64-unknown-galexy`, copies the `.rlib`s and the sibling `.rmeta`
files this nightly writes beside them into
`lib/rustlib/x86_64-unknown-galexy/lib` of a toolchain directory, and
`rustup toolchain link galexy`. The spec has to be resolved by name
from that toolchain. Building against the JSON path stamps every rlib
with `x86_64-unknown-galexy-<hash>`, which later compiles (they load
the spec from the sysroot) will not accept. `bin/rustc` in the
toolchain is a script that injects `-Zunstable-options` — a custom
target still requires it — and clears `LD_LIBRARY_PATH`, which
otherwise makes rustc load the host `librustc_driver` and report the
pinned nightly as its sysroot. `bin/` and the driver libraries are
hardlinks for the same reason: a symlink resolves back to the nightly.
`--check` compiles a std program with `rustc` and with
`cargo +galexy build --target x86_64-unknown-galexy` (no
`-Zbuild-std`) and accepts the ELF the way `sched/loader.rs` does.
The image base is still a link argument
(`--image-base=USER_IMAGE_BASE --no-pie`), the same line each
userspace `build.rs` already passes.

Exit (met): `bin/test-thread`, `bin/test-futex`, and `bin/test-std` —
threads with a `Mutex<Vec<_>>`, `std::fs` writing and re-reading a
galfs file, `Command::new("echo").output()`, `Instant::now()`
monotonic, and `toml` 1.x parsing a ramdisk file with zero patches.
`stdtest` prints `stdtest: passed (75 checks)`.

### Stage 4 — Capacity for a compiler process (Milestone 71) ✅

- **Memory**: `-m 2G` in the runner and `cargo run`; the frame bitmap
  covers 4 GiB; `Map` budget per task is 1 GiB. A spawned thread may
  ask for 8 MiB (`THREAD_STACK_PAGES_MAX`); the leader's stack stays
  4 pages and the thread default stays 16 pages.
- **Storage**: writes past the v12 32 KiB file promote into a RAM
  store with the same file body: 4 KiB blocks, `u32` length, double
  indirection, 64 MiB. v12 disks still load and sync; promoted bytes
  stay in RAM. Host `galexy-galf` v13 is the sealed large-volume
  image (extent map, pool sized to the disk, files of 64 MiB).
  `check_image` dispatches on the version, so v12 and v13 both fsck.
  The kernel does not format or load v13 yet.
- **Ring-3 `gxld`**: `gxldos` is a thin `main` over the Stage 2 crate.
  The kernel test installs read Caps and one write Cap at spawn;
  the program does not open a path. `test-gxld` links two objects
  into a program that exits `0x71`.
- **Limits**: `MAX_OPEN_FILES` is 64. The table is a heap `Vec`
  allocated when the task is registered, before the IRQ gate.
  `SPAWN_ARG_MAX` is 4 KiB, copied with `user_copy`. `PROC_CAP_BASE`
  is `0x80` so the file band does not overlap process caps.
- **Sysroot**: the `.rlib`s the PAL links are packed into the ramdisk
  tar under `rust/` (one per crate, newest hash). `.rmeta` is left
  out: it duplicates the rlib, and a debug `libcore.rmeta` is tens of
  megabytes inside every disk image.

Exit: `bin/test-big` (KVM only) allocates 300 MiB, writes a 20 MiB
file, re-reads it, and checks a hash. `test-fsck` checks the live v12
table and a promoted large file. Host tests check a sealed v13 image.

### Stage 5 — `rustc` cross-built for Galexy, Cranelift only (Milestone 72)

Use the configuration cg_clif's own CI uses to build a `rustc` with no
LLVM backend, then point it at our target:

```toml
[build]
full-bootstrap = true
target = ["x86_64-unknown-galexy"]

[rust]
codegen-backends = ["cranelift"]
llvm-tools = false
download-rustc = false

[llvm]
download-ci-llvm = true   # host only: the *host* compiler keeps LLVM
```

`./x.py build --stage 1 compiler/rustc --target x86_64-unknown-galexy`
yields `rustc` + `rustc_driver` + `rustc_codegen_cranelift` as Galexy
ELFs, built by the Linux host compiler. LLVM is never ported; it only
lives in the host toolchain that does the cross build. The Galexy
`rustc` has one backend, Cranelift, and `-Zcodegen-backend` is not
needed on-OS.

Expected patch set in the fork, all cfg-gated on `target_os = "galexy"`
and each small:

| Crate | Issue | Patch |
|---|---|---|
| `rustc_target` | builtin spec | add `x86_64_unknown_galexy` so `--target x86_64-unknown-galexy` works without a JSON on-OS |
| `rustc_data_structures::memmap` | `memmap2` is unix/windows only | use the existing read-into-`Vec` fallback (today behind `cfg(miri)`) |
| `jobserver` | unix/windows only | the dummy implementation it already ships for wasm, enabled for galexy (`[patch.crates-io]`) |
| `getrandom` | no OS source | `--cfg getrandom_backend="rdrand"` |
| `stacker` / `psm` | stack growth via `mmap` on unix | `psm`'s x86_64 SysV assembly is OS-neutral; stacker's non-unix path allocates stacks on the heap — verify, else `-Zstack-size`-only |
| `rustc_driver_impl` | signal handlers, `ctrlc` | already `cfg(unix)` — confirm nothing leaks |
| `rustc_session` / `rustc_codegen_ssa` | default linker | `linker-flavor = gnu` with linker `gxld` (Stage 6) |
| `psm` build script | needs a cross assembler | `CC_x86_64_unknown_galexy=clang`, `--target=x86_64-unknown-none-elf` |
| panic strategy | `FatalError::raise` uses `resume_unwind` | MVP on `panic=abort`: errors print, then abort. Then `unwinding` crate for real `panic=unwind` |

Inline assembly is the one Cranelift-specific trap: cg_clif compiles
`asm!` by shelling out to an assembler (`CG_CLIF_FORCE_GNU_AS`), which
Galexy does not have. The prebuilt sysroot is compiled on the host, so
`core` / `std` / `compiler_builtins` asm is already machine code; but
`#[inline]` functions instantiate in the *caller's* crate. Rule: every
`galexy-rt` / PAL syscall stub is `#[inline(never)] extern "C"` so user
crates compiled on-OS never contain `asm!`. `core::hint::black_box` is
an intrinsic, not asm, in cg_clif.

Exit: on-OS `rustc --emit=obj hello.rs` under `test-rustc-obj` produces
an ELF relocatable whose symbol table and section sizes match the
host-side cg_clif output for the same source and sysroot (the runner
compares the two). Also `rustc --print cfg` and an intentional type
error whose diagnostic text is asserted.

### Stage 6 — Link on Galexy, run the result (Milestone 73)

`rustc` writes `.o` files and spawns a linker. That linker is the
ring-3 `gxld` from Stages 2 and 4, extended for cg_clif output: rlib
archives whose `lib.rmeta` member must be skipped, `TPOFF32` for TLS
(arrives with threads in Stage 3), `.eh_frame` kept once `unwinding`
lands. The differential test moves to the real inputs: the
galexy-target sysroot plus `hello.rs`, linked by `gxld` on the host and
by `rust-lld`, both booting.

Fallback only if `gxld` cannot cope: **port `wild`** (`--no-fork
--threads=1`, `fork` and `mimalloc` features off, `memmap2` replaced by
read-into-memory). It is pure Rust and supports static non-relocatable
output, but it is Linux-shaped — threads, `rayon`, `mmap` — and would
only build once Stage 3 is complete, which is why it is not the first
choice.

`rustc` sees the linker through `-Clinker=gxld` with `linker-flavor =
gnu`, spawned over `spawn_with` with stdio pipes, exit status via the
process Cap.

Exit: `test-rustc-hello` — in ring 3, `rustc hello.rs -o hello` on
galfs, `./hello` spawns and prints `hello from rustc on galexy`; a
second program using `std::fs` + `std::thread` compiled on-OS also
runs. This is the Phase 11 gate.

## Explicit non-goals

- `cargo` on Galexy: it links `libgit2`, `curl`, OpenSSL (C) by default.
  A thin `gxbuild` driver that invokes `rustc` per crate from a manifest
  is the Galexy answer if it is ever wanted.
- Proc macros on-OS (need a dylib loader for the macro `.so`).
- LLVM, `rust-lld`, GCC, `mrustc`, a libc, POSIX emulation.
- Rebuilding `rustc` or `std` on Galexy.
- Native `#[thread_local]` and `PT_TLS` in the loader before the key-based
  TLS is measured as a problem.
- Incremental compilation on-OS (`-Cincremental` needs many small files;
  off by default for `rustc` invocations anyway).

## Risks and the fallback for each

| Risk | Fallback |
|---|---|
| Fork drift: `std`'s PAL internals move between nightlies | Track one nightly per quarter; the PAL is ~15 files, Xous forward-ports in an afternoon per release |
| `rustc`'s dependency graph grows a unix-only crate | `[patch.crates-io]` with the wasm/dummy path; the set above is the one cg_clif + Hermit ports already hit |
| cg_clif rejects some inline asm in the sysroot at *host* build time | Host build uses GNU `as`; only on-OS compiles must avoid `asm!`, which the `#[inline(never)]` rule enforces |
| `gxld` meets a relocation or section cg_clif emits that v0 does not handle | The differential test names it early (Stage 2 runs on rustc-built `shell` from day one); `wild` is the fallback (Stage 6) |
| A subtly wrong link is a debugging sink | No layout or relocation change lands without the `rust-lld` differential green (`STYLE.md` → Linker) |
| `panic=abort` turns every `FatalError` into a process abort | Acceptable for the gate; `unwinding` crate + `.eh_frame` from cg_clif afterwards |
| 2 GiB of guest RAM slows TCG runs | Stages 4–6 tests are KVM-only in CI (Milestone 64 provides the path); TCG keeps the Stage 1–3 tests |
| galfs large-volume rewrite is bigger than hoped | RAM scratch volume for outputs; galfs v13 lands later without blocking the gate |

## Order of operations vs. the v1.0 roadmap

Stages 1 and 2 can start now (no kernel change; host-only work beside
Phase 9). Stage 3 depends on Milestone 66 (`Map`, `Clock`, argv) and
adds `ThreadSpawn` + `Futex`, which should be designed in `PROCESS.md`
beside `Channel` so the Milestone 67 ABI freeze covers them. Stages 4–6
come after `v1.0`; they need KVM in the runner (Milestone 64) to be
testable in reasonable time. Nothing here blocks Phase 10; `net` in the
PAL is the Phase 10 hook.
