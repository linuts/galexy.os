//! The runner host crate: builds disk images (via build.rs) and boots the
//! normal kernel in QEMU (`cargo run`). Boot tests live in `tests/`.

/// Looks up `name`'s image path in the `GALEXY_IMAGES` manifest
/// (`name:bios_path,uefi_path` entries); `kind` selects bios/uefi.
fn image_path(name: &str, kind: &str) -> String {
    let slot = match kind {
        "bios" => 0,
        "uefi" => 1,
        _ => panic!("unknown image kind {kind}"),
    };
    env!("GALEXY_IMAGES")
        .split(';')
        .find_map(|entry| {
            let (entry_name, rest) = entry.split_once(':')?;
            if entry_name != name {
                return None;
            }
            rest.split(',').nth(slot).map(|p| p.to_string())
        })
        .unwrap_or_else(|| panic!("no {name} {kind} image built"))
}

fn main() {
    let uefi = std::env::args().any(|arg| arg == "--uefi");
    let img_path = if uefi {
        image_path("galexy-os", "uefi")
    } else {
        image_path("galexy-os", "bios")
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
