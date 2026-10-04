# DESIGN — galexy.os

How the pieces fit. This file explains the *why*; `STYLE.md` explains the
*how it's written*.

## Layout philosophy

galexy.os is organized by **layer**, not by POSIX convention (`usr/`, `dev/`,
`etc/` and friends are rejected). There are exactly four kernel layers plus
the kernel's wiring file:

```
crates/
├── galexy-os/           # the kernel: lib (shared code) + bins (entries)
│   └── src/
│       ├── lib.rs       # shared init, panic handler, QEMU exit plumbing
│       ├── main.rs      # normal kernel: wiring only (init order + main loop)
│       ├── bin/         # test kernels: one bin per QEMU integration test
│       ├── shell.rs     # the "shell": consumes driver input, produces screen output
│       ├── arch/        # THE PORT WALL: x86_64 hardware code lives only here
│       ├── drivers/     # device drivers (screen, serial, keyboard, ...)
│       └── sched/       # scheduler (planned; hook point documented in code)
├── galexy-core/         # kernel primitives (alloc-free, host-testable; lifted
│                        #   from the former kcore module when tests arrived)
├── userspace/           # ring-3 programs later, one crate per program (planned)
└── runner/              # host tooling: builds BIOS+UEFI images, launches QEMU,
                         #   hosts the boot tests (tests/boot.rs)
```

## Boundary rules (enforced by structure, checked in review)

1. **`main.rs` is a wiring file.** Logic never accumulates there — it moves
   into the owning layer module.
2. **`arch/` is the port wall.** Only `arch/` touches I/O ports, CPU control
   registers, MSRs, or platform specifics. Drivers and primitives call
   `arch` APIs. Porting to another arch = rewriting `arch/` alone.
3. **Drivers never call drivers.** Shared behavior goes through `galexy-core`
   types/traits; shared *policies* stay in the caller (`shell`).
4. **`galexy-core` stays alloc-free** and platform-independent (no `arch`
   deps) — it's the bottom of the dependency stack:
   `bins → lib → (shell, banner, sched, drivers)`,
   `drivers → arch, galexy-core`, `sched → arch, galexy-core`,
   `galexy-core → (nothing)`.
5. **Crate-lift policy.** A module becomes its own workspace crate only when
   it gains a *second consumer* (e.g. `kcore` → `galexy-core` when the test
   harness needed host-testable primitives; a driver splits out when
   userspace visibility is needed). Lift stable boundaries only — never "to
   make it look organized".
6. **Userspace programs are always their own crates** under
   `crates/userspace/` — never modules of the kernel. Until a loader exists,
   first user programs are hand-assembled flat blobs embedded in the kernel;
   the contract for `crates/userspace/` (one crate per program, linked
   against a small `galexy-rt` runtime, loaded by the kernel) applies from
   the first real program onward.
7. **Syscall layering** (user space, planned): the syscall *mechanism*
   (MSR setup, naked entry, frame building) lives in `arch/`; the dispatch
   table (which syscall does what) lives in `sched/syscalls.rs` — it is
   scheduler-adjacent policy, not hardware.

## Boot flow

```
BIOS/UEFI
  └─> bootloader (bootloader crate v0.11, built by runner/build.rs)
        └─> galexy-os `kernel_main(BootInfo)`
```

- The kernel is compiled as a freestanding ELF for the prebuilt
  `x86_64-unknown-none` target (no custom target JSON needed).
- `runner` is a host-side crate: its `build.rs` uses artifact dependencies
  (`bindeps`) to build the kernel, then `bootloader::BiosBoot`/`UefiBoot` to
  produce `galexy-os-bios.img` / `galexy-os-uefi.img`. Its `main` boots the
  chosen image in QEMU; `--uefi` selects UEFI + OVMF.
- `BootInfo` carries: physical memory map, framebuffer (mapped into our
  address space), RSDP address, etc. `physical_memory_offset` is currently
  `None` — enable `map_physical_memory` in the `BootConfig` when memory work
  starts.

## Module contracts

### screen — "the screen" (`drivers/`)

Bootloader v0.11 provides a **pixel framebuffer** (1280x720 BGR in QEMU); the
legacy VGA text mode is gone, so glyphs are rendered with
`noto-sans-mono-bitmap` (16px regular weight, anti-aliasing preserved by
scaling the foreground color by glyph intensity).

Public surface stays terminal-shaped:

```rust
pub fn out_char(c: char)
pub fn out_str(s: &str)
pub fn clear_screen()
pub fn set_color(color: Color)   // foreground; background is always black
pub fn backspace()               // erase last char of the current line
```

State (cursor, color, framebuffer snapshot) lives behind a single
`spin::Mutex` global; scrolling copies the buffer up by one line height.
Swap-in candidates later: text-mode cursor, tab stops, an ANSI-ish layer.

### serial — "the side channel" (`drivers/`)

`uart_16550` at COM1. Used for panics, boot info, and debug output. **Rule:**
nothing user-facing ever prints here; it's invisible to the OS user by
design. The serial writer shares no lock with the screen, so interrupt
handlers can log through it safely.

### keyboard — "the input decoder" (`drivers/`)

IRQ1 handler → `pc_keyboard` (US layout, scancode set 1) → Unicode chars
pushed into a `kcore::Ring`. Consumers drain via `keyboard::pop_key()`:

```rust
pub fn add_scancode(scancode: u8)   // called from the IRQ handler only
pub fn pop_key() -> Option<char>    // drains decoded input
```

Locks are tiny and never nested (decode under one lock, push under another),
so IRQ context is safe. Queue overflow drops the newest key (documented).

### arch — "the plumbing"

Init order: GDT/TSS → IDT → PICs → timer config → `sti`.

- GDT + TSS: IST slot 0 for double fault; **all segment registers (including
  `ss`, `ds`) are reloaded after `lgdt`** (bootloader migration warning).
- IDT: breakpoint, page fault (reports + parks), double fault (own IST
  stack), timer (IRQ0), keyboard (IRQ1).
- PICs remapped to vectors 32..47 via `pic8259`.
- Timer: PIT channel 0 at ~1 kHz; handler increments an `AtomicU64` and
  heartbeats over serial once per second. **Scheduler phase swaps this
  handler body, nothing else changes.**

### arch/mm — "physical memory" (arch/)

- Frame allocator over the `BootInfo` memory map: only `Usable` regions are
  allocatable, tracked in `.bss`-resident bitmap pair (`USED`/`USABLE`,
  fixed lock order USED→USABLE), 512 MiB coverage (beyond that: serial
  warning + frames stay unused).
- `allocate_frame()` is first-fit; `deallocate_frame` panics on double-free
  and on non-`Usable` frames — bootloader/kernel memory protected by
  construction.
- Access contract: `physical_memory_offset` (fixed `0x0000_4000_0000_0000`
  via `BOOTLOADER_CONFIG`) converts allocated-frame physical addresses to
  virtual ones. Allocated frames are otherwise unmapped → exclusive access.
- The recursive page table is mapped at the canonical P4-index-511 address
  (`(0xFFFF << 48) | (511 << 39)` — MUST be sign-extended AND 512-GiB
  aligned, or the bootloader panics at boot). Paging phase consumes this.

### arch/mm/paging — "virtual memory" (arch/)

- `OffsetPageTable` over the bootloader-created active tables: L4 table
  located via CR3 + the physical-memory offset; mapper lives behind
  `MAPPER: Mutex<Option<...>>` and is main-loop-only (IRQ handlers never
  touch it — recheck when preemption lands).
- `map_page(page, frame)` maps with PRESENT|WRITABLE|NO_EXECUTE and flushes
  the TLB; page-table frames come from the frame allocator via a trait
  adapter. `unmap_page` flushes and returns the frame. `translate(virt)` for
  lookups.
- Tests can swap the page-fault handler at runtime
  (`arch::set_page_fault_handler`) — demand paging will use the same seam.
- Fresh virtual space: the bootloader's dynamic mappings fill P4 indices
  from 0 upward, physical memory is fixed at index 32, recursive at 511 —
  test/scratch mappings should use a high-but-canonical index (e.g. 100,
  heap uses 43).

### arch/mm/heap — "the heap" (arch/)

- `linked_list_allocator::LockedHeap` as the `#[global_allocator`; 400 KiB
  at a fixed fresh virtual area (P4 entry 43), mapped by our mapper from
  frame-allocator frames during `mm::init` (memory bring-up is one call).
- `shell` is the first heap consumer (String line buffers); the scheduler's
  task queues are the planned next one. Host unit tests never touch the
  heap (no_std tests of `galexy-core` are allocation-free by rule).

### sched — "the scheduler" (`sched/`)

Two models, layered:

**Cooperative tasks** — round-robin over voluntarily-yielding state
machines (`fn(&mut TaskCtx) -> TaskStatus`), stepped from the main loop
via a heap-backed `VecDeque`. Only the main loop touches them.

**Preemptive threads** (`sched/context.rs`) — the timer handler is a NAKED
function: the CPU has pushed the IRQ frame; the naked asm pushes all GPRs,
calls the Rust scheduler with the frame pointer, and either swaps RSP to
the next thread's saved context (pops + `iretq` straight into it) or
returns 0 to resume the outgoing task. Each thread owns:
- a 32 KiB heap (`Box`/`vec!`) stack — the context block lives on it
- a leaked 16-aligned FXSAVE area (kernel code may auto-vectorize)
- main is participant slot 0 of the unified rotation.

**Lock audit rule (preemption)**: the ONLY preemptor is the timer IRQ, so
*any lock held by preemptable code must be held with interrupts off*
(`without_interrupts`). The gate lives in the module's public API — never
at call sites (a call-site gate gets forgotten by the next caller; this
caused a real wedge: `thread_stats` once took the sched table ungated).
Applied to: `screen::_print`, keyboard queue ops, the heap
(`InterruptSafeAlloc` GlobalAlloc adapter + stats accessors), the sched
table accessors, and `shell::render_status_bar` (gated as a whole). The
scheduler switch itself holds NO lock across the RSP swap. Locks the
timer handler never touches (e.g. the screen lock) may be held across
preemption safely.
Previously-missed `ltr`: the TSS must be loaded after `lgdt` or IST
dispatch reads a stale descriptor.

Demo tickers/threads (`sched/demo.rs`) prove interleaving on screen; the
banner reports live counts.

### banner — "the boot showcase"

- Runs after full init; every `[ok]` line reads live state from the owning
  subsystem (framebuffer layout from `screen`, free frames from `mm`,
  heap-start translation from the mapper, heap size from `heap`). Adding a
  subsystem = adding a line here.

### shell — "the shell" (`shell.rs`)

Main loop: drains the key queue — printable chars echo + buffer up; Enter
dispatches (`help`, `stats`, `tasks`, `threads`, `clear`, `about`;
unknown lines echo back — the original echo-shell behavior); Backspace
erases. The status bar (`render_status_bar`) redraws the bottom line
in-place once per second (uptime + per-thread tick counts + frames free)
with cursor save/restore — the "quiet OS" demo: everything observable as
live numbers, zero background noise.

## Concurrency model (pre-scheduler, single-core)

- All shared state sits behind `spin::Mutex` (plus `LazyLock` for init-once
  statics and `AtomicU64` for tick counts).
- Print lock: `println!` → screen lock. Current mitigation for the
  hold-lock-while-interrupted hazard: interrupt handlers never touch the
  screen lock (they use serial or atomics only). A "lock contention audit"
  is scheduled before preemption lands.

## Testing strategy

Two tiers, chosen after studying the bootloader crate's own test suite:

- **Host unit tests** (`cargo test -p galexy-core`): pure, alloc-free logic
  (e.g. `Ring<T, N>`) — instant, no QEMU.
- **QEMU integration tests** (`cargo test -p runner`): each test is a small
  kernel *binary* under `crates/galexy-os/src/bin/` that boots the full
  stack, asserts, and exits via `exit_qemu` (`isa-debug-exit`, port 0xF4).
  `runner/build.rs` builds one disk image per kernel binary and exposes them
  via the `GALEXY_IMAGES` manifest; `runner/tests/boot.rs` boots each
  headless and asserts exit codes + serial markers. Any panic in a test
  kernel becomes a Failed exit automatically (Success instead when
  `expect_panic` was registered).
- Headless interactive verification (`scripts/boot-test.sh`): injects HMP
  commands (`sendkey`, `screendump`), captures COM1 — for manual checks.

QEMU exit-code mapping (empirically verified): `Success` (0x10) → exit 33,
`Failed` (0x11) → exit 35.

## Known sharp edges

- `bootloader` 0.11's builder API differs entirely from 0.9's `bootimage`;
  pin exactly in `Cargo.toml`.
- UEFI boot: legacy PIC doesn't exist → timer/keyboard need APIC work before
  they work there.
- `-no-reboot` is always passed to QEMU so triple faults surface as an exit
  instead of an infinite reboot loop.
- Fresh artifacts can live in *multiple* `OUT_DIR` hash dirs; pick images by
  mtime (`ls -t`) when testing manually.
