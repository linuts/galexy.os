# galexy.os

A small, modular operating system written in Rust. Bootable on BIOS and UEFI,
running on **two CPUs**, with a pixel-framebuffer TTY, PS/2 keyboard input,
memory management, cooperative tasks, timer-preemptive kernel threads, and a
real userland: ring-3 tasks with their own address spaces (running on either
CPU), actual Rust programs loaded as ELF from a tar ramdisk, printing through
the syscall ABI, killed cleanly when they crash — the OS survives user bugs.
The interactive shell launches userland programs by name (`run hello`).

## Features

- [x] Bootable image (BIOS + UEFI) via the `bootloader` crate
- [x] Pixel-framebuffer text output (bootloader v0.11 flow; no legacy VGA text
      mode) with scrolling and colors
- [x] Serial port logging (for debugging, never on screen)
- [x] Interrupts: GDT, IDT, ACPI/MADT-discovered APIC stack — per-CPU LAPIC
      timers (PIT-calibrated, share-split to keep ~1 kHz), I/O APIC
      keyboard route; legacy PICs kept quiet (masked)
- [x] SMP: two CPUs run the kernel — per-CPU GS/GDT/TSS, AP
      trampoline bring-up, pinned-at-spawn scheduler with per-CPU
      rotation + owner-reaping; idle CPUs work-steal (owner flips once the
      victim has left that stack, entry on the next tick); kernel-half remaps are
      mechanized with precise-INVLPG shootdown IPIs (vector 0xF8, lock-free
      handler, mailbox pool); every boot test runs at `-smp 2`
- [x] PS/2 keyboard input with scancode translation
- [x] Shell: line editing (Backspace), commands (`help`, `stats`, `threads`,
      `tasks`, `run <program>`, `clear`, `about`), `command not found` for
      unknown lines
- [x] Physical frame allocator over the bootloader memory map
- [x] Paging: map/unmap pages with TLB flushes, page-fault reporting (CR2),
      fresh page-table trees (per-task isolation groundwork)
- [x] Kernel heap (`alloc`): String/Vec/Box work everywhere; grows on
      demand past the initial 400 KiB
- [x] Cooperative round-robin tasks + timer-preemptive kernel threads with
      a real lifecycle: exited threads are reaped (stacks return to the
      heap), slots are stable tombstones, stack canary surfaces overflows
- [x] User space: ring-3 tasks in the same rotation, SYSCALL/SYSRET
      (`exit`, `yield`, `write`, `cap_info`, `open`, `read`, `close`),
      capability authority
      kernel-side, per-task kernel stacks via TSS.RSP0 — and REAL
      isolation: per-task address spaces (FreshL4), CR3 swapped by the
      rotation, whole trees walked back on reap, guard-page fences, and
      ring-3 crashes killing only the faulting task
- [x] `galexy-abi`: the syscall ABI — numbered syscalls + capability model
      (no fds; capabilities day one) — frozen and host-tested before any
      ring-3 code exists. `open`/`read`/`close` turn ramdisk files into
      per-task capabilities
- [x] Shell with commands (`help`, `stats`, `threads`, `run <program>`,
      ...) + live status bar ("quiet OS" demo) — `run` launches real
      userland programs from the ramdisk; console output = screen + serial
- [x] Test harness: host unit tests + per-kernel QEMU integration tests
- [x] Programs beyond blobs: `galexy-rt` runtime (`entry!`, syscall
      wrappers, user panic handler), kernel ELF loader (static ET_EXEC,
      per-segment flags, strict same-P4-entry policy), tar ramdisk packed
      by the runner, and `hello` — a real Rust user program — running a
      full lifecycle through the loader

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

Note: UEFI is a first-class boot path — the timer (LAPIC) and keyboard
(I/O APIC) work identically under both firmware types since the APIC work
(Milestone 17, see `TODO.md`/`DESIGN.md`).

### Tests

```sh
cargo test -p galexy-core   # host unit tests of kernel primitives (instant)
cargo test -p galexy-abi    # syscall-ABI stability + capability-model tests
cargo test -p runner        # boots every kernel binary in headless QEMU
```

Test kernels are regular binaries under `crates/galexy-os/src/bin/`; the
runner builds one disk image per binary, boots at `-smp 2` (BIOS and, for
the UEFI cases, OVMF), and asserts exit codes + serial output. Panics in
test kernels automatically fail the run.

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
│   │       ├── arch/                # the port wall: per-CPU GS/GDT/TSS,
│   │       │                        #   APIC + I/O APIC (per-CPU LAPIC
│   │       │                        #   timers, keyboard route), AP
│   │       │                        #   trampoline, shootdown IPIs, ACPI,
│   │       │                        #   legacy PICs (masked), PIT, mm
│   │       ├── drivers/             # screen, serial, keyboard
│   │       ├── macros.rs            # print!/println! plumbing
│   │       └── sched/               # tasks, preemptive threads, context asm,
│   │                                #   syscall dispatch table
│   ├── galexy-core/                 # kernel primitives (Ring, Bitmap,
│   │                                #   TarCursor), alloc-free + host-testable
│   ├── userspace/                     # ring-3 programs: galexy-rt (the
│   │                                  #   runtime: entry!, syscall wrappers,
│   │                                  #   panic handler) + hello (the first
│   │                                  #   real Rust user program)
│   └── runner/                        # host crate: images, QEMU, boot tests
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
3. **The per-CPU LAPIC timer is sacred.** Calibrated once at boot (share-split
   across CPUs to keep ~1 kHz machine-wide); the handler body is the only
   thing a scheduler ever has to swap.
4. **Test what can be tested.** Host unit tests for the pure primitives
   (`galexy-core`, `galexy-abi`), plus one bootable kernel binary per
   integration test — 31 QEMU boots (all at `-smp 2`) assert exit codes and
   serial output on every run.
5. **Ownership beats locks for per-CPU state.** Rotation cursors, TSS.RSP0,
   syscall scratch and the LAPIC registers are touched by exactly one CPU
   each; only the thread table is a shared lock.

## References

- [Writing an OS in Rust](https://os.phil-opp.com/) (Philipp Oppermann) — the
  foundation this project's structure is based on
- [bootloader crate](https://github.com/rust-osdev/bootloader)
- [rust-osdev](https://github.com/rust-osdev) — `x86_64`, `pic8259`,
  `pc-keyboard`, `uart_16550`
