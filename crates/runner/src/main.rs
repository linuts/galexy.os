//! Host-side runner: builds the disk images (via build.rs) and boots them in
//! QEMU. `cargo run` boots the BIOS image; `cargo run -- --uefi` boots UEFI.

use std::env;
use std::process::{exit, Command};

const OVMF_FD_DEFAULT: &str = "/usr/share/ovmf/x64/OVMF.4m.fd";

fn main() {
    let uefi = env::args().any(|arg| arg == "--uefi");
    let image = if uefi {
        env!("UEFI_PATH")
    } else {
        env!("BIOS_PATH")
    };

    let mut cmd = Command::new("qemu-system-x86_64");
    if uefi {
        let ovmf_fd = env::var("OVMF_FD").unwrap_or_else(|_| OVMF_FD_DEFAULT.into());
        cmd.arg("-bios").arg(ovmf_fd);
    }
    cmd.arg("-drive").arg(format!("format=raw,file={image}"));
    // Surface guest COM1 on the host terminal for debugging.
    cmd.arg("-serial").arg("stdio");
    // Make triple faults visible instead of silently rebooting.
    cmd.arg("-no-reboot");
    // Exit device for automated tests (io port 0xF4 writes end QEMU with a code).
    cmd.arg("-device")
        .arg("isa-debug-exit,iobase=0xf4,iosize=0x04");

    let status = cmd.status().expect(
        "failed to launch qemu-system-x86_64 (install qemu-desktop: pacman -S qemu-desktop)",
    );
    if !status.success() {
        exit(status.code().unwrap_or(1));
    }
}
