//! Boot-test helper: boots a galexy.os kernel image in headless QEMU and
//! returns (exit code, serial output).

use std::io::Read;
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
/// Exit code the `isa-debug-exit` device produces for `QemuExitCode::Failed`.
pub const QEMU_EXIT_FAILED: i32 = 35;

/// How long a kernel may run before it is failed as hung.
const TEST_TIMEOUT: Duration = Duration::from_secs(60);

fn qemu_command(image: &Image) -> Command {
    let mut cmd = Command::new("qemu-system-x86_64");
    cmd.arg("-drive")
        .arg(format!("format=raw,file={}", image.bios))
        .arg("-display")
        .arg("none")
        .arg("-no-reboot")
        // COM1 -> our stdout so the guest's serial output lands in the pipe.
        .arg("-serial")
        .arg("stdio")
        // Exit device: guest writes to port 0xF4 end QEMU with that code.
        .arg("-device")
        .arg("isa-debug-exit,iobase=0xf4,iosize=0x04")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    cmd
}

/// Drains `stdout` to EOF after the guest exited.
fn drain(stdout: &mut std::process::ChildStdout, output: &mut Vec<u8>) {
    let _ = stdout.read_to_end(output);
}

/// Boots `image` headless until it exits or the timeout elapses.
///
/// Returns the QEMU exit code (see [`QEMU_EXIT_SUCCESS`] / [`QEMU_EXIT_FAILED`])
/// and everything the guest wrote to COM1. `None` code means timeout/kill.
pub fn boot(image: &Image) -> (Option<i32>, String) {
    let mut child = qemu_command(image)
        .spawn()
        .expect("failed to launch qemu-system-x86_64 (install qemu-desktop)");

    let deadline = Instant::now() + TEST_TIMEOUT;
    let mut output = Vec::new();
    let mut stdout = child.stdout.take().expect("stdout piped");

    loop {
        let mut buf = [0u8; 4096];
        match stdout.read(&mut buf) {
            Ok(n) if n > 0 => output.extend_from_slice(&buf[..n]),
            _ => {}
        }

        if let Some(status) = child.try_wait().expect("try_wait failed") {
            drain(&mut stdout, &mut output);
            return (status.code(), String::from_utf8_lossy(&output).into_owned());
        }

        if Instant::now() > deadline {
            let _ = child.kill();
            let _ = child.wait();
            return (None, String::from_utf8_lossy(&output).into_owned());
        }

        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Boots `image` and kills the guest after `timeout` instead of failing —
/// for liveness checks of the interactive kernel (it never exits on its own).
pub fn boot_liveness(image: &Image, timeout: Duration) -> String {
    let mut cmd = qemu_command(image);
    cmd.stderr(Stdio::piped());
    let mut child = cmd.spawn().expect("failed to launch qemu-system-x86_64");
    let deadline = Instant::now() + timeout;
    let mut output = Vec::new();
    let mut stdout = child.stdout.take().expect("stdout piped");

    while Instant::now() < deadline {
        let mut buf = [0u8; 4096];
        if let Ok(n) = stdout.read(&mut buf) {
            if n > 0 {
                output.extend_from_slice(&buf[..n]);
            }
        }
        if child.try_wait().expect("try_wait failed").is_some() {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let _ = child.kill();
    let _ = child.wait();
    String::from_utf8_lossy(&output).into_owned()
}
