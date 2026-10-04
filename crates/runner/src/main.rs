//! The runner host crate: builds disk images (via build.rs) and boots the
//! normal kernel in QEMU (`cargo run`). Boot tests live in `tests/`.

fn main() {
    let uefi = std::env::args().any(|arg| arg == "--uefi");
    let img_path = if uefi {
        // Manifest entries are `name:bios_path,uefi_path`.
        env!("GALEXY_IMAGES")
            .split(';')
            .find_map(|entry| {
                let (name, rest) = entry.split_once(':')?;
                if name != "galexy-os" {
                    return None;
                }
                rest.split_once(',').map(|(_, uefi)| uefi.to_string())
            })
            .expect("no galexy-os uefi image built")
    } else {
        env!("GALEXY_IMAGES")
            .split(';')
            .find_map(|entry| {
                let (name, rest) = entry.split_once(':')?;
                if name != "galexy-os" {
                    return None;
                }
                rest.split_once(',').map(|(bios, _)| bios.to_string())
            })
            .expect("no galexy-os bios image built")
    };

    let mut cmd = std::process::Command::new("qemu-system-x86_64");
    if uefi {
        const OVMF_FD_DEFAULT: &str = "/usr/share/ovmf/x64/OVMF.4m.fd";
        let ovmf_fd = std::env::var("OVMF_FD").unwrap_or_else(|_| OVMF_FD_DEFAULT.to_string());
        cmd.arg("-bios").arg(ovmf_fd);
    }
    cmd.arg("-drive").arg(format!("format=raw,file={img_path}"));
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
        std::process::exit(status.code().unwrap_or(1));
    }
}
