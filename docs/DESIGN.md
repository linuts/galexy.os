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
│       ├── drivers/     # device drivers (screen, serial, keyboard, ata, ...)
│       └── sched/       # scheduler + syscall dispatch table (policy layer)
├── galexy-abi/          # THE SYSCALL ABI: numbers, capability model, error
│                        #   codes. The ONLY kernel<->userspace shared surface.
│                        #   no_std, zero deps, host-testable. Frozen before any
│                        #   ring-3 code existed (Milestone 12).
├── galexy-core/         # kernel primitives (alloc-free, host-testable; Ring,
│                        #   Bitmap)
├── userspace/           # ring-3 programs, one package per program
│   ├── galexy-rt/       #   the runtime: entry!, syscall wrappers, panic handler
│   └── hello/           #   the first real Rust user program
└── runner/              # host tooling: builds BIOS+UEFI images, launches QEMU,
                         #   hosts the boot tests (tests/boot.rs)
```

Dependency order: `galexy-core, galexy-abi → (nothing)`;
`galexy-os → {core, abi}`; `galexy-rt → abi`; user programs → `galexy-rt`.
Userspace programs NEVER link against the kernel — `galexy-abi` is the only
contract between them.

## Boundary rules (enforced by structure, checked in review)

1. **`main.rs` is a wiring file.** Logic never accumulates there — it moves
   into the owning layer module.
2. **`arch/` is the port wall.** Only `arch/` touches I/O ports, CPU control
   registers, MSRs, or platform specifics. Drivers and primitives call
   `arch` APIs. Porting to another arch = rewriting `arch/` alone.
3. **Drivers never call drivers.** Shared behavior goes through `galexy-core`
   types/traits; shared *policies* stay in the caller (`shell`). Exception
   (Milestone 16): `drivers/console.rs` is not a driver but the console
   POLICY façade (screen + serial in one place); it is the one sanctioned
   composition point.
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
   `crates/userspace/` — never modules of the kernel. Convention: one
   package dir per program (`dir == package == binary name`); each links
   `galexy-rt` (runtime) + `galexy-abi` (contract) — NEVER the kernel.
   The runner consumes program bins via artifact bindeps (same mechanism
   as kernel bins) and packs them into the tar ramdisk; the kernel loads
   them as static ET_EXEC ELFs (`sched/loader.rs`) at
   `galexy_abi::USER_IMAGE_BASE` — a FIXED virtual load address (per-task
   trees make everyone sharing the base safe). Loader strictness: every
   PT_LOAD must sit under the program's own P4 entry — a segment outside
   would map through kernel-SHARED subtree tables (pollution = rejection).
7. **Syscall layering**: the syscall *mechanism* (MSR setup, naked entry,
   frame building) lives in `arch/`; the dispatch table (which syscall does
   what) lives in `sched/syscalls.rs` — it is scheduler-adjacent policy,
   not hardware. The ABI itself lives in `galexy-abi` and never changes
   from inside either side without a version bump.
8. **ABI stability (capabilities day one).** The `galexy-abi` decisions —
   opaque `Cap` handles (48-bit index + 16-bit rights), reserved indexes
   (console=1, self=2; file caps start at 3, per task; keyboard=0x8000,
   loader=0x8001, stats=0x8002, tasks=0x8003, threads=0x8004,
   power=0x8005, files=0x8006 in the high reserved band, above any file slot),
   syscall numbers (exit=0, yield=1, write=2, cap_info=3, open=4, read=5,
   close=6, spawn=7, power=8, create=9, remove=10), error codes (BadCap=1, AccessDenied=2, BadBuffer=3,
   Unsupported=4, BadValue=5, NotFound=6, NoResource=7) — are permanent.
   New syscalls APPEND;
   renumbering/renaming = ABI major bump. NO file descriptors at this ABI
   level: resources are capabilities kernel-side, validated on every call,
   revoked easily. Files/ports/handles-to-come all become caps.

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
  address space), RSDP address, ramdisk, etc. `physical_memory_offset` is
  FIXED (`0x0000_4000_0000_0000` via `BOOTLOADER_CONFIG`) — the phys map
  covers the whole physical address space from boot (ACPI tables, LAPIC/IO
  APIC discovery, frame access all rely on it).

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

State (cursor, color, framebuffer snapshot, CSI parser) for the visible
TTY lives behind a single `spin::Mutex` global. Each of the twelve
consoles also keeps a cell grid (character and color) behind that same
lock. F1–F12 select which grid is painted; the others keep their cells.
Text uses every row except the last, which the status bar owns; scrolling
shifts only the text rows, by one line height. Tab stops are every 8
columns. CR returns to column 0. ESC introduces a fixed-size CSI parser
(no allocation): SGR colors 30–37 and 90–97 plus reset, cursor position
and movement (`H`/`f`/`A`–`D`), erase in display (`J` 0 and 2), and
erase in line (`K` 0 and 2). The parser state survives a split `write`
and is saved with the TTY. A blinking cursor is still future.

### serial — "the side channel" (`drivers/`)

`uart_16550` at COM1. Used for panics, boot info, and debug output. **Rule:**
nothing user-facing ever prints here; it's invisible to the OS user by
design. The serial writer shares no lock with the screen, so interrupt
handlers can log through it safely.

### keyboard — "the input decoder" (`drivers/`)

PS/2 controller bring-up lives HERE now (`keyboard::init`, called from
`arch::init`): the i8042 first-port enable (`0xAE` to port 0x64) + a
stale-output-buffer drain — firmware (SeaBIOS polled keyboard, OVMF alike)
may leave the port disabled or bytes pending, and a full buffer never
re-asserts the line (the first real keystroke would black-hole). This is
controller work, not interrupt-controller work — it runs on every boot path.

IRQ1 handler (LAPIC-delivered via the I/O APIC) → `pc_keyboard` (US layout,
scancode set 1). Unicode characters are pushed into the active TTY's
`kcore::Ring` (twelve rings, one per F-key). F1–F12 do not become input:
the handler stores the TTY index and the main loop paints that grid.
The in-kernel editor drains TTY 0 via `keyboard::pop_key()`. A ring-3
shell `read`s the keyboard capability (`reserved::KEYBOARD_INDEX`, READ)
and gets the queue of the TTY it was started on. A zero-length success
means that queue is empty, not that input ended. A short read that
cannot fit the next character's UTF-8 puts that character back
(`unget_key_tty`).

```rust
pub fn add_scancode(scancode: u8)   // called from the IRQ handler only
pub fn pop_key() -> Option<char>    // drains TTY 0
pub fn pop_key_tty(tty: u8) -> Option<char>
```

### ata — "the galfs disk" (`drivers/`)

PIO LBA28 on the primary IDE slave (drive index 1). The boot image is
the master and is never touched. `present()` probes once via IDENTIFY;
when the slave is absent every read/write returns `Unsupported` and
galfs stays RAM-only. `flush()` issues FLUSH CACHE after a committed
GALF slot write. The runner attaches a second raw image at
`if=ide,index=1` without a snapshot (`cargo run` and the persistence
tests) so writes survive across QEMU processes.

```rust
pub fn present() -> bool
pub fn read_sectors(lba: u32, dst: &mut [[u8; SECTOR]]) -> Result<(), SysError>
pub fn write_sectors(lba: u32, src: &[[u8; SECTOR]]) -> Result<(), SysError>
pub fn flush() -> Result<(), SysError>
```

The handler never takes the screen lock. Locks are tiny and never nested
(decode under one lock, push under another), so IRQ context is safe.
Queue overflow drops the newest key (documented).

### arch — "the plumbing"

Init order: GDT/TSS (per-CPU slot 0) → per-CPU GS substrate → ACPI (MADT)
→ LAPIC (+calibration) → I/O APIC (keyboard route) → legacy PICs (masked)
→ i8042 enable → AP boot (trampoline + per-AP init + timer arms) → `sti`.

- GDT + TSS: per-CPU slots SLOTS[c] (see arch/cpu + arch/gdt) — the same
  selector layout replicated on every CPU (STAR stays selector-indexed).
  IST slot 0 for double fault, per-CPU stacks. All segment registers
  reloaded per CPU at its own bring-up (bootloader migration warning).
- IDT: ONE shared table, loaded into EVERY CPU's IDTR (`lidt` is per-CPU;
  an AP without its own load runs on the real-mode zero IDTR and
  triple-faults on its first tick). Entries: breakpoint, page fault
  (reports + parks), double fault (per-CPU IST), timer (vector 32, NAKED),
  keyboard (vector 33), shootdown IPI (vector 0xF8, lock-free — M19),
  LAPIC spurious (0xFF).
- Interrupt delivery is ALL-APIC since Milestone 17, now PER-CPU (M18):
  each CPU's LAPIC timer carries vector 32 with a SHARE-SPLIT ICR
  (ticks-per-ms × cpu count) so N cores each tick at 1/N kHz and the
  machine-wide rate stays ~1 kHz; the I/O APIC routes the keyboard to the
  BSP's LAPIC (physical destination in the RTE); the legacy 8259s are
  remapped and fully masked.
- EOI: ONE true EOI — the LAPIC's EOI register (`apic::eoi`), written by
  each CPU's own switch path/handler (per-CPU by hardware: the MMIO
  address is per-CPU-redirected in xAPIC, the MSR interface is per-CPU in
  x2APIC). Edge-triggered I/O APIC lines need no IOAPIC-side EOI.
- Tick accounting: per-thread/per-CPU-main atomics + a shared TICKS
  counter whose seconds semantics survive the share split.

### arch/acpi — "interrupt controller discovery" (arch/)

- RSDP (physical addr from `BootInfo.rsdp_addr`) → XSDT (v2+, 8-byte child
  entries) or RSDT (v1, 4-byte) → first `APIC`-signature table = MADT.
  Every table read goes through the physical-memory mapping (which exists
  from boot, independent of `mm::init` order) and is CHECKSUM-VALIDATED
  before trust; malformed/missing data panics loudly (no guessing).
- Parsed + published (`arch::madt()`): LAPIC MMIO base (header field or
  type-5 override), the boot I/O APIC's MMIO base + GSI base (the record
  covering GSI 0), enabled processor count + BSP APIC ID, and the ISA
  Interrupt Source Overrides (ISA IRQ → GSI; QEMU overrides IRQ0→GSI2,
  leaves IRQ1 identity).
- Host-side the boot CPU is always LAPIC id 0 — the code never assumes it;
  the BSP id is read from the MADT; `enabled_ids()` feeds AP bring-up.
- The same walk looks up `FACP` (the FADT). A missing or unusable FADT
  is logged and shutdown stays unavailable; the MADT is still required.
  PM1a/PM1b control ports (the extended GAS when it is System I/O),
  the SMI command port, and the reset register (flag bit 10) are copied
  out. `_S5_` is the DSDT `Name(_S5_, Package …)` form only — not an AML
  interpreter. `arch::power` programs PM1 with that sleep type (then the
  PIIX4 port `0x604` if the machine is still up) and resets through the
  FADT register or the i8042 command `0xFE`.

### arch/cpu — "per-CPU identity + AP bring-up" (arch/)

- GS-base identity: each CPU WRGSBASEs its slot address (FSGSBASE feature
  asserted AND CR4.FSGSBASE enabled — the CPUID bit alone does not grant
  the instructions). Fixed offset contract for naked asm: gs:[0] self ptr,
  gs:[8] SYSCALL kernel-stack target, gs:[16]/[24] entry scratch.
  Userland never touches GS → no swapgs discipline anywhere.
- Per-CPU GDT/TSS slots (arch/gdt): selectors are REPLICATED identically
  (layout-asserted) so STAR/iret constants stay valid machine-wide; each
  TSS embeds that CPU's own RSP0 + double-fault IST stack.
- AP boot: position-independent trampoline at phys 0x8000 (16→32→64 with
  push/retf walks; EFER.NXE must ride along with LME or the shared kernel
  half's NX pages #PF as reserved-bit violations); INIT → PIT-timed gap →
  SIPI ×2; APs run their own GDT/IDTR/GS/syscall-MSR/LAPIC bring-up and
  write an online magic the BSP waits on.
- Per-CPU SYSCALL hardware: STAR/LSTAR/SFMASK/EFER.SCE are programmed on
  EVERY CPU (per-CPU MSRs).

### arch/apic — "the LAPIC" (arch/)

- Dual interface: xAPIC (MMIO register page at the MADT base — mapped
  PRESENT|RW|NX|uncached at a fixed kernel-half P4 entry, 200; the MMIO
  address is PER-CPU-redirected by hardware, so shared mapping serves all
  CPUs) and x2APIC (MSRs `0x800 + offset >> 4`, inherently per-CPU). Mode
  DETECTED from `IA32_APIC_BASE` bit 10; every register access funnels
  through one read/write pair so both paths share all logic.
- Bring-up (`bring_up`): spurious vector 0xFF (with an IDT gate installed
  — an unhandled stray spurious would triple-fault), TPR 0, flat DFR/LDR,
  LVT entries masked — each CPU runs it during ITS bring-up.
- LAPIC timer = THE timer, PER CPU (M18): calibrated ONCE on the BSP
  against a PIT channel-2 one-shot (~10 ms window, ratio math only — no
  wall-clock assumptions, TCG safe); EVERY CPU arms its own (`arm_timer`)
  PERIODIC on vector 32 with the SHARE-SPLIT ICR (ticks-per-ms × online
  count) — N cores × 1/N kHz keeps the machine-wide tick rate ~1 kHz.

### arch/ioapic — "external interrupt routing" (arch/)

- MMIO register page mapped at fixed kernel-half P4 entry 201 (the LAPIC
  page's sibling); IOREGSEL/IOWIN pair indexes the register space.
- Bring-up (BSP-only, once): version sanity (I/O APICVER ≥ 0x11), ALL
  redirection entries masked first (inherited state is firmware's), then
  exactly ONE wiring: the keyboard — ISA IRQ1 → GSI (MADT override or
  identity) → RTE pin, vector 33, edge-triggered, active-high, physical
  destination = the BSP's LAPIC ID. Masked-by-default is the rule: mask
  what you don't use.

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
  located via CR3 + the physical-memory offset. **Every page op
  (map/unmap/translate) runs IRQ-gated in the API** — the lock-audit rule
  made page ops callable from any kernel context, including IRQs.
- `map_page(page, frame)` maps with PRESENT|WRITABLE|NO_EXECUTE and flushes
  the TLB; page-table frames come from the frame allocator via a trait
  adapter. `unmap_page` flushes and returns the frame. `translate(virt)`
  for lookups.
- `FreshL4` — per-task address spaces (Step B): allocates a frame and
  clones the kernel root cached at init, then re-points the recursive
  entry (P4 511) at the FRESH frame — a verbatim copy would leave the
  recursive mapping addressing the kernel tree once the fresh table is
  loaded into CR3 (kernel higher-half entries are shared frames either
  way). The copy does not follow CR3, so a child cannot inherit another
  task's user mappings. The loader still runs on the main loop: it
  allocates, and a syscall runs with interrupts off. Tests can map into
  a fresh (non-active, coherent) tree via the `unsafe with_table()`
  mapper (no TLB flush — no CPU can address it).
- `install_cr3(frame)` — no-op when already active (Redox pattern: a swap
  costs a full TLB flush); `kernel_cr3()` = the boot table, cached at init.
- `free_user_tree(root, p4_index)` — reclaims a tombstoned task's WHOLE
  subtree under its own P4 entry: P3/P2/P1 frames AND data frames (the
  kernel's shared subtrees live under other entries — never touched).
- Fresh virtual space: the bootloader's dynamic mappings fill P4 indices
  from 0 upward, physical memory is fixed at index 32, recursive at 511 —
  test/scratch mappings use a high-but-canonical index (heap 43, tests
  100, task regions head above that).
- Tests can swap the page-fault handler at runtime
  (`arch::set_page_fault_handler`) — demand paging will use the same seam.

### arch/mm/heap — "the heap" (arch/)

- `linked_list_allocator::LockedHeap` as the `#[global_allocator`,
  IRQ-gated `InterruptSafeAlloc` adapter. Starts at 400 KiB in a fixed
  virtual area (P4 entry 43) and **grows on demand**: a failed `alloc`
  maps a 64 KiB chunk right past the current end (frames from the frame
  allocator) and `Heap::extend`s the allocator (the whole P4 entry spans
  512 GiB, so the growth path needs no new top-level structures).
- Growth is machine-serialized by a `GROWING` flag (SMP M19). The map
  loop runs IRQ-gated; the TLB shootdown between map and `Heap::extend`
  is lock-free (no Rust spin lock across the broadcast — see shootdown
  below). A second CPU that OOMs mid-growth does not fail the alloc: it
  spins with interrupts enabled until the in-flight chunk lands (a ~2s
  watchdog panics if the grower sticks), then the caller retries.
- `shell` is the first heap consumer (String line buffers); the scheduler's
  task queues are the next one. Host unit tests never touch the heap
  (no_std tests of `galexy-core` are allocation-free by rule).

### arch/mm/shootdown — "precise INVLPG" (arch/)

Kernel-half PTEs are shared across every CPU and every task tree. A remap
on one CPU is stale in every other TLB until invalidated.

- Mailbox pool: 8 slots × 16 VAs (one heap-growth chunk fits one slot).
  The ICR only carries the vector (0xF8); the VAs ride in the slot. A
  monotonic machine-global `seq` is published with Release; a slot is
  reused only after every target's per-slot `seen` has consumed that
  `seq` (ABA closed; a stale re-INVLPG is harmless).
- Initiator (`shootdown_others`): claim a slot, publish, `send_fixed_ipi`
  to every other online CPU, spin until each target's `seen` catches
  `seq`. Holds no Rust spin lock. Any IRQ state is fine — targets ack
  the next time they run with IF=1.
- Target: the 0xF8 handler scans unseen seqs, `invlpg`s the listed VAs,
  stores `seen`. Takes no locks, ever (an IF=0 lock holder must still be
  able to ack, or a broadcasting initiator spins forever).
- `map_kernel_page_broadcast` is the single-page primitive: map, local
  flush, then broadcast. Heap `grow()` batches instead: `map_page` the
  chunk under the IRQ gate, one `shootdown_others` for every new VA (a
  16-page chunk fills one mailbox slot), then `Heap::extend`. Per-task
  trees stay local: only that CPU's CR3 swap (a full flush) publishes
  them.

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
- a leaked-at-spawn FXSAVE area (kernel code may auto-vectorize), freed by
  the reaper on exit
- a pin: the CPU whose rotation runs it (round-robin at spawn; SMP M18)
- its CPU's main is participant slot 0 of THAT CPU's rotation.

**Pinned rotation (SMP M18).** Each CPU's `CPU_SCHED[c]` holds its own
current-slot/cursor/main-saved-context + FXSAVE area; the scans skip
foreign-owned slots like tombstones. `THREADS` remains THE cross-CPU lock
(spawn, reap, stats all serialize on it; switches touch nothing global
but it).

**Idle-pass work stealing (SMP M19).** When a CPU's scan is about to
serve main AND it owns no runnable thread (serving main every other tick
while threads exist is not idle), it may flip one foreign RUNNING slot's
owner to itself — under `THREADS`, and only if that slot is not current
on its owner and its saved context is **stable**. Stability is a per-slot
flag the victim's naked tail sets only after `mov rsp` leaves the thread
stack (`gs:[40]` carries the slot). A fresh thread starts unstable, so
its first run stays on the spawn CPU. `current != slot` is stored earlier,
when the lock drops, so it is not proof the tail has finished — a
host-starved victim vCPU can still be on that stack a guest tick later.
Entry on the stealer stays deferred to its next scan. That scan rides the saved
context; per-CPU switch-in (gs:[8], TSS.RSP0, CR3) is CPU-agnostic.
`stolen_at` holds the thread ~100 ticks so idle CPUs do not ping-pong it.
One steal attempt per idle pass.

**Lifecycle (tombstones).** Slots are NEVER removed from the thread vec:
the per-CPU `current`/`last_served` cursor indexes into it mid-switch, so
shifting entries would corrupt in-flight state. A thread whose entry
RETURNS tombstones itself (`thread_exit`, called by the trampoline); the
owner's rotation scans forward past dead or foreign slots (bounded — main
is always eligible at slot 0); the OWNER's `sched::reap()` frees stack +
fx of every exited thread it owns and checks the
stack canary (a deep overflow walks downward through the magic word at the
stack's very bottom first — reaping turns silent heap corruption into a
loud panic). A `Freed` slot is handed out again when no CPU's `current`
is that slot and `CTX_STABLE` says the switch-out tail has left the old
stack. The index stays put. The name is a 64-byte buffer on the thread,
copied at spawn. THE ZOMBIE RULE: after `thread_exit`, the park
loop MUST stay interrupts-ENABLED — with IF=0 the dead thread sleeps in
`hlt` forever, nothing ever preempts it, and the whole machine wedges
(found by `bin/test-threadexit.rs`).

**User tasks (Step B, now on any CPU — M18).** Same rotation, same
lifecycle — private address space; the pin is the spawn's round-robin
over the enabled CPUs (a ring-3 task can run and be reaped entirely on an
AP: per-CPU TSS.RSP0, per-CPU kstack slot gs:[8], per-CPU STAR/LSTAR). `spawn_user_task` (kernel tree asserted) builds a `FreshL4` and maps
the task's world INTO ITS OWN TREE via `with_table`: code page at a scanned
top-free P4 entry (`< 256` — 512 GiB per task), user stack (4 pages,
RW\|NX\|USER) at +1 GiB, an RW scratch page right above. All kernel-side
staging (blob bytes, zeroing, the initial ring-3 frame) goes through the
BACKING FRAMES (`frame_virt`) — the phys map is present in every tree, so
the task tree never needs to be active to write it. `Thread.cr3` swaps in
both switch paths (timer + syscall handoff), no-op when unchanged. The
task's own kernel-mode stack (heap Vec) serves its ring 3→0 crossings via
TSS.RSP0. Reaping = `free_user_tree(root, p4_index)`: one walk takes tables
AND data frames; the accounting closes exactly (proven by
`bin/test-treechurn.rs`). CRASH ISOLATION: one unmapped guard page below
the stack; the page-fault vector runs a NAKED handler (timer-shaped
prologue + error-code word ⇒ raw-offset reads, never resumed): ring-3
faults tombstone + rotate, ring-0 faults report + park.

**Ring-3 readiness** (structure only until userland): GDT carries DPL-3
user code/data segments, appended consecutively (`user SS = user CS + 8`,
the SYSRET quirk); `arch::set_tss_rsp0`/`tss_rsp0` update/read the live
TSS (UnsafeCell holder — the CPU reads it through the descriptor while
Rust updates RSP0, single-core + IRQ-gated); `Context::cpl()` decodes the
frame's privilege — the frame SHAPE is identical for both rings (iret
semantics push the same 5 words; ring 3→0 crossings differ only in WHERE
the CPU puts the frame: TSS.RSP0).

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

### sched/syscalls — "the spice must flow from a table" (sched/)

The dispatch table: number → behavior. Mechanism (MSR/STAR/LSTAR/naked
entry) is `arch/` business (rule 7); this file is policy. The table
consumes `galexy-abi` constants — numbers, `Cap` layout, error codes are
ABI-stabilized there (rule 8). Register contract: args in the frame
(RDI/RSI/RDX), result stamped back (RAX = value, RDX = 1 ok / 0 err).

Live behaviors: `exit` (tombstone + handoff — the reaper frees the task's
user stack pages, scratch, code page + kernel stack), `yield` (real
rotation switch via `sched::syscall_handoff`), `write` (cap authority:
console index + WRITE right; 1 KiB cap; page-walk validation of the user
buffer via `translate_active` — the CR3-ACTIVE tree, since per-task
address spaces the kernel-rooted `translate` cannot see user buffers;
printable-ASCII staging; screen output **+ serial mirror** — console =
screen + COM1, which is what makes userland output observable headless);
`cap_info` echoes handles (dispatch proving ground). `open`/`read`/`close`
are ramdisk files as capabilities (Milestone 20): each user task has a
fixed table of 8 opens (no allocation on the IF=0 syscall path), indexes
from `FILE_CAP_BASE` (3), authoritative grant intersected with the
handle snapshot. `read` copies the next bytes of an open file
(short-read at 1 KiB, 0 at EOF);
`close` drops the slot. `create(name, len, flags)` (syscall 9) puts a
path in a fixed galfs table (128 objects, 32 actors with one root each,
64-byte component names, shared 256×512-byte block pool with 8 direct
pointers per file / 4 KiB max, no heap on the syscall path — Milestone
45 / GALF v10). A path ending in `/` is a directory and returns 0. A file returns
READ|WRITE. `RDX == 1` empties an existing file; any other value creates
only when the name is new. Uniqueness is the parent plus the component.
The first path component may be `owner@name` (`dan@Desktop`); without an
owner the walk starts at the task's actor root. A token on the task
(object id + rights) must cover the target: create needs CREATE on the
parent, open needs READ, write needs WRITE, remove needs REMOVE, and
the files snapshot needs LIST. Holding a token on a parent covers the
children it permits. `open` of a path with no slash and no `@` still
hits the ramdisk first and grants READ. Any other path walks galfs and
must name a file (READ|WRITE). A directory open is `Unsupported`. A
path with no covering token is `AccessDenied`. `write` on a galfs
cap appends; the read cursor stays at the start. A ramdisk name at
`/`, or a name that already exists, is `Unsupported`. A missing parent
is `NotFound`. A full object table is `NoResource`. `close` drops the
task's cap; the bytes stay until `remove`. `write` on an
archive open is `Unsupported`. `remove(name, len)` (syscall 10) deletes
a galfs file or an empty directory and frees the slot. A ramdisk
name, or a directory that still has a child, is `Unsupported`. A
missing path is `NotFound`. An open cap on a removed file becomes
`BadCap`. `grant(path, len, rights, task, task_len)` (syscall 11)
installs a galfs token on a live user task: `RDX` is a mask of
`TOKEN_READ`/`WRITE`/`LIST`/`CREATE`/`REMOVE` (any other bit is
`BadValue`), `R8`/`R9` name the target. The caller must already hold
every right being granted on the resolved object (or an ancestor). A
missing path or task is `NotFound`. A full token table on the target
is `NoResource`. Same-object grants merge rights. The shell's
`grant lr <path> <task>` uses it. `revoke` (syscall 12) uses the same
registers and clears those rights from the target's token that names
the object exactly; a zeroed slot is freed. `share` / `unshare`
(syscalls 21–22) use that layout but target an **actor name**: the
durable home share is stored in the GALF image and re-applied at that
actor's next login (`install_session`). The caller must hold every
right being shared. Live cards use `grant`; durable cards use `share`
(shell `share` / `unshare`). `pipe(addr)` (syscall 13) writes a
READ cap and a WRITE cap into a 16-byte user buffer for an anonymous
pipe (8 pipes × 256-byte rings). `give(cap, task, len)` (syscall 14)
moves an open file or pipe end to another live user task and returns
the target's new Cap bits. `seek(cap, offset, whence)` (syscall 15)
sets the **read** cursor on an archive or galfs open (`SEEK_SET` /
`SEEK_CUR` / `SEEK_END`); a pipe is `Unsupported`. galfs `write` stays
**append-only** (seek does not move the write point). `cp` and `mv` are
ramdisk programs; `mv` prefers `rename` (syscall 17) and falls back to
copy+remove. `user(addr, len, op)` (syscall 16) manages actors:
`USER_WHOAMI` / `USER_USERS` write names into a buffer; `USER_ADD`
creates an actor plus empty Desktop; `USER_DEL` removes an empty actor
(never `admin`, never one a live task still uses); `USER_SU` replaces
the caller's tokens with ALL on the target (so a switched seat cannot
keep writing the previous actor's tree). Add/del require the caller's
root to be `admin`. A seat born as admin may `su admin` to return.
`rename(old, new)` (syscall 17) moves a galfs dirent (REMOVE on source,
CREATE on dest parent; no byte copy). `truncate(cap, size)` (syscall 18)
sets a galfs file length (WRITE; shrink frees blocks, grow zero-fills).
`stat(path, buf)` (syscall 19) writes a [`STAT_LEN`] record (kind, size,
owner, held token rights; needs LIST). Directory listing remains the
`FILES` snapshot — `open` on a directory is `Unsupported`. The shell
exposes `whoami`, `users`, `useradd`, `userdel`, `su`, `truncate`, and
`stat` (and resets cwd on `su`). Boot formats one immortal actor,
`admin`, with Desktop. When the primary IDE slave is present,
`galfs::init` loads the newest valid GALF **v10** sealed slot (dual
288-sector images: wrapped volume key + ChaCha20-HMAC payload of
actors/objects/shares/bitmap/blocks + per-actor quotas, generation +
ciphertext CRC + structural checks) or formats that admin tree under a
fresh volume key; create/remove/append/rename/truncate/useradd/userdel
/share/unshare sync to the inactive slot and flush the cache; `sync()`
(syscall 20) is an explicit barrier (shell `sync`). Each actor has
durable `max_objects` / `max_bytes` (defaults for new users; admin at
table maxima); create/append/truncate-grow/cross-actor rename return
`NoResource` when exceeded (`USER_QUOTA` / `USER_SETQUOTA`, shell
`quota`). `userdel` also refuses open caps on that actor and clears
tokens and durable shares that named its objects. Empty zeros format;
both slots with GALF magic that fail checks leave galfs unavailable
(no silent format). Without a slave the table stays RAM-only. Empty
files allocate no blocks; append grows through direct pointers; remove
frees blocks back to the bitmap. `cargo run` attaches a persistent
`galfs.img`. `bin/test-galfs-disk` proves multi-block persist and
dual-slot recover; `test-galfs-corrupt` refuses format on a both-bad
image; `test-fsck` runs live-table consistency after mutate;
`test-quota` covers object/byte limits; `test-shares` covers durable
home shares. Host `galfs-fsck` (crate `galexy-galf`) unlocks a sealed
image and reports structural issues; the runner checks a guest-written
`galfs.img` offline. `test-scratch` / `test-rm` fill objects to
`NoResource`; `test-blocks` fills the block pool; `test-ops` covers
rename/truncate/stat. Shell `tokens` / `USER_TOKENS` lists cards.
Utilities use `SPAWN_WAIT` so the prompt returns after `ls` / `mkdir`
exit.
Auth is password for identity (PBKDF2-HMAC-SHA256 in `galexy-crypto`,
CSPRNG salts) plus galfs tokens for authorization (see `docs/AUTH.md`).
galfs trees, paths, and sealed GALF layout: `docs/GALFS.md`. Process
wait/kill/supervise use **process Caps**, not global PIDs (plan:
`docs/PROCESS.md`; Milestone 47 + Phase 6). Every F-key shell boots
**logged out** (console + keyboard only) on a login screen
(`Galexy.OS v… (ttyN)`); password login installs a session and `logout`
returns to that screen. There is no guest account. Access cards + `su`
still switch without a password when the caller holds ALL on the target
root.
User buffers must be `USER_ACCESSIBLE` in the active tree (a destination
must also be writable) — a kernel address is present but not a user
buffer. `read` on the keyboard cap copies waiting keystrokes (0 = nothing
queued). `read` on the stats, tasks, threads, and files caps copies a fresh text
snapshot (no cursor; the syscall renders into a stack buffer so it does
not allocate). The files snapshot is the ramdisk's regular names, then each
galfs path the task's tokens may list (`Desktop/`, `dan@Desktop/notes`),
one per line. The shell keeps the current directory and accepts a leading
`/` plus `owner@` on the first component. `echo`, `cat`, `touch`, `mkdir`, `rm`, and `ls` are ramdisk
programs: the shell composes the path and `spawn`s them. `ls` and `rm`
also receive the query grant, so they can read the files snapshot.
`cd` stays in the shell, because that path lives there. A program name
on its own is a launch: `spawn` on the loader cap (EXEC) parks the
caller (`STATE_WAITING`) until the main loop has loaded the ELF
(without `SPAWN_WAIT`) or until the child exits (with `SPAWN_WAIT`).
Shell utilities set `SPAWN_WAIT` so the prompt returns after they
finish; bare program names (`hello`, `linger`) do not. `r8`/`r9` are an
optional argument, at most 256 bytes, copied onto the child's stack
(`rdi` is the address, `rsi` the length). `r10` bits are
`SPAWN_GRANT_QUERY` and/or `SPAWN_WAIT`. Any other bit is `BadValue`.
User `spawn` rejects the F-key shell names (`shell`…`shell12`) and
rejects a name that already has a live task (`NoResource`), so typing
`shell` cannot start a second keyboard-less shell that spins. The child
always receives the console, and it writes the console of the task that
spawned it. Keyboard, the loader, and power stay with the shell once it
is logged in. Boot starts one shell on each F-key, pinned to the BSP,
logged out (pre-login grants, no tokens, login banner with 1-based TTY).
Password login restores loader/query (and power for admin). F1's shell
is named `shell`; the others are `shell2` through `shell12`. F1–F12
select which cell grid is painted.
The keyboard interrupt only records that index; the main loop paints
it. Keys go to the visible console. COM1 mirrors only that console.
Presenting a reserved index is not enough; the task must have been
granted it. A ramdisk entry that is not an ELF is `Unsupported`. The
main loop, which is on the kernel page
table, loads the ELF. `FreshL4` copies the kernel root cached at init,
so the new table does not inherit another task's user mappings. The
load stays on the main loop because the loader allocates and a syscall
runs with interrupts off.
Without `SPAWN_WAIT`, the waiter is marked runnable when that load
finishes. With it, the child's exit wakes the waiter. If one of those
shells is not running or waiting, the main loop loads that shell again
with the launcher grants and admin's root token. Other tasks keep
running. The new shell starts at `/`. `power` on the power cap (POWER right) shuts the
machine down (`op` 0, ACPI S5) or resets it (`op` 1). It does not
return when the platform honors it; a return is Unsupported and the
shell says the machine stayed up. Console `write` accepts backspace
(`0x08`), tab (`0x09`), form feed (`0x0c`, clear), CR (`0x0d`), and
ESC (`0x1b`) so the ring-3 shell can edit a line and programs can emit
CSI. The screen interprets those; the serial mirror stays raw and follows
the visible console. Other
control bytes are still `BadValue`. Unknown numbers
→ Unsupported. Kernel-origin syscalls are impossible-by-structure: the
arch shim dies loudly instead.

### arch/syscall — "the mechanism" (arch/)

- MSR bring-up (order matters): `STAR::write_raw(user_cs, kernel_cs)`
  (SYSRET forces RPL 3 on both CS and SS — our consecutive GDT layout
  makes the hardware `SS = CS + 8` land on user data), `LSTAR` → naked
  entry, `SFMASK` = IF+TF (RFLAGS bits cleared at entry — see below),
  `EFER.SCE` last.
- Naked entry: `cli` FIRST (the whole syscall is atomic vs the timer;
  resumed tasks restore IF from their own saved RFLAGS), then switch to
  the CURRENT task's kernel stack via the kstack registry
  (`set_task_kstack`, updated on every switch-in to a user task — the
  entry RSP is the user stack and can't be pushed onto), then the uniform
  frame: SS (static user SS), RSP (stashed), RFLAGS (r11), CS (static
  user CS), RIP (rcx), GPRs r15..rax. The first iteration forgot the CS
  push — the cpl()==3 assert in `sched::syscalls::service` caught it
  immediately (cheap tripwears pay). SFMASK lesson (Milestone 16):
  `FMASK=0` left IF set for the entry's first instructions — a timer tick
  in that window interrupts at CPL=0 with the USER stack as RSP (no RSP0
  auto-switch below ring 3) and the tick's context lands on the user
  stack; the rotation's CR3 swap then unmaps it under the timer's return
  path → PF → double fault → silent reset. SFMASK clearing IF (and TF)
  closes the window; the user's RFLAGS still rides in R11 into the frame.
- Dispatch result: 0 resumes the outgoing frame (pop + iretq), a pointer
  switches (yield/exit handoff — the decomposition used by the timer too).
- Ring-3 segment hygiene: SYSCALL leaves DS/ES/FS/GS as the kernel's
  bootstrap selectors; user code must not do segment-based addressing
  (TODO'd).

### banner — "the boot showcase"

- Runs after full init; every `[ok]` line reads live state from the owning
  subsystem (framebuffer layout from `screen`, free frames from `mm`,
  heap-start translation from the mapper, heap size from `heap`). Adding a
  subsystem = adding a line here.

### shell — "the shell" (`shell.rs`)

Main loop: drains the key queue — printable chars echo + buffer up (the
echo goes through the console policy: screen + serial); Enter
dispatches (`help`, `stats`, `tasks`, `threads`, `clear`, `about`; a
lone name that is an ELF is started; anything else reports
`<line>: command not found`); Backspace erases. The name is looked up
in the ramdisk and handed to the loader (dispatch body factored into
`shell::exec(line)` so boot tests drive the SAME path typing does).
Foreground semantics for this in-kernel editor: the typed line closes
with a newline, the program spawns, and the prompt is NOT reclaimed
until the program exits (`sched::is_name_running` polled by `poll`) —
the program's output always lands on its own line, never on an input
line. The ring-3 shell returns its prompt when the load finishes.
The status bar (`render_status_bar`) redraws the bottom line
in-place once per second (uptime + per-thread tick counts + frames free)
via `screen::draw_status_bar` — it never moves the text cursor (hijacking
`set_pos` for the bar could leave typing on the status row so input
vanished on the next redraw). That row is not part of the text scroll,
and the bar is drawn without feeding the CSI parser.

### sched/ramdisk — "the archive" (`sched/`)

The runner packs user programs into a USTAR tar and the bootloader maps it
(`BootInfo.ramdisk_addr` = a VIRTUAL address, framebuffer-like contract).
`sched::ramdisk::init` publishes those bytes once (kernel-lifetime, so a
`&'static [u8]` view); `find(name)` walks them read-only via
`galexy-core::TarCursor` per call. `for_each_name` walks those names
for the files capability. Consumers (the shell's launch and `ls`, `open`,
test kernels) never touch raw BootInfo ramdisk fields again. An `open`
holds that `&'static` slice plus a per-cap cursor; closing the cap does
not free ramdisk bytes.

## Concurrency model (SMP, two CPUs — Milestones 18–19)

- **Per-CPU ownership first.** Rotation state (`CPU_SCHED`), LAPIC access,
  SYSCALL entry scratch (gs:[16]/[24]), TSS.RSP0, and the idle loop are
  per-CPU by construction — no lock is needed where only one CPU touches.
- **Pinned-at-spawn + owner-reap, with idle stealing.** `Thread.owner`
  (assigned round-robin at spawn, returned race-free to the caller)
  decides which CPU's rotation a thread rides and which CPU's `reap()`
  frees it. An idle CPU may flip that pin (work stealing, M19) once the
  victim has published a stable context (naked tail, after `mov rsp`);
  the owner-reap rule still holds after the flip. "A thread is current on
  exactly one CPU" stays true: a steal only takes a slot that is not
  current on its owner and whose stack tail has finished, and the new
  owner does not enter until its next tick.
- **`THREADS` is THE global lock** (`spin::Mutex`); the naked timer switch,
  syscall handoff, steals, and reapers serialize on it briefly — no nested
  locks. The IRQ gate is still part of every acquisition (a local
  `hlt`-sleeping CPU must not re-enter a held lock). Steal correctness
  rides this lock; there is no separate migration lock.
- **BSP homeownership**: cooperative tasks (`SCHED` queue), the status
  bar, and the framebuffer stay BSP-only by design — one display, one
  input queue, one accounting yardstick (`main_ticks` = the BSP's slot-0
  counter). The interactive shell is a ring-3 program (Milestone 21)
  spawned on the BSP with `no_steal`, so idle CPUs cannot migrate it
  onto the AP where the keyboard IRQ is not delivered.
- **Kernel-half remaps are shootdowns (M19).** Shared kernel-half PTEs
  are no longer frozen after boot. `map_kernel_page_broadcast` maps,
  flushes locally, and broadcasts precise INVLPG (vector 0xF8, mailbox
  of VAs, lock-free handler). The initiator holds no Rust spin lock
  across the broadcast. Heap growth is the production path: map the
  chunk under the IRQ gate, one batched broadcast, then `extend`. A
  conflicting grower waits
  on `GROWING` with IF=1. Per-task trees stay local (that CPU's CR3
  swap is a full flush).
- Print lock: `println!` → screen lock (BSP-side consumers only today).
  Interrupt handlers never touch the screen lock (serial or atomics only).
  The shootdown handler is the same rule pushed further: serial or
  atomics, never a lock.

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
- APIC discovery is MADT-based (RSDP → XSDT/RSDT walk, checksums enforced);
  no fallback to hard-coded MMIO bases — a machine without ACPI tables
  fails loudly rather than guessing.
- The LAPIC timer calibration assumes the PIT exists (it does on every
  x86 platform worth booting; QEMU emulates it under both SeaBIOS and
  OVMF). TSC-deadline mode is the follow-up if drift ever matters.
- x2APIC-mode hosts take the MSR path (`0x800 + offset>>4`); QEMU defaults
  to xAPIC — both are exercised by the access-layer abstraction, only xAPIC
  by the QEMU test suite (assert in `bin/test-apic`).
- `-no-reboot` is always passed to QEMU so triple faults surface as an exit
  instead of an infinite reboot loop. A `reboot` request still pulses the
  reset line; QEMU then exits rather than restarting the guest. `shutdown`
  powers the VM off (process exit 0, not the isa-debug-exit code).
- Fresh artifacts can live in *multiple* `OUT_DIR` hash dirs; pick images by
  mtime (`ls -t`) when testing manually.
