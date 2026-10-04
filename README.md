# galexy.os

A small, modular operating system written in Rust. Bootable on BIOS and UEFI,
with a basic VGA text-mode TTY and PS/2 keyboard input — currently an echo
shell, growing toward scheduling and beyond.

## Features

- [x] Bootable image (BIOS + UEFI) via the `bootloader` crate
- [x] Pixel-framebuffer text output (bootloader v0.11 flow; no legacy VGA text
      mode) with scrolling and colors
- [x] Serial port logging (for debugging, never on screen)
- [x] Interrupts: GDT, IDT, remapped PICs, ~1 kHz PIT timer tick
- [x] PS/2 keyboard input with scancode translation
- [x] Echo shell: type a line, Enter echoes it back, Backspace edits
- [ ] Physical memory manager
- [ ] Paging / virtual memory
- [ ] Preemptive scheduler
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

### Headless automated test

`scripts/boot-test.sh` boots an image headless, feeds monitor commands from
stdin (e.g. `sendkey ...`), captures the serial log in
`/tmp/opencode/serial.log` and supports `screendump` for framebuffer checks.

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
│   ├── galexy-os/                   # the kernel (bin)
│   │   └── src/
│   │       ├── main.rs              # wiring only: init order + main loop
│   │       ├── echo.rs              # the echo "shell"
│   │       ├── kcore/               # kernel primitives (ring buffers, ...)
│   │       ├── arch/                # the port wall: GDT/TSS, IDT, PICs, PIT
│   │       ├── drivers/             # screen, serial, keyboard
│   │       ├── macros.rs            # print!/println! plumbing
│   │       └── sched/               # scheduler (planned; hook point documented)
│   ├── userspace/                   # ring-3 programs later (planned)
│   └── runner/                      # host crate: builds disk images, runs QEMU
│       ├── build.rs                 # bootloader BIOS+UEFI image builder
│       └── src/main.rs              # QEMU invocation (--uefi flag)
```

Layer rules (drivers only talk to `arch` + `kcore`; `arch` is the only place
ports are touched; userspace programs are separate crates) live in
`docs/DESIGN.md`.

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
