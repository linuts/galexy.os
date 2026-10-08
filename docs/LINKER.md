# LINKER — `gxld`, a static ELF64 linker for the Galexy loader

Plan for the one linker Galexy will have. Checkboxes: `TODO.md`
Milestone **69** (host library), **71** (ring-3 program), **73** (links
`rustc` output). Rules: `docs/STYLE.md` → Linker. It is Stage 2 of the
`rustc` plan (`RUSTC.md`) and the only Phase 11 item with no kernel
dependency, so it comes first.

## Why a linker at all

Today nothing on Galexy links. `gxc` emits a finished `ET_EXEC`
directly (`crates/gxc/src/elf.rs`, one code blob, no relocations), and
every real program is linked on the host by `rust-lld` with
`--image-base=USER_IMAGE_BASE --no-pie`. That is fine while every
program is one translation unit built on Linux. It stops being fine at
three points:

| Trigger | Why a linker is unavoidable |
|---|---|
| `rustc` on Galexy (Milestone 73) | `rustc` emits one `.o` per codegen unit and expects an external linker to combine them with the sysroot `.rlib` archives |
| `gxc` past one file | Separate compilation, or calling into a precompiled runtime instead of inlining a syscall prelude |
| Mixed producers | A gxr object and a rustc object in one program |

What it is **not** for: dynamic linking. The loader has no interpreter,
W^X forbids patching text at run time, and a capability OS does not
want ambient library search paths. `gxld` is a static linker, which is
also why it is small — dynamic symbols, PLT/GOT fix-ups at load time,
`DT_NEEDED`, versioning, and linker scripts are the bulk of `ld`.

## How it fits the OS

Three placements, two of which are the same code:

1. **Library (`crates/gxld`)**, `no_std + alloc`, host-tested. Inputs are
   byte slices, the output is a `Vec<u8>`. `gxc` on the host calls it
   instead of `gxc::elf`. This is the version that exists first and
   runs in the existing CI and runner with no kernel change.
2. **Ring-3 program** (Milestone 71), a thin `main` over the same crate.
   The compiler driver `spawn_with`s it, `give`s it read Caps on the
   input files and one write Cap on the output, and `Wait`s on the
   process Cap for the exit status. The linker never opens a path; it
   can read only what it was handed and write only the one output.
   This needs the user heap (Milestone 66) and large galfs files
   (Milestone 71): objects for a `std` program are megabytes.
3. **Never the kernel.** The loader stays a static-ELF mapper
   (`sched/loader.rs`). The one coupling worth adding is agreement by
   construction: the linker takes `USER_IMAGE_BASE`, the page size and
   the W^X rule from `galexy-abi` and validates its own output with
   the same `elf_bytes_wx_ok` the loader runs. `gxc::validate_elf`
   becomes the shared post-link check.

## Inputs and output

**Inputs.** ELF64 `ET_REL` x86_64 objects, and `ar` archives of them
(rlibs are `ar` archives with one extra `lib.rmeta` member to skip).
Parsing is the `object` crate with `read_core` + `elf` + `archive`,
which builds without `std`. Nothing is written with `object`; the
output is emitted by hand, as `gxc::elf` does today, because the
output shape is fixed and tiny.

Two relocation styles must both work: today's userspace is compiled
for `x86_64-unknown-none`, which is PIC by default (so `PLT32` and
`GOTPCRELX` dominate even though the final image is `--no-pie`); the
Milestone 68 target sets `relocation-model = static`, which shifts
objects toward `PC32` / `32S`. cg_clif output for the sysroot may be
either.

**Output.** One `ET_EXEC`, `e_entry = _start`, three page-aligned
`PT_LOAD`s at `USER_IMAGE_BASE` in this order, each obeying W^X:

| Segment | Flags | Sections merged |
|---|---|---|
| rodata | R | `.rodata*`, the synthesized GOT, `.gcc_except_table`, later `.eh_frame` |
| text | RX | `.text*`, `.init`/`.fini` if present |
| data | RW | `.data*`, `.data.rel.ro*` (static image, so it is just data), `.bss*` as `memsz > filesz`, `.tbss`/`.tdata` once TLS exists |

Discarded in v0: `.eh_frame`, `.eh_frame_hdr`, `.debug_*`, `.comment`,
`.note*`, `.llvm_addrsig`, `.symtab`/`.strtab` (no symbol table in the
image; the loader does not read one). `.eh_frame` returns when
`unwinding` lands (Milestone 72 follow-on).

## Scope of v0

- **Symbol resolution**: global, weak, local, `COMMON`; archive members
  pulled on demand by undefined symbol until a fixpoint; duplicate
  strong definitions and undefined references are errors naming the
  referencing object and section.
- **Layout**: section merge by name prefix into the three segments;
  alignment honored; `_start` required; `__executable_start`,
  `_end`, `__bss_start`/`_edata` provided because `compiler_builtins`
  and runtimes expect them.
- **Relocations** (x86_64 SysV, small code model, static image):

| Kind | Handling |
|---|---|
| `R_X86_64_64` | `S + A` |
| `R_X86_64_32`, `32S` | `S + A`, range-checked (the image lives below 4 GiB, so these are legal) |
| `R_X86_64_PC32` | `S + A - P` |
| `R_X86_64_PLT32` | `S + A - P` — no PLT, direct call |
| `R_X86_64_GOTPCREL`, `GOTPCRELX`, `REX_GOTPCRELX` | One GOT slot per symbol in the rodata segment, `G + GOT + A - P`; relaxation to a direct `lea`/`mov` is an optimization, not v0 |
| `R_X86_64_TPOFF32` | Milestone 70, with threads and TLS |
| anything else | `Err` naming the kind, object and section |

- **Errors over panics**: every input-derived value is bounds-checked;
  truncated headers, overlapping sections, out-of-range relocations and
  unknown kinds return `Err`. Fuzz-style host tests cover each.

Rough size: a few thousand lines. The comparable part of `wild` is
much larger because of dynamic linking, linker scripts, LTO,
`--gc-sections`, debug info and speed, none of which are in scope.

## Testing — differential or it did not happen

The strong test is available today and needs no kernel work:

1. Host `rustc --emit=obj` (and the rlib archives it already produces)
   for `hello`, `util` and `shell`.
2. Link each twice: `rust-lld --image-base=... --no-pie` and `gxld`.
3. Pack both images in the ramdisk under two names; the runner boots
   both and runs the same cases (`test-realprogram`, `test-hellogxc`,
   the `shell_*_e2e` typing tests). Symbol addresses may differ;
   behavior may not.

`shell` matters because it is the input that carries archive members,
GOT relocations, `.bss`, and `compiler_builtins` symbols. A linker that
passes on `hello` and fails on `shell` is the expected first state and
the test names the missing piece.

Milestone 73 moves the same test to the real inputs: the
galexy-target sysroot plus `hello.rs` compiled by cg_clif.

## Phases

| Milestone | Delivers |
|---|---|
| **69** | `crates/gxld` library; `gxc` emits a relocatable object and links through it (`gxc::elf` deleted); differential suite on `hello`, `util`, `shell`; hostile-input tests |
| **70** | `TPOFF32` and `.tdata`/`.tbss` the same milestone threads and TLS arrive |
| **71** | Ring-3 `gxld` over Caps; `bin/test-gxld` links two ramdisk objects into a program that runs |
| **72** (follow-on) | `.eh_frame` kept for `unwinding` |
| **73** | Links cg_clif output: rlibs with `lib.rmeta` skipped; `rustc -Clinker=gxld` on-OS; the Phase 11 gate |

## Non-goals

- Dynamic linking, `PT_INTERP`, `PT_DYNAMIC`, PLT, shared objects,
  `dlopen`.
- Linker scripts beyond the fixed three-segment layout.
- LTO, `--gc-sections`, identical-code folding, incremental linking.
- Debug info in the image (`.debug_*` are discarded; host tools keep
  the unstripped objects).
- Any architecture but x86_64; any format but ELF64.
- Speed. Inputs for a hello are kilobytes; for `rustc` output,
  megabytes. Correctness and the differential test come first.

## Risks

| Risk | Mitigation |
|---|---|
| A subtly wrong image is a debugging sink | No layout or relocation change without the `rust-lld` differential green; `elf_bytes_wx_ok` and `validate_elf` on every output |
| cg_clif emits a relocation or section v0 does not know | The error names it; the differential test on `shell` surfaces most of them before `rustc` exists; `wild` is the written fallback (`RUSTC.md` Stage 6) |
| `object` crate API churn | Pin the version; the read API used is small (sections, symbols, relocations, archive members) |
| Scope creep toward `ld` | Non-goals above are the gate; a new relocation kind or section needs a checkbox |
| Ring-3 version needs the heap and big files | Library first; the program is Milestone 71 and does not block 69 |

## Related docs

| Doc | Role |
|---|---|
| `COMPILER.md` | `gxc`, the first producer of objects for `gxld` |
| `RUSTC.md` | Stage 2 (library), Stage 4 (ring-3), Stage 6 (links `rustc` output) |
| `DESIGN.md` | Loader contract the output must satisfy |
| `PROCESS.md` | `spawn_with`, `give`, `Wait` — how the ring-3 linker receives Caps |
| `STYLE.md` → Linker | Rules |
