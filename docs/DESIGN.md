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
│       ├── drivers/     # device drivers (screen, serial, keyboard, block/ata, ...)
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
├── gxc/                 # host mini Rust-subset compiler (gxr → ELF) — docs/COMPILER.md
└── runner/              # host tooling: builds BIOS+UEFI images, launches QEMU,
                         #   hosts the boot tests (tests/boot.rs)
```

Dependency order: `galexy-core, galexy-abi → (nothing)`;
`galexy-os → {core, abi}`; `galexy-rt → abi`; user programs → `galexy-rt`.
Userspace programs NEVER link against the kernel — `galexy-abi` is the only
contract between them.

The preemptive scheduler is split so the file does not keep growing.
`sched/thread.rs` is the thread table, reap, and pin. `sched/spawn.rs` is
queued spawn, shells, wait/kill, and login sessions. `sched/task.rs` is
the file, pipe, and channel syscalls. `sched/iowait.rs` is the timer
handoff, parked I/O, and the per-tick console budget. `sched/mod.rs`
keeps the cooperative queue and re-exports the names the rest of the
kernel already calls. The shell is split the same way: `state`, `edit`,
`builtins`, and `jobs`. `userspace/shell/src/main.rs` is login and the
dashboard.

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
   Unsupported=4, BadValue=5, NotFound=6, NoResource=7, Interrupted=8, Locked=9) — are permanent.
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
and is saved with the TTY. The shown TTY draws a two-pixel underscore
at the cursor on a 500 ms phase (`timer_ticks / 500`). The main loop
wakes an idle CPU at that edge. The mark is cleared before the next
glyph so a block cannot stick on a cell the cursor has left.

Each TTY's cell grid is the scrollback: `TTY_ROWS` (128) by `TTY_COLS`
(200). Scrolling drops the top line. There is no extra history buffer.

Supported controls, and nothing else: tab, CR, LF, backspace (`0x08`),
form feed (`0x0c`), ESC, and the CSI subset above. ASCII BEL (`0x07`)
is a PC-speaker beep in the console writer, not a CSI and not a glyph.
Other CSI final bytes are consumed and ignored. The console stays
byte/ASCII-centric: the keyboard may deliver a Unicode scalar, but the
shell line editor accepts only ASCII graphic characters (and space
outside a secret prompt). Full Unicode editing is a non-goal.

Ctrl-C (`U+0003`) cancels a shell prompt, or, when a TTY foreground job
Cap is live, kills that job and is not queued. Ctrl-D (`U+0004`) is
not end-of-file; the line editor ignores it. Esc cancels a prompt.

### serial — "the console on a wire" (`drivers/`)

`uart_16550` at COM1. Panics, boot info, and debug output go here, and
the console policy also mirrors the **visible** TTY's bytes here so a
headless boot is readable. Receive is the other direction: IRQ4 (vector
36) drains the FIFO and `keyboard::push_char` delivers each byte to the
active TTY. `\r` is Enter, a following `\n` is swallowed (CRLF is one
key), DEL and BS are backspace, and bytes above ASCII are dropped. The
UART lock is dropped before that delivery. One `_print` holds the lock
for the whole line, so two CPUs cannot tear it, and transmit harvests
pending receive bytes while the FIFO is busy so a paste during that hold
cannot overrun the 16-byte FIFO. `serial_println!` also appends the
line to a 32×96 dmesg ring. `read` of reserved cap `0x8007` (query
grant, same rule as `stats`) returns the newest lines that fit.
Consecutive lines containing `login fail` collapse to one ring entry
so a lockout storm cannot evict a fault. The shell builtin is `dmesg`
(pre-login has no query grant, so that read is denied). Idle-steal
tracing is the non-default `verbose-sched` feature; the one-line reap
count and tree-free breadcrumbs stay in every build.

### keyboard — "the input decoder" (`drivers/`)

PS/2 controller bring-up lives HERE now (`keyboard::init`, called from
`arch::init`): the i8042 first-port enable (`0xAE` to port 0x64) + a
stale-output-buffer drain — firmware (SeaBIOS polled keyboard, OVMF alike)
may leave the port disabled or bytes pending, and a full buffer never
re-asserts the line (the first real keystroke would black-hole). This is
controller work, not interrupt-controller work — it runs on every boot path.

IRQ1 handler (LAPIC-delivered via the I/O APIC) → `pc_keyboard` (US layout,
scancode set 1). Unicode characters, and COM1 bytes via
`keyboard::push_char`, are pushed into the active TTY's `kcore::Ring`
(twelve rings, one per F-key). F1–F12 do not become input and have no
UART equivalent: the handler stores the TTY index and the main loop
paints that grid.
The in-kernel editor drains TTY 0 via `keyboard::pop_key()`. A ring-3
shell `read`s the keyboard capability (`reserved::KEYBOARD_INDEX`, READ)
and gets the queue of the TTY it was started on. A zero-length success
means that queue is empty, not that input ended. A short read that
cannot fit the next character's UTF-8 puts that character back
(`unget_key_tty`). Each queue holds 64 characters (`QUEUE_CAPACITY`).
A full queue drops the newest character, counts the drop, and prints
`[kbd] queue full` on the first drop and every 16th.

```rust
pub fn add_scancode(scancode: u8)   // called from the IRQ handler only
pub fn push_char(c: char)           // PS/2 Unicode and COM1 bytes
pub fn pop_key() -> Option<char>    // drains TTY 0
pub fn pop_key_tty(tty: u8) -> Option<char>
```

### block — `BlockDevice` (`drivers/block.rs`)

galfs talks only to this trait (present / capacity / read / write /
flush). Impls: `virtio_blk::VirtioBlk` (preferred when present) and
`ata::PrimarySlave`.

```rust
pub trait BlockDevice: Sync {
    fn present(&self) -> bool;
    fn capacity_sectors(&self) -> u64;
    fn read_sectors(&self, lba: u32, dst: &mut [[u8; SECTOR]]) -> Result<(), SysError>;
    fn write_sectors(&self, lba: u32, src: &[[u8; SECTOR]]) -> Result<(), SysError>;
    fn flush(&self) -> Result<(), SysError>;
}
```

### ata — primary IDE slave (`drivers/`)

PIO LBA28 on the primary IDE slave (drive index 1), exposed as
`PrimarySlave: BlockDevice`. The boot image is the master and is never
touched. `present()` probes once via IDENTIFY (words 60–61 / 100–103 →
`capacity_sectors()`); when the slave is absent or too small for both
GALF dual slots, galfs stays RAM-only. Reads/writes reject LBAs past
capacity. `flush()` issues FLUSH CACHE after a committed GALF slot
write. ERR/DF and command timeout log `[ata] I/O error` or
`[ata] I/O timeout` and return `Unsupported` — they do not panic.
A missing slave returns `Unsupported` without that log (`test-ata`).
The runner attaches a second raw image at `if=ide,index=1`
without a snapshot (`cargo run` and the persistence tests) so writes
survive across QEMU processes. Default `cache=writethrough`; the flush
matrix also boots with `writeback` and `none`. `cargo run` prefers
virtio-blk; set `GALEXY_GALFS_IDE=1` for this path.

```rust
pub struct PrimarySlave; // impl BlockDevice
pub fn present() -> bool
pub fn capacity_sectors() -> u64
pub fn read_sectors(lba: u32, dst: &mut [[u8; SECTOR]]) -> Result<(), SysError>
pub fn write_sectors(lba: u32, src: &[[u8; SECTOR]]) -> Result<(), SysError>
pub fn flush() -> Result<(), SysError>
```

### virtio_blk — virtio 1.x PCI (`drivers/`)

Virtio 1.x over PCI capabilities (common / notify / ISR / device cfg)
and MMIO BARs, with `VIRTIO_F_VERSION_1`. PCI config is ECAM when ACPI
published `MCFG`, else ports `0xCF8` / `0xCFC`. The scan walks PCI-PCI
bridges so a function behind a q35 root port is visible. One outstanding
request. Completion prefers MSI-X; INTx (I/O APIC, level, active-low)
is the fallback. The requester parks `STATE_WAITING` + `IO_BLOCK`, and
a missed line is noticed on the next timer tick. FLUSH via
`VIRTIO_BLK_T_FLUSH`. galfs selects this over ATA when probe succeeds.
`disable-modern=on` keeps the legacy I/O BAR and logs
`legacy IO BAR`. The runner default is `-M q35` without
`disable-modern=on`.

```rust
pub struct VirtioBlk; // impl BlockDevice
```

**DMA follows physical pages, not `translate(buf) + len`.** A kernel
buffer is contiguous in virtual memory only. The bootloader's `.bss`
frames are usually adjacent, which is why a single translation ever
worked; at a 2 MiB boundary it allocates a page-table frame between two
data frames, and a sector straddling that boundary used to DMA into the
page table (`[pf] PAGE FAULT in ring 0` inside galfs `DISK_BUF` on the
boots where KASLR put it across the boundary; on a write, page-table
bytes went to disk). `push_data_descs` translates every 4 KiB page of a
request and emits one descriptor per physically contiguous run, so a
request is up to `MAX_BATCH_SECTORS` (32 sectors, at most five
descriptors) regardless of alignment. galfs' `DISK_BUF` is page-aligned
(`SectorBuf`) so the common request is one descriptor. The legacy ring
layout (desc + avail on one page, used on the next) still requires those
two frames to be physically adjacent and refuses the device otherwise.
`bin/test-dmasplit` maps two non-adjacent frames at adjacent pages,
plants a sentinel in the frame that is physically next, and checks
straddling reads and writes on both transports.

The keyboard handler never takes the screen lock. Locks are tiny and
never nested (decode under one lock, push under another), so IRQ
context is safe. Queue overflow drops the newest key (documented).

### arch — "the plumbing"

Init order: GDT/TSS (per-CPU slot 0) → per-CPU GS substrate → ACPI
(MADT, FADT boot-arch, MCFG, HPET) → LAPIC (x2APIC when CPUID reports
it, then calibration) → I/O APIC (COM1) → 8259 (remap+mask, or mask
only when the FADT says the pair is absent) → virtio-input or i8042 →
AP boot (trampoline + per-AP init + timer arms) → `sti`. The MMIO
window (P4 203) is reserved in `mm::init`, before this sequence.

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
  count). Timer delivery is **deadline one-shot** (not a free-running
  1 kHz periodic): busy CPUs re-arm a preempt quantum (`online()` ms
  share-split); idle CPUs sleep until the next whole second (status bar /
  uptime), the next `sleep` deadline, or an earlier device IRQ.
  `timer_ticks` advances by the armed duration so uptime stays honest
  under tickless idle. `Syscall::Sleep` parks on a global sleep queue
  woken from the timer path — see `docs/SCHEDULING.md` (Milestone 56).

### arch/ioapic — "external interrupt routing" (arch/)

- MMIO register page mapped at fixed kernel-half P4 entry 201 (the LAPIC
  page's sibling); IOREGSEL/IOWIN pair indexes the register space.
- Bring-up (BSP-only, once): version sanity (I/O APICVER ≥ 0x11), ALL
  redirection entries masked first (inherited state is firmware's), then
  two wirings, both edge-triggered, active-high, physical destination =
  the BSP's LAPIC ID: the keyboard (ISA IRQ1 → GSI → vector 33) and COM1
  (ISA IRQ4 → GSI → vector 36). Masked-by-default is the rule: mask what
  you don't use. `arch::init` drains the UART once after the route is
  unmasked so a byte that arrived early (line already high) is not stuck
  waiting for an edge.

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
  holds no lock of `grow`'s own (see shootdown below). The CALLER of
  `alloc` may hold locks — `GlobalAlloc` cannot know — which is why every
  kernel lock's spin loop services shootdowns (`sync::Mutex`). A second
  CPU that OOMs mid-growth does not fail the alloc: it spins, servicing
  the mailbox so the in-flight grower gets its ack, until the chunk lands
  (a spin-count watchdog panics if the grower sticks), then the caller
  retries. It does NOT re-enable interrupts: the caller may be inside an
  IRQ gate holding locks the timer path needs.
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
  `seq`, servicing other initiators' requests while it waits. Holds no
  Rust spin lock of its own. Any IRQ state is fine. A target that has not
  acked after 2^26 spins is reported to serial (`[shootdown] cpu A
  waiting on cpu B …`) and the wait continues — the line is the first
  evidence a hung boot gives about which CPU is wedged IF=0.
- Target: the 0xF8 handler scans unseen seqs, `invlpg`s the listed VAs,
  records `seen` (`fetch_max`). Takes no locks, ever (an IF=0 lock holder
  must still be able to ack, or a broadcasting initiator spins forever).
- **Polled ack (galexy.os#86).** The IPI cannot reach a CPU that is
  spinning IF=0 on a lock. If the lock's holder is the broadcaster, both
  CPUs stop with interrupts off and nothing prints — that was `passwd`:
  the BSP's 1 Hz status bar built `String`s under `THREADS`, one
  allocation grew the heap and broadcast, and the AP's `passwd` syscall
  was spinning on `THREADS`. The same handler body is therefore exposed
  as `shootdown::service_pending()`, and the kernel's lock type
  (`sync::Mutex`, a `spin` mutex with a custom `RelaxStrategy`) calls it
  on every spin. The IPI stays as the fast path; the poll guarantees
  progress whatever the waiter's IF state. `test-lockgrow` pins an IF=0
  lock contender to the AP and grows the heap under that lock on the BSP:
  it hangs without the poll and passes with it.
- `map_kernel_page_broadcast` is the single-page primitive: map, local
  flush, then broadcast. Heap `grow()` batches instead: `map_page` the
  chunk under the IRQ gate, one `shootdown_others` for every new VA (a
  16-page chunk fills one mailbox slot), then `Heap::extend`. Per-task
  trees stay local: only that CPU's CR3 swap (a full flush) publishes
  them.

### sched — "the scheduler" (`sched/`)

Runtime plan (time, block/wake, policy freeze): `docs/SCHEDULING.md`
(Phase 7). Process Caps / init: `docs/PROCESS.md`.

**Block / wake (Milestone 57).** `STATE_WAITING` covers spawn wait,
Cap-wait, `sleep`, keyboard `read`, and pipe read/write. Parked I/O
stores buffer addr/len on the thread; the keyboard IRQ and pipe peer
activity complete the syscall via the waiter's page tables
(`with_table` + phys-map copy). Archive and galfs `read`/`write` stay
**non-blocking** (short counts) — they never park and never spin.
Cap-kill of a sleep/I/O waiter stamps `SysError::Interrupted` then
`EXITED` (Cap-waiters still see exit code `137`).

**Policy freeze (Milestone 58).** Numbers, non-goals, and sched lock
rules live in `docs/SCHEDULING.md` (Frozen policy + Sched lock / IRQ
rules). Wiring stays here; do not fork a second policy table. The
kernel lock-order table is in **Concurrency model** below.

**Init / seats / jobs (Milestones 53–55).** When ramdisk `init` is present
the kernel loads it once (`Thread.is_init`, `Grants::init`). On parent
reap, process Caps for live children move to init and `parent_slot`
follows. Cap-kill of init is `AccessDenied`; init exit panics. Init
spawns `shell`…`shell12` (shared `shell` ELF, 1-based TTY arg,
`pre_login` grants, BSP/no-steal); only init may spawn those reserved
names. Kernel `ensure_shell` runs only when init is absent. Seat restart
is round-robin Cap-wait in userspace init; `svc` / extra services are
waived for v1. Per-TTY foreground job (M55): spawn drain records the
child as the TTY’s Ctrl-C target; `^C` stops that task (exit 137) without
queuing into the seat keyboard ring.

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
top-free P4 entry (`< 256` — 512 GiB per task) mapped RX (never
writable), user stack (4 pages, RW\|NX\|USER) at +1 GiB, an RW\|NX
scratch page right above. ELF `PT_LOAD` that is both writable and
executable is refused (W^X). All kernel-side staging (blob bytes,
zeroing, the initial ring-3 frame) goes through the BACKING FRAMES
(`frame_virt`) — the phys map is present in every tree, so the task tree
never needs to be active to write it. `Thread.cr3` swaps in both switch
paths (timer + syscall handoff), no-op when unchanged. The task's own
kernel-mode stack (heap Vec) serves its ring 3→0 crossings via TSS.RSP0.
Reaping = `free_user_tree(root, p4_index)`: one walk takes tables AND
data frames; the accounting closes exactly (proven by
`bin/test-treechurn.rs`). CRASH ISOLATION: one unmapped guard page below
the stack; the page-fault vector runs a NAKED handler (timer-shaped
prologue + error-code word ⇒ raw-offset reads, never resumed): ring-3
faults tombstone + rotate, ring-0 faults report + park. Jumping to
scratch proves NX (`bin/test-wx.rs`).

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
fixed table of **8** opens (`MAX_OPEN_FILES` — kept small on purpose so
`open` never heap-allocates on the IF=0 syscall path; a grow would force
a shootdown ack from every CPU). Indexes from `FILE_CAP_BASE` (3),
authoritative grant intersected with the handle snapshot. Raising the
cap is a later change with a non-blocking allocation story. `read` copies the next bytes of an open file
(short-read at 1 KiB, 0 at EOF);
`close` drops the slot. `create(name, len, flags)` (syscall 9) puts a
path in a fixed galfs table (128 objects, 32 actors with one root each,
64-byte component names, shared 256×512-byte block pool with 8 direct
pointers plus one single-indirect block per file / 32 KiB max, no heap
on the syscall path — Milestone 45 / GALF v11). A path ending in `/` is
a directory and returns 0. A file returns
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
`galfs::init` loads the newest valid GALF **v11** sealed slot (dual
288-sector images: wrapped volume key + ChaCha20-HMAC payload of
actors/objects/shares/bitmap/blocks + per-actor quotas + single-indirect
pointers, generation + ciphertext CRC + structural checks) or formats
that admin tree under a fresh volume key; create/remove/append/rename/
truncate/useradd/userdel/share/unshare sync to the inactive slot and
flush the cache; `sync()` (syscall 20) is an explicit barrier (shell
`sync`). Each actor has durable `max_objects` / `max_bytes` (defaults
for new users; admin at table maxima); create/append/truncate-grow/
cross-actor rename return `NoResource` when exceeded (`USER_QUOTA` /
`USER_SETQUOTA`, shell `quota`). `userdel` also refuses open caps on
that actor and clears tokens and durable shares that named its objects.
Empty zeros format; both slots with GALF magic that fail checks leave
galfs unavailable (no silent format). Without a slave the table stays
RAM-only. Production boot does not auto-unlock: the seat prompts for
the volume passphrase (`USER_UNLOCK`) before mounting. A wrong
passphrase keeps the RAM table and refuses disk sync. The last logout
and power-off zero the volume key (cold-boot remanence accepted). The
payload MAC stays HMAC-SHA256; RFC 8439 Poly1305 waits for a versioned
cutover. `bin/test-unlock` covers wipe, a rejected guess, and remount.
Empty files allocate no blocks; append grows through direct
then single-indirect pointers (32 KiB max); remove frees blocks back to
the bitmap. `cargo run` attaches a persistent `galfs.img`.
`bin/test-galfs-disk` proves multi-block persist and dual-slot recover;
`test-galfs-corrupt` refuses format on a both-bad image; `test-fsck`
runs live-table consistency after mutate; `test-quota` covers
object/byte limits; `test-shares` covers durable home shares;
`test-indirect` covers past-direct writes and 32 KiB files;
`test-cards` covers token/share slot exhaustion and confused-deputy
`share` rules. Host `galfs-fsck` (crate `galexy-galf`) unlocks a sealed
image and reports structural issues; the runner checks a guest-written
`galfs.img` offline. `test-scratch` / `test-rm` fill objects to `NoResource`;
`test-blocks` fills the block pool; `test-ops` covers
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
Console `write` accepts ASCII BEL (`0x07`): the console policy runs a
short PC-speaker beep (`arch::speaker`) and does not draw the byte.
The ring-3 shell emits BEL on “command not found”.
User buffers must be `USER_ACCESSIBLE` in the active tree (a destination
must also be writable) — a kernel address is present but not a user
buffer. `read` on the keyboard cap copies waiting keystrokes (0 = nothing
queued). `read` on the stats, tasks, threads, and files caps copies a fresh text
snapshot (no cursor; the syscall renders into a stack buffer so it does
not allocate). The files snapshot is the ramdisk's regular names, then each
galfs path the task's tokens may list (`Desktop/`, `dan@Desktop/notes`),
one per line. The shell keeps the current directory and accepts a leading
`/` plus `owner@` on the first component. `echo`, `cat`, `nano`, `touch`, `mkdir`, `rm`, and `ls` are ramdisk
programs: the shell composes the path and `spawn`s them. `ls` and `rm`
also receive the query grant, so they can read the files snapshot.
`nano` also receives the keyboard grant and the shell Cap-waits on it.
Ctrl-G opens a help page of the keys that are not already on the shortcut
bar; the next key closes that page and is not inserted.
`cd` stays in the shell, because that path lives there. A program name
on its own is a launch: `spawn` on the loader cap (EXEC) parks the
caller (`STATE_WAITING`) until the main loop has loaded the ELF, then
returns a **process Cap** (`PROC_CAP_BASE`…). Shell utilities set
`SPAWN_INHERIT` (session tokens) and Cap-`wait` so the prompt returns
after they finish; bare program names (`hello`, `linger`) drop the Cap
(fire-and-forget). `SPAWN_WAIT` still parks until exit as a convenience
(Cap remains installed). `r8`/`r9` are an optional argument, at most 256
bytes, copied onto the child's stack (`rdi` is the address, `rsi` the
length). `r10` bits are `SPAWN_GRANT_QUERY`, `SPAWN_WAIT`,
`SPAWN_INHERIT`, and/or `SPAWN_GRANT_KEYBOARD` (bit 3). Any other bit
outside the rights mask is `BadValue`.
User `spawn` rejects the F-key shell names (`shell`…`shell12`) and
rejects a name that already has a live task (`NoResource`), so typing
`shell` cannot start a second keyboard-less shell that spins. The child
always receives the console, and it writes the console of the task that
spawned it. `SPAWN_GRANT_KEYBOARD` also gives the child the keyboard
so an interactive program (`nano`) can read keys while the shell
Cap-waits. The loader stays with the shell once it is logged in. Power
stays on init while init is alive. Boot starts one shell on each F-key, pinned to the BSP,
logged out (pre-login grants, no tokens, login banner with 1-based TTY).
Password login restores loader/query. An admin login receives Power
only when init is not running. F1's shell
is named `shell`; the others are `shell2` through `shell12`. F1–F12
select which cell grid is painted.
The keyboard interrupt only records that index; the main loop paints
it. Keys go to the visible console. COM1 mirrors only that console.
A console write is committed only up to a byte that is not inside an
escape sequence: the per-tick budget (512 bytes) used to slice a CSI
cursor command in half, and the next kernel log on COM1 then landed
inside that sequence. The host terminal stopped painting until a later
byte resynced it, so a screen editor (and a long listing) looked frozen
the way `ls` did before the shell waited for the child. A write that
cannot fit the next whole sequence returns a short `0` and the runtime
yields and retries.
Presenting a reserved index is not enough; the task must have been
granted it. A ramdisk entry that is not an ELF is `Unsupported`. The
main loop, which is on the kernel page
table, loads the ELF. `FreshL4` copies the kernel root cached at init,
so the new table does not inherit another task's user mappings. The
load stays on the main loop because the loader allocates and a syscall
runs with interrupts off.
Without `SPAWN_WAIT`, the waiter is marked runnable when that load
finishes (Cap bits in `rax`). With it, the child's exit wakes the
waiter (exit code in `rax`). The child stays parked until that Cap is
installed and, when waiting, until `wait_child_slot` names it — a
short program on the other CPU would otherwise exit before the parent
is linked, and the exit would wake nobody. `wait(cap)` / `kill(cap)`
are the Cap syscalls. If one of those
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
- Ring-3 segment hygiene: SYSCALL entry leaves DS/ES/FS/GS as the
  kernel bootstrap selectors. On return to ring 3 (syscall, timer, and
  page-fault iretq tails), DS/ES are reloaded with the user data
  selector (RPL 3). FS stays the kernel bootstrap selector (unused).
  GS keeps the kernel per-CPU base — userland must not load GS.

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
in-place once per second: TTY, uptime, heap used/size, galfs blocks,
active tasks, free frames, then abbreviated per-thread ticks when they
fit — via `screen::draw_status_bar`. It never moves the text cursor
(hijacking `set_pos` for the bar could leave typing on the status row so
input vanished on the next redraw). That row is not part of the text
scroll, and the bar is drawn without feeding the CSI parser. Arrow keys
arrive as CSI (`ESC [ A`…`D`) on the keyboard Cap. Up/down browse
`shell.history`. Left/right move inside the line; Ctrl-A / Ctrl-E jump
to the ends; Ctrl-U clears the line. `echo text | cat` is one pipe:
the shell `give`s the write end to `echo` and the read end to `cat`.
A `*` word expands to names in the current directory from the files
snapshot. A bare program shares the console with the seat (no
background job). Login, logout, and `su` clear the working directory.
A failed `cd` leaves it, so the prompt stays on the directory that
exists.

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
  rides this lock; there is no separate migration lock. The thread vec
  grows with the lock dropped: that allocation can shoot down TLBs, and
  the other CPU's timer is already inside `THREADS` with interrupts off.
- **Login cool-down (`LOCKOUT`)** is a separate RAM table. Acquire it only
  when `THREADS` and the galfs table are not held. It is IRQ-gated. The
  deadline is absolute `timer_ticks` (Milestone 43).
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

### Memory policy (Milestone 48)

**PCID / GLOBAL.** Every CR3 swap is a full TLB flush. No suite test is
TLB-bound: QEMU time is PBKDF2 (10 000 HMAC rounds, about two seconds
each on this TCG host) and ATA PIO. PCID and global kernel pages are
waived until a profile shows CR3 churn as the cost.

**Demand paging.** Non-goal. A user task gets a fixed image, a 4-page
stack (`USER_STACK_PAGES`) with one unmapped guard page below it, and
one scratch page. There is no user heap. A user `#PF` kills that task
(`test-userfault`). The kernel heap grows by an explicit map, not by
faulting.

**Kernel heap.** Start is 100 pages (400 KiB) at `0x0000_5555_5555_0000`
(P4 index 170). Growth is 16 pages (64 KiB) per step, cap
`HEAP_MAX_PAGES` (64 × 1024 pages, 256 MiB). A failed `alloc` returns
null. Syscall paths that cannot proceed return `NoResource` (object
table, frame reserve `SPAWN_FRAME_RESERVE` = 64). An `expect` on a
frame the kernel must have is a panic: that is a kernel bug, not a
user error.

**FSGSBASE.** Per-CPU GS requires CPUID 7.0 EBX bit 0. The runner and
`cargo run` pass `-cpu max`. Boot panics with
`cpu: FSGSBASE unsupported — per-CPU mechanism requires it (CPUID 7.0.EBX bit 0)`.
A software GS fallback is out of scope.

**ASLR.** User-side ASLR is waived. Every user ELF links at
`USER_IMAGE_BASE`; randomizing the P4 slot would not hide that address
from the program, and it would break the single load address the loader
and the ABI share. Kernel KASLR is the bootloader's `mappings.aslr`,
limited to P4 indexes 1..=24 so it cannot land on the user image
(25), the physical-memory map (128), the heap (170), the LAPIC (200),
or the I/O APIC (201). The BIOS stage-4 stack is 64 KiB
(`third_party/bootloader-x86_64-bios-stage-4`); the stock `0x7c00`
stack overflows while seeding the bootloader RNG. `docs/THREAT.md` →
CPU features is the table.

**CPU security features.** `arch/cpu.rs` sets `CR4.FSGSBASE` always
(panic if the bit is missing) and `CR4.SMEP` / `CR4.SMAP` / `CR4.UMIP`
when CPUID leaf 7 reports them. User-VA syscall copies go through
`arch::user_copy`, the only `stac` / `clac` site; copies that walk the
physical map do not. `#GP` (UMIP) shares the naked fault handler with
`#PF`, so a ring-3 fault kills that task. Spectre v1 masking and the
KPTI / IBRS / MDS / CET waivers are in `THREAT.md` → CPU features.

**Hostile ELF images.** `loader::validate_elf` runs before any page
is mapped (the spawn syscall calls it on the ramdisk bytes): ELF
magic, `ELFCLASS64`, `ET_EXEC`, `EM_X86_64`, the phdr table inside the
file (`xmas_elf` slices it unchecked), `phentsize == 56`, 1–64 phdrs,
`filesz <= memsz`, segment bytes inside the file, every `PT_LOAD`
inside `[USER_IMAGE_BASE, +USER_IMAGE_WINDOW)` (512 MiB), no `W|X`
segment, no overlapping `PT_LOAD` pages, the entry inside an
executable segment, and the page count of one segment and of the
image both inside `USER_IMAGE_MAX_PAGES`. Each failure is
`Unsupported` (not our ELF shape) or `BadValue` (ours but malformed)
— never a panic. `spawn_program` returns that `Result`. Trusted
ramdisk callers (the shell, init) still `expect` a binary this repo
just linked. `bin/test-badelf` forges nineteen mutations of the real
`hello` image plus truncated header and table cases and asserts each
one. The ramdisk is still trusted input — measured, not signed
(`THREAT.md` → Trust assumptions).

**User pointers.** Syscalls copy path, name, password, and write bytes
into stack buffers only after a length check (`MAX_NAME`, `MAX_READ` /
`MAX_WRITE`, `SPAWN_ARG_MAX` 256, password ≤ 64) and a `user_buffer`
walk. `user_buffer(0)` is an empty range, not `BadBuffer`. The copy
itself is `arch::user_copy` (`stac` / `clac` when SMAP is on). Parse
and KDF run on that copy, so a later store in the user page cannot
change the bytes mid-check. A waiter whose CR3 is zero is a kernel
bug: keyboard and pipe completion `expect` a kept kernel address and
`debug_assert` that the slot is `WAITING`.

**Rotation watchdog.** Each CPU records the tick of its last scheduler
entry. Another CPU that still has `STATE_RUNNING` threads and has not
entered the scheduler for 2 s is dumped once: rotation indexes,
`armed_ms`, and any in-flight shootdown mailbox (`[watchdog]`). A
wake that arrives while an idle CPU is programming its tickless
deadline re-arms a quantum, and `cpu::kick` sends IPI `0xF7` so the
owner leaves `hlt`. Spawn pokes the same way: a new `RUNNING` thread
on an idle CPU must not wait for the next whole-second deadline.
The dump does not panic and does not contain secrets. A long `IF=0`
PBKDF2 can trip it once.

**Secrets.** Login, useradd, passwd, and volume-unlock staging buffers
are `wipe_bytes`'d before the syscall returns, including the error
path. `check_password` wipes its digest. PBKDF2 wipes HMAC key blocks
and the working block. Volume key and passphrase are wiped on last
logout and on power. Cold-boot RAM remanence stays accepted.

**Canaries and guards.** Reap reads the kstack canary
(`STACK_CANARY`) and panics in every build if it changed — a corrupted
kernel stack is not a soft error. `test-threadexit` reaps clean
returns, so a broken canary fails that boot. The user stack's guard
page is unmapped; `test-userfault` recurses into it, kills only that
task, and requires `free_frames` back at the boot baseline.
`test-treechurn` and `test-smpstress` are the same budget after N
spawn/exit cycles. Tests that peek a task's scratch page after it
exits pin the task to the BSP (`spawn_user_task_on(.., 0, ..)`,
`spawn_user_launcher`): the owner CPU reaps, and an AP owner would
zero-wipe the tree from its idle loop before the BSP's peek.

**Debug vs release.** Canary mismatch always panics. GALF structural
checks return failure and refuse the image (soft) in every build.
`debug_assert` on encode lengths stays debug-only.

**Cooperative scheduler.** `run()`'s mid-sweep fairness and `TaskCtx`'s
eight `u64` slots are waived for review. Preemptive threads are the
path that runs user code (Milestone 58).

### Lock order

Allowed nesting is top to bottom. Never take a lock above one you
already hold. IRQ-gate means `interrupts::without_interrupts` (or an
IRQ that already has IF=0) around the acquire.

| Lock | May hold while taking | IRQ-gate |
| --- | --- | --- |
| `THREADS` | galfs `TABLE`, then `VOLUME_KEY`, then `VOLUME_PASS`, then `CHANS`. No heap allocation while held (snapshot into fixed arrays, allocate after the drop) | yes |
| galfs `TABLE` | `DISK_BUF` only while encoding; not `THREADS`. The PBKDF2 for `login` / `passwd` / `useradd` runs before the lock is taken (`PasswordCred`) | yes. Syscalls arrive IF=0; the main-loop entries (`sync_to_disk`, `wipe_volume_key`, `seal_and_lock`) gate themselves — a preempted main-loop holder on the BSP deadlocked the shell's next galfs syscall on the same CPU (galexy.os#86) |
| `VOLUME_KEY` | `VOLUME_PASS` | with the caller |
| `LOCKOUT` | nothing above | yes; never under `THREADS` or `TABLE` |
| Frame `USED` then `USABLE` | nothing else | yes |
| Heap `INNER` | nothing else; growth drops it before shootdown. A second grower that lost the race polls `shootdown::service_pending()` while it waits and never re-enables IF (galexy.os#86) | yes |
| `ATA` / virtio `DEV` then `DMA` | nothing else. The timer may take `DMA` alone to read `used.idx`; it never takes `DEV`. The waiter holds `DEV` across the halt and stays on that CPU (`xfer_wait_cpu`) so a same-CPU switch cannot spin on it | with the caller (syscall or BSP); `used.idx` disables interrupts around `DMA` |
| Screen `SCREEN` / `GRIDS` | nothing else | BSP; IRQ handlers do not take it |
| Keyboard queue | nothing else (drop warning takes `DMESG` after the queue lock drops) | IRQ may push; readers gate |
| `DMESG` | nothing else | with `serial_println!`; never acquired before `THREADS` |
| COM1 `SERIAL1` | nothing (received bytes are delivered after it drops) | TX path gates; the receive IRQ takes it |
| `CHANS` | nothing else | yes; only after `THREADS` |
| `SCHED`, `RAMDISK`, `PIPES`, `PENDING_SPAWN` | not `THREADS` | yes when called from preemptable code |
| Shootdown handler | **no lock** | runs at IPI, and from the relax step of every `sync::Mutex` spin via `service_pending()` so an IF=0 lock waiter still acks; the initiator services other CPUs' requests while it waits and holds no lock across the broadcast |

Init order (`galexy-os` `main`): serial → framebuffer → ramdisk publish
→ `mm` (frames, paging, heap) → `arch` (GDT, IDT, ACPI, FSGSBASE,
LAPIC, SMP) → `sched::init` (`galfs::init`) → `init` / shells.

Shutdown order (`Syscall::Power`): `galfs::sync`, `wipe_volume_key`,
then platform power. A platform that stays up has already dropped the
key.

### Steal and reap

1. A thread is current on at most one CPU.
2. `owner` is the CPU that rotates it and the CPU that may `reap` it.
3. Steal flips `owner` only when the victim is not current and its
   switch-out tail has stored `CTX_STABLE`.
4. Reap runs on the owner, sees `EXITED` or a finished kernel thread,
   checks the canary, zeros stacks, and frees frames.
5. `test-smpstress` is the hammer: churn on both CPUs, then
   `free_frames` returns to the post-grow baseline.

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

`scripts/review-smoke.sh` runs the host suites, `audit_strings`, and
eleven boots that touch every axis (BIOS + UEFI, SMP, auth, sealed disk
persist + recover, hostile ELF, negative suite, capacity, typing e2e);
`--full` runs the whole suite. CI (`.github/workflows/ci.yml`) always
runs the whole suite — README → CI maps each matrix axis to its boots.

### Test seams (feature-gated; off in the default image)

Every switch that changes kernel or userspace behaviour for a test. A
new seam is added to this table in the same PR (`STYLE.md` →
Production vs test builds). The default `cargo run` image has none of
them on; `audit_strings` and the e2e boots run against that image.

| Seam | Kind | Where | What it changes | Who turns it on |
| --- | --- | --- | --- | --- |
| `crash-seam` | Cargo feature on `shell` | `userspace/shell` | adds the `crash` command (commit `keep`, start a second mutate, get killed) | `runner/build.rs` builds a second ramdisk (`ramdisk-crash.tar`) and the `galexy-os-crashseam-*` image for `crash_injection_picks_consistent_slot`; `default_image_has_no_crash_seam_e2e` proves the main image lacks it |
| `verbose-sched` | Cargo feature on `galexy-os` | `sched/iowait.rs` | prints an idle-steal trace line per steal | nobody in the suite; a developer flag. The one-line `[sched] reap` count is unconditional because the suite reads it |
| `expect_panic` | runtime registration | `galexy_os::test` | a panic exits QEMU with `Success` instead of `Failed` | `bin/test-should-panic`, `bin/test-memory` (allocator exhaustion) |
| Inline `login user pass` | shell command form | `userspace/shell` | password on the command line (no masked prompt) | scripted typing e2e; production UX is the masked prompt (`AUTH.md`) |
| `GALEXY_GALFS_IMG`, `GALEXY_GALFS_IDE`, `GALEXY_ACCEL` | runner env | `runner/src/main.rs`, `runner/tests/common` | disk path / IDE-slave attach / `kvm` or `tcg` accel for `cargo run` and the suite | the developer; CI logs `[runner] accel=` |
| `OVMF_FD` | runner env | `runner` | UEFI firmware path | CI (`/usr/share/ovmf/OVMF.fd`), developers on non-Arch distros |
| `pub fn test_*` seams | public kernel functions | `keyboard::test_inject` / `test_drain`; `screen::test_cursor_bar_lit` / `_clear` / `fill_shown_for_bench` / `repaint_shown`; `sched::test_push_token` / `test_revoke_token` / `test_auth_flags` / `test_backdate_input` / `test_poll_idle_all` / `test_set_idle_limit`; `ramdisk::measure` | expose or poke internals a test kernel asserts on; never called by the main kernel | `bin/test-audit`, `test-galfs`, `test-cards`, `test-idle`, `test-mustchange`, `test-ramdisk`, `test-bench` |

Not seams: `ramdisk-gxld.tar` is the same userspace linked by `gxld`
instead of `rust-lld` — a build axis, not a behaviour switch.
`pc-speaker` is default-on (the beep); `--no-default-features` silences
it. The suite does not toggle it.

### Coverage: which milestone each test kernel guards

One row per `crates/galexy-os/src/bin/test-*.rs` (78). The boot test is
the `runner/tests/boot.rs` function that boots it; the milestone is the
one whose promise breaks first if the kernel goes red. Typing e2e boots
(`shell_*_typing_e2e`, `gxld_image_*`, `util_typing_e2e_on`) run the
main image and are listed at the end.

| Test kernel | Boot test | Guards |
| --- | --- | --- |
| `test-basic` | `test_kernel_runs_and_passes` | M5 harness; the full stack boots and exits via `isa-debug-exit` |
| `test-should-panic` | `should_panic_kernel_exits_successfully` | M5 `expect_panic` seam |
| `test-memory` | `memory_test_passes` | M6 frame allocator |
| `test-paging` | `paging_test_passes` | M7 paging |
| `test-heap`, `test-heapgrow` | `heap_test_passes`, `heap_grow_test_passes` | M8 heap, M19 grow + shootdown |
| `test-sched` | `sched_test_passes` | M9 cooperative scheduler |
| `test-preempt`, `test-threadexit`, `test-reuse` | `preempt_test_passes`, `threadexit_test_passes`, `reuse_test_passes` | M10 preemptive threads, M26 slot reuse |
| `test-screen` | `screen_test_passes` | M2 framebuffer text |
| `test-rings`, `test-userpreempt`, `test-syscall`, `test-user` | `rings_test_passes`, `userpreempt_test_passes`, `syscall_test_passes`, `user_lifecycle_test_passes` | M12–M13 ring 3 and the first syscall |
| `test-freshl4`, `test-cloneroot`, `test-treechurn`, `test-userfault`, `test-wx` | `freshl4_test_passes`, `cloneroot_test_passes`, `treechurn_test_passes`, `userfault_test_passes`, `wx_test_passes` | M14 isolation, M28 clone root, M48 W^X and address-space lifecycle |
| `test-ramdisk`, `test-realprogram`, `test-open`, `test-runshell` | `ramdisk_test_passes`, `realprogram_test_passes`, `open_test_passes`, `runshell_test_passes` | M15 ELF + ramdisk, M20 files as Caps, M31 launch by name; M51 ramdisk measurement |
| `test-acpi`, `test-apic` | `acpi_test_passes`, `apic_test_passes` | M17 APIC family; M65 x2APIC / TSC-deadline follow CPUID |
| `test-smp`, `test-ipi`, `test-smpuser`, `test-smpstress` | `smp_test_passes`, `ipi_test_passes`, `smpuser_test_passes`, `smpstress_test_passes` | M18–M19 SMP, shootdown, steal |
| `test-lockgrow` | `lockgrow_test_passes` | heap growth under a held lock vs an IF=0 waiter on the other CPU (galexy.os#86); shootdown acks from the lock spin |
| `test-shutdown`, `test-reboot` | `shutdown_test_powers_off`, `reboot_test_resets` | M23 power |
| `test-audit` | `audit_console_test_passes` | M49 keyboard overflow, dmesg ring, blink |
| `test-scratch`, `test-rm`, `test-seek` | `scratch_test_passes`, `rm_test_passes`, `seek_test_passes` | M27 / M30 / M36 scratch files, remove, seek |
| `test-galfs`, `test-paths`, `test-cards` | `galfs_test_passes`, `paths_test_passes`, `cards_test_passes` | M34–M35 tokens and grant, M45 path policy and confused deputy |
| `test-blocks`, `test-quota`, `test-shares`, `test-indirect`, `test-ops`, `test-fsck` | `blocks_test_passes`, `quota_test_passes`, `shares_test_passes`, `indirect_test_passes`, `ops_test_passes`, `fsck_test_passes` | M39 / M45 galfs for real usage |
| `test-galfs-disk`, `test-galfs-part`, `test-share-disk` | `galfs_disk_persists_*`, `assert_galfs_disk_persists`, `galfs_disk_persists_partition_offset`, `share_disk_persists_across_reboot` | M38 / M46 disk-backed galfs, cache modes, virtio, partition offset |
| `test-galfs-corrupt`, `test-galfs-idempotent`, `test-crash` | `galfs_disk_recovers_*`, `galfs_disk_refuses_format_when_both_slots_corrupt`, `galfs_idempotent_after_recover`, `crash_injection_picks_consistent_slot` | M39 / M45 crash safety |
| `test-ata` | `ata_absent_returns_unsupported` | M45 ATA error propagation |
| `test-dmasplit` | `dmasplit_test_passes` | virtio-blk DMA splits at physical discontinuities (both transports); sentinel frame stays untouched |
| `test-users`, `test-mustchange`, `test-lockout`, `test-idle`, `test-unlock` | `users_test_passes`, `mustchange_test_passes`, `lockout_test_passes`, `idle_test_passes`, `unlock_test_passes` | M37 / M42 / M43 auth, M44 sealed unlock |
| `test-pipe` | `pipe_test_passes` | M36 pipes + `give`, M57 block/wake |
| `test-userheap`, `test-channel` | `userheap_test_passes`, `channel_test_passes` | M66 per-task `Map` budget and reap, `Clock`, channel send/recv and `give` |
| `test-proccap`, `test-selfcap`, `test-procgive`, `test-procbudget`, `test-capforge`, `test-orphan` | `proccap_test_passes`, `selfcap_test_passes`, `procgive_test_passes`, `procbudget_test_passes`, `capforge_test_passes`, `orphan_test_passes` | M47 process Caps and forge battery |
| `test-init`, `test-jobcap` | `init_test_passes`, `jobcap_test_passes` | M53 init orphan root, M55 job Cap / Ctrl-C |
| `test-sleep`, `test-idle` | `sleep_test_passes`, `idle_test_passes` | M56 time and deadlines, M58 policy freeze |
| `test-hellogxc` | `hellogxc_test_passes` | M61 hello via `gxc`, M69 `gxld` link |
| `test-badelf`, `test-negative` | `badelf_test_passes`, `negative_test_passes` | M51 hostile ELF oracle and negative suite; M63 loader returns `SysError` and caps image pages |
| `test-smep`, `test-smap`, `test-umip`, `test-kaslr` | `smep_test_passes`, `smap_test_passes`, `umip_test_passes`, `kaslr_kernel_base_differs_across_boots` | M63 SMEP, SMAP, UMIP, kernel KASLR (`kaslr` boots the image twice) |
| `test-soak`, `test-fairness`, `test-pathological`, `test-bench` | `soak_test_passes`, `fairness_test_passes`, `pathological_test_passes`, `bench_test_passes` | M51 soak (exact table closure per round), steal fairness under load, console-budget flood; M64 `test-bench` prints `[bench] name=… us=…` and the runner asserts KVM ceilings |
| main image | `main_kernel_boots_and_timer_ticks`, `uefi_image_boots_and_timer_ticks`, `shell_*_typing_e2e`, `shell_run_hello_typing_e2e_uefi`, `shell_tty_switch_e2e`, `util_typing_e2e_on`, `assert_passwords_masked`, `uart_console_login_e2e` | M21 / M29 / M33 / M40 / M43 / M50 / M54 seats, shell, utilities, masked prompts; `shell_password_paste_typing_e2e` (M51 pathological input), `default_image_has_no_crash_seam_e2e` (M52 default-build audit), `uart_console_login_e2e` (COM1 is the console: DEL, CR, masked password) |
| `gxld` image | `gxld_image_run_hello_typing_e2e`, `gxld_image_util_typing_e2e` | M69 linker differential |

Host suites (no QEMU): `galexy-abi` (table integrity), `galexy-core`
(`Ring`, tar, `parse_path` exhaustive sweep), `galexy-crypto`
(vectors), `galexy-galf` (slot round trip, generation monotonicity,
token algebra properties), `galfs-fsck`, `gxc`, `gxld`.

## User heap, clock, and channels (Milestone 66)

`Map` appends NX|RW|user pages 512 MiB above the task's image base, at
most 32 pages. The syscall returns the new base. Reap walks that P4
slot, so the frames return with the tree. `galexy-rt` bumps an
allocator over the region; `dealloc` does not unmap. `Clock` reads
`timer_ticks` and does not change `Sleep`. `Channel` / `Send` / `Recv`
are in `docs/PROCESS.md`: one queued message, up to 256 bytes and two
file Caps, `Recv` parks. Milestone 67 marks `Map`, `Clock`,
`Channel`, `Send`, and `Recv` stable. `spawn` stays experimental.
`SPAWN_NO_FG` (r10 bit 5) keeps a background spawn off the TTY's Ctrl-C
slot until `fg` Cap-waits it.

## Init control (Milestone 67)

Init creates one channel and keeps both ends. The kernel records that
id. A logged-in seat `send`s on reserved index `0x8008`. The kernel
prepends a 20-byte header (admin, tty, debug id, session generation),
queues it from the end init does not recv on, and parks the sender
until init `send`s the reply on its recv end. One RPC is in flight.
Pre-login is `AccessDenied`. No control channel is `Unsupported`, and
the shell then calls Power only if that grant is still installed.

A control message cancels init's sleep, or its Cap-wait when the child
is still alive. The process Cap stays in the table and the wait returns
`Interrupted`. If the child has already exited, the exit wake owns the
wait. Other tasks' Cap-waits are not cancelled this way. `wait` and
`sleep` also refuse to park when a control message is already queued,
so a send that lands while init is still running cannot hide behind a
live seat. `WAIT_POLL` lets init reap a zombie without parking on a
live seat.

Admin login drops the Power grant while init is alive.
`Grants::launcher` (power on) stays the grant for kernel test tasks and
for an admin login when init is not running. Shutdown Cap-kills seats,
syncs, then calls Power. The sender's seat is left parked so the reply
can still be delivered if Power returns.

## Process Caps (Milestone 47)

Plan: `docs/PROCESS.md`. Style: `docs/STYLE.md` → Process model and init.
Checkboxes: `TODO.md` Milestone 47.

**Why not PIDs as ABI.** A guessable global integer (`kill(pid)`,
`waitpid(pid)`) fights the rest of Galexy — files, galfs tokens, and
devices are Caps. Spawn stays spawn-not-fork. Authority is a **process
Cap** returned to the parent; listings may show a monotonic **debug id**
and a human **name**, but there is no `open_process(debug_id)` syscall.

| Concept | Role |
| --- | --- |
| Process Cap | Handle + `PROC_*` rights over a task |
| Debug id | KOID-style number for `tasks` / serial only |
| Name | Label (`shell`, `hello`); not the wait/kill key |
| Parent | Kernel parent pointer; orphans go to init (M53) |
| Zombie | Exited task kept until Cap-wait reaps |

**Rights** (`galexy-abi::CapRights`, bits 6–9):

| Right | Allows |
| --- | --- |
| `PROC_WAIT` | Block until exit; receive status; reap |
| `PROC_KILL` | Stop the task (signals-lite) |
| `PROC_TRANSFER` | `give` the Cap to another task (attenuates) |
| `PROC_INSPECT` | Read debug id / name / state |
| `PROC_PARENT` | Union granted to the spawner by default |

Attenuation on `grant`/`give` applies the same intersection rule as file
Caps. Dropping the last wait Cap without a reaper is a bug path — orphans
must land at init with wait rights once Milestone 53 lands.

**Landed (spawn Cap + self/inspect).** `spawn` returns a process Cap
(`PROC_CAP_BASE`…); `wait(cap)` / `kill(cap)` are syscalls. Shell
utilities use `SPAWN_INHERIT` + Cap-wait; bare launches drop the Cap so
zombies can reap. `SPAWN_WAIT` remains a park-until-exit convenience
(Cap still installed). Wait is by Cap/slot generation, not by name.
`SELF_INDEX` is an inspect Cap (`read` → `id=… name=… state=…`);
`getpid` is not the API. `tasks` / `threads` listings show debug ids;
shell `echo $?` prints the last Cap-wait exit status. Args remain a
single blob until argv/env layout freezes in abi.

**ABI stability.** Process Cap `wait` / `kill` are **stable** as of
Milestone 67. `spawn` stays experimental. Numbers for `PROC_*` bits are
pinned in `galexy-abi` tests.

## Known sharp edges

- `bootloader` 0.11's builder API differs entirely from 0.9's `bootimage`;
  pin exactly in `Cargo.toml`.
- APIC discovery is MADT-based (RSDP → XSDT/RSDT walk, checksums enforced);
  no fallback to hard-coded MMIO bases — a machine without ACPI tables
  fails loudly rather than guessing.
- LAPIC calibration measures a 10 ms HPET window (PIT channel 2 if
  the HPET is absent) and accepts CPUID 0x15 / 0x16 only when that
  nominal rate is within 2× of the measurement. TCG often advertises a
  crystal the emulated timer does not run; the measured window wins and
  the disagreement is logged. TSC-deadline mode is used when CPUID.1
  ECX bit 24 is set. QEMU TCG still does not enumerate that bit, so the
  suite observes one-shot mode. `bin/test-apic` checks the LVT against
  the same bit. `IDLE_MAX_MS` and the quantum are unchanged.
- x2APIC is enabled (IA32_APIC_BASE EN, then EN|EXTD) when CPUID.1 ECX
  bit 21 is set; otherwise the guest stays on xAPIC MMIO and says so.
  `-cpu max,+x2apic` requests the bit. QEMU TCG through 8.2 drops it
  ("TCG doesn't support requested feature"); QEMU 9+ TCG keeps it.
  `bin/test-apic` asserts the MSR path when the bit is present and the
  MMIO path when it is not.
- Default machine is `-M q35`: ECAM, virtio 1.x, MSI-X, virtio-input.
  Named fallbacks, each with one regression boot: 8259 remap when the
  FADT says the pair is present (mask-only when it does not), PS/2
  i8042 (`shell_ps2_typing_e2e`), PIO IDE on `-M pc`
  (`galfs_disk_persists_across_reboot`, log `config via 0xCF8`), legacy
  virtio I/O BAR (`galfs_disk_persists_virtio_legacy`, log
  `legacy IO BAR`). The PC speaker (`pc-speaker`, default on) is the
  only audio device and the only PIT user after boot.
- Stated non-goals for this platform pass: 5-level paging, huge user
  pages, USB, a GPU beyond the GOP framebuffer, and a network stack
  (Phase 10).
- The measured path is `cargo run --release` and
  `cargo test -p runner --test boot --release`. The runner picks
  `-accel kvm -cpu host` when `/dev/kvm` is writable, else
  `-accel tcg -cpu max,+x2apic` (`GALEXY_ACCEL` overrides). Numbers live in
  `docs/PERF.md`. PCID and a second heap allocator stay waived.
- `-no-reboot` is always passed to QEMU so triple faults surface as an exit
  instead of an infinite reboot loop. A `reboot` request still pulses the
  reset line; QEMU then exits rather than restarting the guest. `shutdown`
  powers the VM off (process exit 0, not the isa-debug-exit code).
- Fresh artifacts can live in *multiple* `OUT_DIR` hash dirs; pick images by
  mtime (`ls -t`) when testing manually.
