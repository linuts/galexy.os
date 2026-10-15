# Rust on Galexy — making `rustc` a tenant, not a rewrite

Plan for getting the real Rust toolchain onto Galexy.OS by the lowest
path of resistance: reuse what upstream already built for new operating
systems, and grow the kernel only where a compiler actually hits a wall.
This is the Phase 11 plan; checkboxes live in `TODO.md` Milestones
**68–73**. Milestones **74–77** (Phase 12) are the login shell.
The linker has its own plan in `LINKER.md`. `gxc` was
removed in Milestone 73 (`COMPILER.md`); this document is about running
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
   Milestone 73 proved the compile and the link inside a test kernel.
   Milestones 74–77 put that compiler on the login shell:
   `rustc hello.rs -o /hello` then `/hello`.

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
| At least one spawned thread with an 8 MiB stack, TLS, locks | `ThreadSpawn` shares CR3; `Futex`; key TLS via `wrfsbase`. A spawned thread may ask for 4352 pages (17 MiB); the default stays 16 pages | — |
| Read a 30–60 MB sysroot | Ramdisk `open` + `read` + `seek`; sysroot `.rlib`s are packed under `rust/` | The packed set is the crates the PAL links, not a full 60 MB sysroot |
| Write multi-MB `.o` / `.rlib` / ELF outputs | 64 open files. Writes past 32 KiB promote into a RAM store: 4 KiB blocks, 80 MiB per file, 128 MiB shared pool | Those bytes are not sealed into a v12 disk. Host v13 (`check_image`) is the on-disk large format; the kernel still syncs v12 |
| Long command lines | `SPAWN_ARG_MAX` = 4096 | — |
| Spawn the linker, capture its exit status / stderr | `gxldos` receives input and output Caps at spawn and does not open a path; `test-gxld` links two objects into a program that exits `0x71` | stderr capture is the existing pipe path; `test-gxld` checks the exit code |
| `fs`: open / read / write / seek / stat / create / remove / rename / readdir | All present. `write` is positional; readdir is the files snapshot; files past 32 KiB use the RAM store | — |
| Clock | `Clock` reads `timer_ticks` | — |
| Environment variables, cwd | None | Waive: PAL returns empty env; cwd lives in the process |

Stages 1–6 are in the tree. Stage 5 cross-builds Cranelift-only
`rustc` and runs it on Galexy. Stage 6 links that output with `gxld`
and runs the program.

## Paths considered

| Path | Verdict |
|---|---|
| **A. Target + PAL + Cranelift-only `rustc` + Rust linker** | **Chosen.** Everything is Rust, every piece exists, each stage is testable alone |
| B. `rustc` via a WASI runtime on Galexy (`rustc.wasm` on `wasmi` / `wasmtime`) | Rejected: `rustc` has no WASI host build (no threads, no `dlopen`, LLVM/cg_clif not on wasm32 hosts); we would be porting the runtime *and* the compiler |
| C. `mrustc` / `gccrs` | Rejected: C++ codebases, need a C/C++ toolchain port and a libc first |
| D. cg_clif JIT mode (`-Cllvm-args=jit-mode`) to skip linking | Rejected: requires every dependency as a dylib + `libloading`; Galexy has no dynamic loader |
| E. Grow `gxc` into Rust | Rejected. Milestone 73 removed `gxc`. The on-OS compiler is this `rustc` |
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
taking the layout rules from `galexy-abi`. It absorbed `gxc::elf`.
`gxc` used to produce relocatable objects; Milestone 73 removed that
crate, and the linker is the one place that knows the loader contract.
Proof is the differential image:
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
| `net` | `unsupported`. Phase 10 does not grow `std::net`. A program holds a lane Cap (`docs/NET.md`). Phase 14's Python does not grow it either (`docs/PYTHON.md`) |

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
  4 pages and the thread default stays 16 pages. Milestone 72 raised
  the maximum to 17 MiB.
- **Storage**: writes past the v12 32 KiB file promote into a RAM
  store with the same file body: 4 KiB blocks, `u32` length, double
  indirection, 64 MiB (80 MiB per file since Milestone 72, for
  `core`'s metadata; 128 MiB shared pool since Milestone 73). v12
  disks still load and sync; promoted bytes
  stay in RAM. Host `galexy-galf` v13 is the sealed large-volume
  image (extent map, pool sized to the disk, files of 64 MiB).
  `check_image` dispatches on the version, so v12 and v13 both fsck.
  The kernel does not format or load v13 yet. Milestone 98 is that
  load, and it keeps the actor table. Milestone 99 sizes the object
  table to one desk compile (`docs/GALFS.md` → Scheduled commits).
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

### Stage 5 — `rustc` cross-built for Galexy, Cranelift only (Milestone 72) ✅

`scripts/build-galexy-rustc.sh` builds stage 1 of nightly-2026-10-08
(`1d81eb4ad`) with `codegen-backends = ["cranelift"]` and
`--host` / `--target` set to `x86_64-unknown-galexy.json`. The result
is one static ELF. `rustc_driver` and `rustc_codegen_cranelift` are
rlibs inside it, not separate ramdisk files. Stage 2 is not run: that
would try to execute the Galexy ELF on Linux. `download-ci-llvm` is
false. Cranelift does not compile LLVM, and LLVM cannot instruction-select
this target. The fork lives outside this repo; `scripts/galexy-rustc.patch`
is the diff.

`channel = "nightly"` so the guest reports
`1.101.0-nightly (1d81eb4ad 2026-10-07)`, the same string stage 0
stamped into `core`'s `.rmeta`. A `dev` channel says `1.101.0-dev` and
the guest rejects the crate (`E0514`).

What the fork changes, cfg-gated on `target_os = "galexy"`:

| Crate | What landed |
|---|---|
| `rustc_target` | builtin `x86_64_unknown_galexy` (soft-float, panic abort, large code model, linker still `rust-lld`) |
| `rustc_interface` | the `cranelift` backend arm |
| `rustc` `main` | hops onto a 17 MiB thread before parsing argv. The leader stack stays 4 pages |
| `rustc_codegen_ssa` | the codegen coordinator uses that same stack. The default 64 KiB thread overflows it |
| `rustc_span` | `FatalError::raise` calls `process::exit(1)`. `panic=abort` cannot catch the unwind sentinel |
| `rustc_data_structures` | memmap reads into a `Vec` |
| `rustc_metadata` | proc-macro dlopen returns "no dynamic loader" |
| `compiler_builtins` | the `mem` feature, so `memcpy` / `memset` / `memcmp` / `memmove` link |
| `target-lexicon` | the triple maps to x86_64 / none / elf. No new OS variant |
| `getrandom` | `--cfg getrandom_backend="rdrand"` |
| `jobserver` | the crate's existing non-unix dummy. No crates.io patch |
| `stacker` / `psm` | not in the rustc link closure |

`--print cfg` uses the builtin name. `--emit=obj` passes
`/x86_64-unknown-galexy.json` plus `-Zunstable-options`, because a
JSON target's spec hash must match the rlibs stage 0 built. Bootstrap
still passes `-Zembed-metadata=no`, so the image carries each rlib's
sibling `.rmeta`. A `no_std` hello needs only `libcore` and
`libcompiler_builtins`. `core`'s metadata is about 66 MiB, which is
why one file may be 80 MiB. The shared pool is 128 MiB so a `std`
sysroot fits beside that metadata. The on-disk v13 file cap stays 64 MiB.

Cranelift injects SSE for this OS. The kernel enables `CR4.OSFXSR` on
every CPU (an AP's trampoline otherwise reloads CR4 with only PAE) and
new threads restore a masked `MXCSR`. The soft-float warning from
issue 162235 stays a warning.

Inline `asm!` in a crate compiled on-OS is still out of scope. The
sysroot was built on the host. PAL syscall stubs stay
`#[inline(never)]`.

Exit (met): `rustc_obj_matches_host` under TCG. `--print cfg` prints
`target_os="galexy"` and `panic="abort"` and exits 0. `bad.rs` prints
`mismatched types` and exits 1. `--emit=obj` of `hello.rs` matches the
host Cranelift object's section sizes and function/object symbol
sizes. CI skips that boot unless the ELF existed when the runner image
was built; x.py does not run inside the qemu job. Stage 6 links
that object.

### Stage 6 — Link on Galexy, run the result (Milestone 73) ✅

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

Exit (met): `rustc_hello_runs` under TCG. Host `hello-lld` and
`hello-gxld` both print `hello from rustc on galexy`. On-OS
`rustc hello.rs -o hello` exits 0 and `./hello` prints the same line.
`files.rs`, compiled on-OS, writes `/scratch` from `std::thread` and
prints `files-ok`. `gxld` linked the sysroot hello, so `wild` was not
ported. `.eh_frame` stays out until `panic=unwind`. `std` closes every
file cap in `rt::cleanup`: stdio is not a `File`, and `Command::output`
reads the linker pipe to EOF before `Wait`.

On Galexy the `gxld` CLI hops onto a 17 MiB thread before it reads
inputs, the same size as `rustc`'s compile thread. The leader stack
stays 4 pages. Linking a one-line `println!` dropped a `BTreeMap` of
`gxld::link::Def` on that leader and wrote the unmapped guard
(`err=0x6`, `rip=0xc8000069a07`, `cr2=0xc803ffff1f0`, 3600 bytes below
the stack at image+1GiB).

### On the shell (Milestones 74–77, and the desk image)

`test-rustc-hello` is not a seat. The login shell is:

- `cargo run` builds `target/galexy-rustc/rustc` when it is missing
  (`scripts/build-galexy-rustc.sh`, including a clone of
  `1d81eb4ad9cd207e3e638bd32b17ec4fce8412a6` into `$HOME/galexy-rust`
  when that checkout is absent) and then boots `galexy-os-desk`.
  The same run builds `python` when that ELF is missing. Later
  runs reuse the ELFs. `GALEXY_SKIP_COMPILERS=1` skips both and
  boots the small image. That image's shell prints
  `rustc: not in this image` and `scripts/build-galexy-rustc.sh`.
  The desk image is large enough that a default FAT format is
  FAT32. BIOS stage 2 does not read a FAT32 root directory
  (`unimplemented!` in `fat.rs`), so the image is FAT16 with a
  cluster size that stays under 65525 clusters.
  Stage 0 compiles the Galexy sysroot with the host nightly's
  Cranelift backend. This nightly has no `codegen-backends` directory
  until `rustc-codegen-cranelift` is installed; the script installs
  that component. If `lib64/rustlib` exists, `rustc` looks there
  instead of `lib/` and does not see the component. The script
  removes that empty shadow. Stage 0's `rustc` is
  `scripts/galexy-stage0-rustc.sh`, which hardlinks that backend
  into a real directory on the `--sysroot` bootstrap passes before
  the real compiler runs. A directory symlink is followed by the
  later sysroot copy, and that copy deletes the host backend.
  `rustup component remove` will not drop a component whose manifest
  paths are already gone, so the script recreates those empty paths,
  removes the component, and installs it again.
- The compiler ramdisk (~214 MiB ELF, ~90 MiB sysroot) is attached
  to `test-rustc`, `test-rustc-hello`, `galexy-os-rustc`, and
  `galexy-os-desk`. Both ELFs means one desk image: the compiler
  pieces and `python`. `--rustc` and `--python` still parse. They
  do not select a different image. They error only when that ELF
  is missing. `cargo test` does not build them, so the shared test
  ramdisk stays the small userspace set and CI does not run `x.py`
  or fetch RustPython (Milestone 74, then Milestone 96).
- The login kernel attaches the JSON and `srlib/*` when the ramdisk
  contains `srlib/` (Milestone 75). A diskless or auto-unlocked boot
  attaches before init. A sealed volume stays locked until the
  passphrase, and loading that disk replaces the RAM table, so the
  attach runs again from the main loop after unlock. Each rlib and
  rmeta stays the ramdisk mapping until a write copies that file into
  4 KiB blocks. Before the attach, a stored admin quota below the
  128 MiB pool (an older sealed disk) is raised to the pool, and the
  object limit is raised to the table size. The must-change flag is
  kept. A higher quota is left alone. A v12 disk file still cannot
  hold `core`'s ~66 MiB `.rmeta`, so the libraries are not sealed and
  the attach repeats every boot. An empty rlib header left by an
  earlier seal is replaced from the ramdisk; a file that already has
  the right length is kept. A program with arguments inherits the
  session, so `rustc` can open those files (Milestone 76). A bare name
  still does not.
- `spawn` of a path loads that galfs ELF (Milestone 77). The typed
  gate is `rustc hello.rs -o /hello` and then `/hello`, which prints
  `hello from rustc on galexy`. The bare label `hello` stays the
  ramdisk program, so `-o hello` is `Unsupported`. The shell supplies
  `--edition 2024` when the line did not name an edition. The nightly
  otherwise stays on 2015 and prints that the edition was unspecified.
  A ramdisk program with arguments
  receives the query grant (`read_dir` of the sysroot) and the loader
  grant (`rustc` spawns `gxld`). A galfs path does not receive the
  loader. A pipeline stage does not either, so `rustc` is the command
  itself. The sysroot is imported into the admin tree, so another
  account does not see those libraries. `read_dir` is one 8192-byte
  snapshot. The login image's ramdisk names plus the sysroot paths
  fit; many extra files before `rustc` starts can drop a library from
  that list. A shell Cap-waiting on `rustc` does not idle-logout until
  the compile finishes or 15 min pass with no keys.

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
| `panic=abort` turns every `FatalError` into a process abort | Stage 5 calls `process::exit(1)` after the diagnostic. `unwinding` plus `.eh_frame` is still the follow-on |
| 2 GiB of guest RAM slows TCG runs | `rustc_obj_matches_host` ran under TCG on a host whose KVM cannot create a vCPU. CI runs it only when the ELF was already built |
| galfs large-volume rewrite is bigger than hoped | RAM scratch volume for outputs. Phase 15 (Milestones 98–99) is the on-disk load; it does not block this gate |

## Order of operations vs. the v1.0 roadmap

Stages 1 and 2 can start now (no kernel change; host-only work beside
Phase 9). Stage 3 depends on Milestone 66 (`Map`, `Clock`, argv) and
adds `ThreadSpawn` + `Futex`, which should be designed in `PROCESS.md`
beside `Channel` so the Milestone 67 ABI freeze covers them. Stages 4–6
come after `v1.0`; they need KVM in the runner (Milestone 64) to be
testable in reasonable time. Those stages have since landed (Milestones
68–73). Phase 14 (`docs/PYTHON.md`, Milestones 91–96) has landed:
`cargo run` carries `python`, and the compiler, when those ELFs
were built. Phase 10 (`docs/NET.md`, Milestones 78–85) and Phase 13
(`docs/USB.md`, Milestones 86–88) are still required before the
`v1.0` tag, along with a KVM pass. `std::net` stays unsupported;
a lane is not a `TcpStream`, and RustPython does not add a socket
module.
