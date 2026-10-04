# galexy.os

A small, modular operating system written in Rust. Bootable on BIOS and UEFI,
with a pixel-framebuffer TTY, PS/2 keyboard input, memory management,
cooperative tasks and timer-preemptive kernel threads — currently a shell
with live system stats, growing toward user space and beyond.

## Features

- [x] Bootable image (BIOS + UEFI) via the `bootloader` crate
- [x] Pixel-framebuffer text output (bootloader v0.11 flow; no legacy VGA text
      mode) with scrolling and colors
- [x] Serial port logging (for debugging, never on screen)
- [x] Interrupts: GDT, IDT, remapped PICs, ~1 kHz PIT timer tick
- [x] PS/2 keyboard input with scancode translation
- [x] Shell: line editing (Backspace), commands (`help`, `stats`, `threads`,
      `tasks`, `clear`, `about`), echo fallback for unknown lines
- [x] Physical frame allocator over the bootloader memory map
- [x] Paging: map/unmap pages with TLB flushes, page-fault reporting (CR2),
      fresh page-table trees (per-task isolation groundwork)
- [x] Kernel heap (`alloc`): String/Vec/Box work everywhere; grows on
      demand past the initial 400 KiB
- [x] Cooperative round-robin tasks + timer-preemptive kernel threads with
      a real lifecycle: exited threads are reaped (stacks return to the
      heap), slots are stable tombstones, stack canary surfaces overflows
- [x] Ring-3 groundwork: GDT user segments, TSS.RSP0 control, frame CPL
      introspection (no userland yet)
- [x] `galexy-abi`: the syscall ABI — numbered syscalls + capability model
      (no fds; capabilities day one) — frozen and host-tested before any
      ring-3 code exists
- [x] Shell with commands (`help`, `stats`, `threads`, ...) + live status
      bar ("quiet OS" demo)
- [x] Test harness: host unit tests + per-kernel QEMU integration tests
- [ ] User space

## Quick start

### Requirements

- Rust nightly (pinned by `rust-toolchain.toml`; `rustup toolchain install`
  picks it up, including `rust-src`, `llvm-tools-preview` and the
  `x86_64-unknown-none`/`x86_64-unknown-uefi` targets)
- QEMU + OVMF: `sudo pacman -S --needed qemu-desktop ovmf`
- Linux host

### Run in QEMU (BIOS, default)

```sh
cargo run
```

The kernel's COM1 output appears on the host terminal (`-serial stdio`).

### Run in QEMU (UEFI)

```sh
cargo run -- --uefi
```

Uses the system OVMF firmware (`/usr/share/ovmf/x64/OVMF.4m.fd`); override
with `OVMF_FD=/path/to/OVMF.fd cargo run -- --uefi`.

Note: under UEFI the timer/keyboard are not wired up yet (needs APIC, see
`TODO.md`); the kernel boots and reports over serial.

### Tests

```sh
cargo test -p galexy-core   # host unit tests of kernel primitives (instant)
cargo test -p galexy-abi    # syscall-ABI stability + capability-model tests
cargo test -p runner        # boots every kernel binary in headless QEMU
```

Test kernels are regular binaries under `crates/galexy-os/src/bin/`; the
runner builds one disk image per binary and asserts exit codes + serial
output. Panics in test kernels automatically fail the run.

### Build the bootable images only

```sh
cargo build --release
find target -name "galexy-os-*.img"
```

The BIOS image can be written straight to a USB stick and booted on real
hardware:

```sh
sudo dd if=<galexy-os-bios.img> of=/dev/sdX bs=1M status=progress
```

## Project layout

```
├── Cargo.toml                       # workspace
├── rust-toolchain.toml              # pinned nightly + components/targets
├── scripts/                         # headless boot-test tooling
├── docs/
│   ├── STYLE.md                     # code style & conventions
│   ├── ROADMAP.md                   # where this is going
│   └── DESIGN.md                    # how the pieces fit
├── crates/
│   ├── galexy-abi/                  # THE syscall ABI: numbers, capability
│   │                                #   model, error codes (kernel<->user
│   │                                #   contract, host-testable)
│   ├── galexy-os/                   # the kernel: lib + bins
│   │   └── src/
│   │       ├── lib.rs               # shared init, panic handler, exit_qemu
│   │       ├── main.rs              # normal kernel: wiring + main loop
│   │       ├── bin/                 # test kernels (one per QEMU test)
│   │       ├── shell.rs             # the shell: commands + status bar
│   │       ├── banner.rs            # boot feature showcase
│   │       ├── arch/                # the port wall: GDT/TSS, IDT, PICs, PIT, mm
│   │       ├── drivers/             # screen, serial, keyboard
│   │       ├── macros.rs            # print!/println! plumbing
│   │       └── sched/               # tasks, preemptive threads, context asm,
│   │                                #   syscall dispatch table
│   ├── galexy-core/                 # kernel primitives (Ring, Bitmap), host-testable
│   ├── userspace/                   # ring-3 programs later (see DESIGN rule 6/8)
│   └── runner/                      # host crate: images, QEMU, boot tests
│       ├── build.rs                 # bootloader image builder (per kernel bin)
│       ├── src/main.rs              # QEMU invocation (--uefi flag)
│       └── tests/boot.rs            # boots every kernel binary headless
```

## Design principles

1. **Every module is swappable.** The screen exposes terminal-shaped calls
   (`out_char`, `out_str`, `clear_screen`) — pixel-framebuffer today, and the
   same surface can sit in front of any renderer tomorrow.
2. **No singletons beyond explicit statics with `spin` primitives** — locks
   are the concurrency story, so a scheduler can rely on them.
3. **Timer ticks are sacred.** The PIT fires ~1 kHz; the handler body is the
   only thing a scheduler later has to swap.
4. **Test what can be tested.** Kernel unit tests are planned via
   `#[test_case]` runnable in QEMU; meanwhile `scripts/boot-test.sh` gives
   repeatable headless boot verification.

## References

- [Writing an OS in Rust](https://os.phil-opp.com/) (Philipp Oppermann) — the
  foundation this project's structure is based on
- [bootloader crate](https://github.com/rust-osdev/bootloader)
- [rust-osdev](https://github.com/rust-osdev) — `x86_64`, `pic8259`,
  `pc-keyboard`, `uart_16550`
