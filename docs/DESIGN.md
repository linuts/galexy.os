# DESIGN — galexy.os

How the pieces fit. This file explains the *why*; `STYLE.md` explains the
*how it's written*.

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

### screen — "the screen"

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

### serial — "the side channel"

`uart_16550` at COM1. Used for panics, boot info, and debug output. **Rule:**
nothing user-facing ever prints here; it's invisible to the OS user by
design. The serial writer shares no lock with the screen, so interrupt
handlers can log through it safely.

### keyboard — "the input decoder"

IRQ1 handler → `pc_keyboard` (US layout, scancode set 1) → Unicode chars
pushed into a fixed ring buffer. Consumers ask for characters via
`keyboard::pop_key()`:

```rust
pub fn add_scancode(scancode: u8)   // called from the IRQ handler only
pub fn pop_key() -> Option<char>    // drains decoded input
```

Locks are tiny and never nested (decode under one lock, push under another),
so IRQ context is safe. Queue overflow drops the newest key (documented).

### interrupts — "the plumbing"

Init order: GDT/TSS → IDT → PICs → timer config → `sti`.

- GDT + TSS: IST slot 0 for double fault; **all segment registers (including
  `ss`, `ds`) are reloaded after `lgdt`** (bootloader migration warning).
- IDT: breakpoint, page fault (reports + parks), double fault (own IST
  stack), timer (IRQ0), keyboard (IRQ1).
- PICs remapped to vectors 32..47 via `pic8259`.
- Timer: PIT channel 0 at ~1 kHz; handler increments an `AtomicU64` and
  heartbeats over serial once per second. **Scheduler phase swaps this
  handler body, nothing else changes.**

### echo — "the shell"

Main loop: `echo::poll()` drains the key queue — printable chars echo to the
screen and buffer up; Enter flushes the line as `echo: <text>` and prints a
fresh `galexy> ` prompt; Backspace erases via `screen::backspace()`. Lines
are fixed 128-char buffers until `alloc` lands. Later, the shell becomes a
task the scheduler runs.

## Concurrency model (pre-scheduler, single-core)

- All shared state sits behind `spin::Mutex` (plus `LazyLock` for init-once
  statics and `AtomicU64` for tick counts).
- Print lock: `println!` → screen lock. Current mitigation for the
  hold-lock-while-interrupted hazard: interrupt handlers never touch the
  screen lock (they use serial or atomics only). A "lock contention audit"
  is scheduled before preemption lands.

## Testing strategy

- `scripts/boot-test.sh`: headless QEMU, injects HMP commands (`sendkey`,
  `screendump`), captures COM1 via `-serial stdio`.
- Kernel `#[test_case]` harness is planned (see TODO) before memory work.
- Pure logic (scancode decode, ring buffer) should get in-kernel tests once
  the harness exists.

## Known sharp edges

- `bootloader` 0.11's builder API differs entirely from 0.9's `bootimage`;
  pin exactly in `Cargo.toml`.
- UEFI boot: legacy PIC doesn't exist → timer/keyboard need APIC work before
  they work there.
- `-no-reboot` is always passed to QEMU so triple faults surface as an exit
  instead of an infinite reboot loop.
- Fresh artifacts can live in *multiple* `OUT_DIR` hash dirs; pick images by
  mtime (`ls -t`) when testing manually.
