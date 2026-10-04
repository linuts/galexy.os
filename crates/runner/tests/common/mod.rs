//! Boot-test helper: boots a galexy.os kernel image in headless QEMU and
//! returns (exit code, serial output).

use std::io::Read;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// Parsed entry from the `GALEXY_IMAGES` manifest emitted by build.rs.
pub struct Image {
    /// Kernel binary name (e.g. `galexy-os`, `test-basic`).
    pub name: String,
    /// BIOS boot image path.
    pub bios: String,
    /// UEFI boot image path.
    pub uefi: String,
}

/// Parses the build-script manifest into image entries.
pub fn images() -> Vec<Image> {
    env!("GALEXY_IMAGES")
        .split(';')
        .filter(|entry| !entry.is_empty())
        .map(|entry| {
            let (name, rest) = entry.split_once(':').expect("manifest entry has ':'");
            let (bios, uefi) = rest.split_once(',').expect("manifest has bios,uefi");
            Image {
                name: name.to_string(),
                bios: bios.to_string(),
                uefi: uefi.to_string(),
            }
        })
        .collect()
}

/// Finds the image for `name`, panicking the test if absent.
pub fn image(name: &str) -> Image {
    images()
        .into_iter()
        .find(|img| img.name == name)
        .unwrap_or_else(|| panic!("no image built for kernel binary '{name}'"))
}

/// Exit code the `isa-debug-exit` device produces for `QemuExitCode::Success`.
pub const QEMU_EXIT_SUCCESS: i32 = 33;

/// How long a kernel may run before it is treated as hung.
const TEST_TIMEOUT: Duration = Duration::from_secs(60);

/// Unique per-call serial log file path.
fn serial_log_path(name: &str) -> PathBuf {
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    std::env::temp_dir().join(format!("galexy-serial-{}-{n}.log", name.replace('-', "_")))
}

/// Builds the QEMU command for `img_path`: headless, COM1 to `serial_path`,
/// writable-overlays (`-snapshot`) so parallel tests never conflict.
fn qemu_command(img_path: &str, serial_path: &PathBuf) -> Command {
    let mut cmd = Command::new("qemu-system-x86_64");
    cmd.arg("-drive")
        .arg(format!("format=raw,file={img_path}"))
        .arg("-snapshot")
        .arg("-display")
        .arg("none")
        .arg("-no-reboot")
        // COM1 -> log file, polled by the test (no blocking pipe reads).
        .arg("-serial")
        .arg(format!("file:{}", serial_path.display()))
        // Exit device: guest writes to port 0xF4 end QEMU with that code.
        .arg("-device")
        .arg("isa-debug-exit,iobase=0xf4,iosize=0x04")
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    cmd
}

/// Boots `image` headless until it exits or the timeout elapses.
///
/// Returns the QEMU exit code (see [`QEMU_EXIT_SUCCESS`] / [`QEMU_EXIT_FAILED`];
/// `None` means timeout/kill) and everything the guest wrote to COM1.
pub fn boot(image: &Image) -> (Option<i32>, String) {
    let serial_path = serial_log_path(&image.name);
    let mut child = qemu_command(&image.bios, &serial_path)
        .spawn()
        .expect("failed to launch qemu-system-x86_64 (install qemu-desktop)");

    let deadline = Instant::now() + TEST_TIMEOUT;
    let code = loop {
        match child.try_wait().expect("try_wait failed") {
            Some(status) => break status.code(),
            None if Instant::now() > deadline => {
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
            None => std::thread::sleep(Duration::from_millis(100)),
        }
    };
    // QEMU's own errors (for diagnosing startup failures in assertions).
    let mut qemu_err = String::new();
    if let Some(mut stderr) = child.stderr.take() {
        let _ = stderr.read_to_string(&mut qemu_err);
    }

    let serial = std::fs::read_to_string(&serial_path).unwrap_or_default();
    let serial = if serial.is_empty() && code.is_none() {
        format!("(qemu stderr: {qemu_err})")
    } else {
        serial
    };
    (code, serial)
}

/// Boots `image` under UEFI (OVMF) and kills the guest after `timeout`.
/// Returns the serial output.
pub fn boot_uefi(image: &Image, timeout: Duration) -> String {
    const OVMF_FD_DEFAULT: &str = "/usr/share/ovmf/x64/OVMF.4m.fd";
    let ovmf_fd = std::env::var("OVMF_FD").unwrap_or_else(|_| OVMF_FD_DEFAULT.into());

    let serial_path = serial_log_path(&image.name);
    let mut cmd = qemu_command(&image.uefi, &serial_path);
    cmd.arg("-bios").arg(ovmf_fd);
    let mut child = cmd.spawn().expect("failed to launch qemu-system-x86_64");
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if child.try_wait().expect("try_wait failed").is_some() {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let _ = child.kill();
    let _ = child.wait();

    std::fs::read_to_string(&serial_path).unwrap_or_default()
}

/// Boots `image` and kills the guest after `timeout` instead of failing —
/// for liveness checks of the interactive kernel (it never exits on its own).
pub fn boot_liveness(image: &Image, timeout: Duration) -> String {
    let serial_path = serial_log_path(&image.name);
    let mut child = qemu_command(&image.bios, &serial_path)
        .spawn()
        .expect("failed to launch qemu-system-x86_64");
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if child.try_wait().expect("try_wait failed").is_some() {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let _ = child.kill();
    let _ = child.wait();

    std::fs::read_to_string(&serial_path).unwrap_or_default()
}
