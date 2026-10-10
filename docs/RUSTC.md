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

The OS side is roughly a third done. What a compiler process needs, and
where Galexy stands today:

| `rustc` needs | Galexy today | Gap |
|---|---|---|
| Hundreds of MB of heap | No user heap; 4-page stack; QEMU default 128 MiB (runner passes no `-m`); frame bitmap covers 512 MiB | `Map` syscall (Milestone 66), 2 GiB QEMU memory, bitmap past 512 MiB |
| At least one spawned thread with an 8 MiB stack, TLS, locks | One task = one thread; no user TLS; FS base unused in ring 3 | `ThreadSpawn` sharing CR3, `Futex`, user `wrfsbase` TLS |
| Read a 30–60 MB sysroot | Ramdisk `open` + `read` + `seek` on archive names ✅ | Big tar, nothing else |
| Write multi-MB `.o` / `.rlib` / ELF outputs | galfs: 32 KiB per file, 128 KiB pool, 8 open files per task | galfs large-volume format, 64 open files |
| Long command lines | `SPAWN_ARG_MAX` = 256, single blob | argv v2 at ≥ 4 KiB |
| Spawn the linker, capture its exit status / stderr | `spawn_with` + Caps at spawn + `pipe` + `wait` ✅ | PAL glue only |
| `fs`: open / read / write / seek / stat / create / remove / rename / readdir | All present as syscalls ✅ (`write` is append-only; readdir is the files snapshot) | Positional write |
| Clock | `Sleep` only | `Clock` (Milestone 66) |
| Environment variables, cwd | None | Waive: PAL returns empty env; cwd lives in the process |

So the user's instinct is right about the *toolchain* and wrong about the
*kernel*: the remaining work is almost entirely Galexy-side runtime
surface (heap, threads, big files), plus one linker decision.

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
`singlethread = true`, `disable-redzone = false` (SYSCALL switches to
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
`scripts/galexy-std-sysroot.py` copies the toolchain `library/` and adds
those unsupported fallbacks (null allocator, generic I/O errors, static
TLS, address-derived `HashMap` keys). That is not a rust-lang/rust fork;
Milestone 70 is the fork, and it deletes this overlay. `stdmin` opts in
with `#![feature(restricted_std)]` and the `galexy-rt` `std` feature,
which drops `galexy-rt`'s panic handler so `panic_abort` can provide it.
The `#[global_allocator]` stays: it is the `Map` bump allocator from
Milestone 66. `String`, `Vec`, `HashMap`, and `format!` work.
`std::fs`, threads, and `println!` do not.

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
What Stage 6 adds on top is TLS (Stage 3), cg_clif's output shape, and
the ring-3 wrapper (Stage 4) — not a new linker.

Exit (met): `test-hellogxc` green through `gxld`; `gxld_image_*_e2e`
green; hostile-input tests return `Err`.

### Stage 3 — Real PAL in a rust-lang/rust fork (Milestone 70)

Fork `rust-lang/rust` as `galexy-rust`, tracked monthly like Xous does.
Three edits upstream documents for exactly this (`wiki.osdev.org/
Porting_Rust_standard_library`):

- `library/std/build.rs`: add `target_os == "galexy"` to the supported
  list (drops `restricted_std`).
- `library/std/src/sys/pal/mod.rs`: `target_os = "galexy" => mod galexy`.
- `library/std/src/sys/pal/galexy/`: copy `unsupported`, then fill in
  modules in this order, each over `galexy-abi` syscalls:

| Module | Backed by | Kernel work |
|---|---|---|
| `alloc` | `Map` | Milestone 66 |
| `stdio` | console Cap `write`; stdin from the keyboard / pipe Cap handed at spawn | none |
| `time` | `Clock` | Milestone 66 |
| `args` | argv v2 blob | argv size bump |
| `os` (env, cwd, exit, errno mapping) | env empty; cwd per-process; `Exit`; `SysError` → `io::ErrorKind` | none |
| `fs` | `Open` / `Read` / `Write` / `Seek` / `Stat` / `Create` / `Remove` / `Rename` / `Truncate`; `read_dir` from the files snapshot | positional `Write` (or `Seek`-then-append semantics documented) |
| `process` | `spawn_with` + `pipe` Caps for stdio + `Wait` for status | none |
| `thread` + `thread_local_key` | `ThreadSpawn` (new slot, same CR3, fresh user stack, kernel stack); per-thread pointer in FS base via user-mode `wrfsbase` (CR4.FSGSBASE is already on; the kernel never touches FS in ring 3) | `ThreadSpawn`, `ThreadExit`, join via the existing process Cap `Wait` |
| `sync` | `std`'s generic futex implementations | `Futex` (`wait(addr, expected)` / `wake(addr, n)`) on the Milestone 57 park / wake machinery |
| `random` | `rdrand` in the PAL | none |
| `net` | `unsupported` until Phase 10 | none |

Key-based TLS (`thread_local_key`) is chosen over native `#[thread_local]`
so the ELF loader need not learn `PT_TLS` yet; the cost is a pointer
chase per access, which a compiler's hot paths tolerate (the OSDev
guide calls this the easy, multi-thread-safe option).

Build and ship the way Xous did: `cargo build` of `library/sysroot`
from the fork for the Galexy target, copy the `.rlib`s into a rustup
toolchain's `lib/rustlib/x86_64-unknown-galexy/lib`, and `rustup
toolchain link galexy`. No `x.py` dance for users of the target.

Exit: `bin/test-std` — threads with a `Mutex<Vec<_>>`, `std::fs` writing
and re-reading a galfs file, `Command::new("echo").output()`,
`Instant::now()` monotonic. Plus one real crate ported with zero
patches (`toml` or `regex` parsing a file from the ramdisk) to prove the
target is honest.

### Stage 4 — Capacity for a compiler process (Milestone 71)

The kernel limits a compiler trips over, all already named in `TODO.md`
as debt:

- **Memory**: `-m 2G` in the runner and `cargo run`; frame bitmap past
  512 MiB (Milestone 6 open box); `Map` budget per task up to 1 GiB;
  user stack size chosen at spawn (`rustc` wants 8 MiB on its main
  compile thread — it spawns that thread itself via the PAL).
- **Storage**: galfs **large-volume format** (v13): 4 KiB blocks, 32-bit
  lengths, extents or double indirection, pool sized to the disk; keep
  the current small format readable. Files up to at least 64 MiB. If
  this lags, a RAM-backed scratch volume with the same `FileBody`
  interface unblocks Stage 5.
- **Ring-3 `gxld`**: the Stage 2 crate behind a thin `main`, receiving
  input Caps and the output Cap at spawn (`LINKER.md` → On the OS).
- **Limits**: `MAX_OPEN_FILES` 8 → 64 (heap table on spawn, not in the
  IF=0 path); `SPAWN_ARG_MAX` 256 → 4 KiB (one page, copied with
  `user_copy`).
- **Sysroot**: `lib/rustlib/x86_64-unknown-galexy/lib/*.rlib` packed into
  the ramdisk tar under `rust/`; read-only is fine, `rustc` only reads it.

Exit: `bin/test-big` — a `std` program allocates 300 MiB, writes a 20 MiB
file, reopens it, checks a hash; `test-fsck` still passes on both
galfs formats.

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
