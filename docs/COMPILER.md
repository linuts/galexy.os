# COMPILER — mini Rust for Galexy hello-world

galexy.os already runs Rust user programs. Today they are built on the
**host** with nightly `rustc` + `galexy-rt`, linked as static ELF64 at
`USER_IMAGE_BASE`, and loaded by `sched/loader.rs`.

This document plans a **small Galexy-owned compiler** — just enough to
turn a tiny Rust-shaped program into a runnable hello ELF. It is **not**
a plan to vendor or reimplement full `rustc`.

## Status — frozen at gxr v0

Phase 8 delivered what `gxc` was for: a Galexy-owned compiler that
emits an ELF the loader runs (`hello-gxc`, `test-hellogxc`). The
language does **not** grow from here. Upstream `rustc` on Galexy is the
self-host path (`RUSTC.md`, Phase 11); growing a subset compiler toward
Rust would compete with that port and never converge. What `gxc` is
now:

- A **fixture**: the second, independent producer of loader-conformant
  ELFs. Bug fixes only; `test-hellogxc` stays green.
- The **first client of the linker** (`LINKER.md`, Milestone 69 ✅):
  `gxc::elf` is gone; `gxc` emits a relocatable object (`gxc build
  -c`, `gxc::compile_object`) and `gxc build` links it through the
  `gxld` library. That was the only change it receives again.
- Milestone 62 (on-OS `gxc`) is **superseded**; a ring-3 compile is
  `rustc` (Milestone 72–73), not `gxc`.

| Layer | Mechanism | Answers |
| --- | --- | --- |
| Source | Tiny Rust subset (“gxr”) | What syntax is accepted? |
| Frontend | Lex → parse → check (host tests first) | Is the program well-formed? |
| Codegen | Hand x64 or Cranelift | What machine code? |
| Object | Static ELF64 @ `USER_IMAGE_BASE` | Can the Galexy loader map it? |
| Runtime | Syscall stubs matching `galexy-rt` | `write` + `exit` for hello |

Checkboxes: `TODO.md` Phase 8 — Milestones **59–61** ✅ (**62**
superseded by Phase 11). Style: `docs/STYLE.md` → Compiler. ABI and
loader contracts stay in `galexy-abi` / `DESIGN.md`.

## Success for v1

One source file, compiled by **our** tool on the host, runs under QEMU
and prints a hello line through the console Cap — same path as today's
`crates/userspace/hello`.

```text
gxc hello.gxr -o hello.elf
# → static ET_EXEC, linked at USER_IMAGE_BASE, W^X PT_LOADs
# → ramdisk / spawn / shell launch prints the line; exits 0
```

The host `rustc`-built `hello` remains the golden behavioral reference.
Byte-identical output is **not** required.

## Reuse first (do not rebuild the world)

### Already in this repo (use as-is)

| Asset | Why it speeds hello |
| --- | --- |
| `galexy-abi` | Syscall numbers, Cap words, `USER_IMAGE_BASE` |
| `galexy-rt` | `_start` / `write_console` / `exit` register contract |
| `sched/loader.rs` | Maps static non-PIE ELF; W^X; enters `e_entry` |
| `crates/userspace/hello` | Minimal source + QEMU tests (`test-realprogram`, typing E2E) |
| Ramdisk packing | Drop a new ELF next to `hello` without new kernel policy |

The compiler's job for v1 is **codegen + ELF emit** into a shape that
loader and runtime already understand. Do not invent a second ABI.

### External work to study / borrow (license-aware)

| Project | What to take | What not to take |
| --- | --- | --- |
| [Cranelift](https://github.com/bytecodealliance/wasmtime/tree/main/cranelift) | Host-side x64 codegen without LLVM; later on-OS if `no_std` path is solid | Full Wasmtime / WASI stack |
| [rustc-lite](https://github.com/suhteevah/rustc-lite) (ClaudioOS) | Shape of a tiny Cranelift-backed subset compiler; MIT/Apache-2.0 | Whole frontend as a black box — evaluate, then **vendor or reimplement** the slices we need with attribution |
| [`object`](https://crates.io/crates/object) + [`iced-x86`](https://crates.io/crates/iced-x86) | ELF64 emit / instruction encode without binutils (pattern used by hobby compilers such as NCC-Rust) | PE/Mach-O backends we do not need |
| Upstream Cranelift `no_std` work | ~~Future on-OS compile~~ — superseded; Cranelift arrives as `rustc`'s backend (`RUSTC.md`) | Blocking v1 on host |

### Explicitly out of scope for v1

| Project | Why not now |
| --- | --- |
| Full `rustc` / LLVM | Orders of magnitude too large *for this phase*; we already use host rustc for real programs. Running upstream `rustc` (Cranelift backend, no LLVM) *on* Galexy is its own plan: `RUSTC.md` (Phase 11) |
| [mrustc](https://github.com/thepowersgang/mrustc) | Bootstraps *full* rustc via C; wrong size for “hello on Galexy” |
| `rustc_codegen_cranelift` as our product | Still needs full rustc frontend; useful later as a *host* build accelerator, not the Galexy compiler |

Prefer **Apache-2.0 / MIT** dependencies. Any vendored slice gets a
`THIRD_PARTY` note and stays behind a clear crate boundary
(`crates/gxc/` or similar).

## Language slice (gxr v0) — frozen (Milestone 59)

Enough to express today's hello — nothing more until that works.
Canonical example: `crates/gxc/examples/hello.gxr`.

```rust
// hello.gxr — frozen gxr v0
#![no_std]
#![no_main]
fn main() -> i32 {
    write_console(b"Hello from gxc!\n");
    0
}
```

| Allowed | Notes |
| --- | --- |
| `#![no_std]` / `#![no_main]` | Optional; ignored for codegen (entry injected) |
| `fn main() -> i32 { ... }` | Exactly one function; name must be `main` |
| `write_console(b"...");` | Only prelude call; non-empty byte-string ≤ 4096 |
| Byte-string escapes | `\n` `\t` `\r` `\\` `\"` `\0` |
| Trailing `i32` literal | Return value (optional `;` before `}`); default `0` if omitted… actually required path returns explicit or defaults to 0 when body ends at `}` with only statements — today parser defaults `ret` to `0` |
| `//` line comments | Yes |
| Host CLI | `gxc check <file.gxr>` |

| Rejected in v0 | |
| --- | --- |
| Other `fn` items, `let`, control flow, types besides `i32` | |
| `std`, traits, generics, macros, `unsafe`, floats, `alloc` | |
| Unknown idents / attrs / block comments | |
| Linking `galexy-rt` as an rlib | Prelude inlined at Milestone 60 |

**Codegen (M60 ✅):** hand-written x86_64 (`gxc::CODEGEN_BACKEND_PLAN =
"hand-x64"`). `gxc build` emits static ELF64 (R rodata + RX text) at
`USER_IMAGE_BASE`. Cranelift deferred unless the subset grows.

Claim carefully in docs: **“Rust subset for Galexy”**, not “Rust
compatible.”

## Pipeline

```text
.gxr source
  → lex / parse / name-resolve (host unit tests)
  → typed AST (i32, &[u8] byte strings, fn)
  → IR (custom tiny IR *or* Cranelift CLIF)
  → machine code (x86_64 SysV) with R_X86_64_64 string relocations
  → ELF64 ET_REL (.text, .rodata, .rela.text)      ← `gxc build -c`
  → gxld → ET_EXEC @ USER_IMAGE_BASE (R, RX; no W|X) ← `gxc build`
  → ramdisk / galfs → spawn
```

**Host-only:** `gxc` is a Linux host binary in the workspace and stays
one. On-OS compilation is upstream `rustc` (`RUSTC.md`). Since
Milestone 69 the last two pipeline steps are "ELF64 relocatable object
→ `gxld` → `ET_EXEC`"; the layout rules live in the linker only.

## Runtime contract (must match loader)

Programs `gxc` emits must satisfy what `sched/loader.rs` already
enforces:

1. ELF64, static, non-PIE, image linked at `galexy_abi::USER_IMAGE_BASE`
2. Every `PT_LOAD` is W^X (no writable+executable segment)
3. Entry is `_start(arg: *const u8, arg_len: usize) -> !` SysV, or a
   wrapper that ignores args and calls `main` then `exit`
4. Console output: `syscall` Write with console Cap bits from
   `galexy_abi::reserved::console(WRITE)`
5. Termination: `syscall` Exit with status in the first arg

Golden test: behavior matches `crates/userspace/hello` under the existing
QEMU suite (serial line + exit 0). Prefer a **second** ramdisk name
(`hello-gxc`) so rustc-built `hello` never regresses.

## Milestone map

| Milestone | Delivers |
| --- | --- |
| **59** ✅ | Language slice frozen; `gxc` crate; lex/parse/check + host tests |
| **60** ✅ | Codegen + ELF emit at `USER_IMAGE_BASE`; prelude syscalls |
| **61** ✅ | `hello.gxr` → ramdisk `hello-gxc` + `test-hellogxc` in QEMU |
| **62** *superseded* | ~~Port `gxc` to ring-3~~ — on-OS compile is `rustc` (Phase 11); `gxc` frozen |
| **69** ✅ (Phase 11) | `gxc` emits a relocatable object; `gxld` links it (`LINKER.md`) |

## Suggested crate layout (when coding starts)

```text
crates/
  gxc/              # host binary + lib (parse/codegen/obj → gxld)
  gxc-prelude/      # tiny asm/Rust blobs for write/exit (optional)
docs/COMPILER.md    # this plan
```

Keep `gxc` out of the kernel. The kernel only loads ELFs it already
trusts (ramdisk / later signed measure — Milestone 51).

## Risks

| Risk | Mitigation |
| --- | --- |
| Subset creep (“just one more Rust feature”) | Hello is the gate; new syntax needs a milestone checkbox |
| Cranelift weight on-OS | Host Cranelift first; hand x64 for the 20-instruction hello if deps hurt |
| ELF/linker subtleties | Reuse loader tests; emit minimal two-segment images |
| License / provenance | Prefer MIT/Apache; document vendored files |
| Confusion with host rustc programs | Separate binary name + COMPILER.md “subset” banner |

## Explicit non-goals (v1)

- Full Rust / edition parity / `cargo` clone
- Compiling `shell`, `galexy-rt`, or the kernel with `gxc`
- LLVM, GCC, or mrustc in the Galexy tree
- JIT inside the kernel
- Cross-compiling to non-x86_64
- Claiming rustc compatibility

## Related docs

| Doc | Role |
| --- | --- |
| `DESIGN.md` | Loader, syscall register contract, crate DAG |
| `PROCESS.md` | Process Caps (compiler does not change spawn policy) |
| `SCHEDULING.md` | Runtime scheduling (orthogonal; parallel track) |
| `STYLE.md` | Coding rules for `gxc` when it lands |
| `ROADMAP.md` Phase 8 | Direction-level bullets |
| `TODO.md` 59–62 | Checkboxes (62 superseded) |
| `LINKER.md` | `gxld`, the static linker `gxc` will emit objects for |
| `RUSTC.md` | Upstream `rustc` on Galexy — the self-host path |
