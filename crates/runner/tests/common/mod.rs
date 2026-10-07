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
///
/// `-smp 2 -cpu max`: the SMP substrate requires FSGSBASE (`-cpu max`;
/// QEMU's default qemu64 model lacks it), and 2 cores exercise the per-CPU
/// paths in EVERY test — single-core assumptions regress loudly.
fn qemu_command(img_path: &str, serial_path: &PathBuf) -> Command {
    let mut cmd = Command::new("qemu-system-x86_64");
    cmd.arg("-drive")
        .arg(format!("format=raw,file={img_path}"))
        .arg("-snapshot")
        .arg("-smp")
        .arg("2")
        .arg("-cpu")
        .arg("max")
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

/// Like [`qemu_command`], but the boot drive uses a per-drive snapshot and
/// `galfs_path` is attached as the primary IDE slave without a snapshot so
/// writes persist for a second boot of the same image.
fn qemu_command_with_galfs(img_path: &str, galfs_path: &PathBuf, serial_path: &PathBuf) -> Command {
    let mut cmd = Command::new("qemu-system-x86_64");
    cmd.arg("-drive")
        .arg(format!(
            "format=raw,file={img_path},if=ide,index=0,snapshot=on"
        ))
        .arg("-drive")
        .arg(format!(
            "format=raw,file={},if=ide,index=1,cache=writethrough",
            galfs_path.display()
        ))
        .arg("-smp")
        .arg("2")
        .arg("-cpu")
        .arg("max")
        .arg("-display")
        .arg("none")
        .arg("-no-reboot")
        .arg("-serial")
        .arg(format!("file:{}", serial_path.display()))
        .arg("-device")
        .arg("isa-debug-exit,iobase=0xf4,iosize=0x04")
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    cmd
}

/// Boots `image` with a fresh empty galfs data disk, then again with the
/// same data disk so the guest can prove the table survived. Returns
/// `(first_exit, first_serial, img_after_write, second_exit, second_serial)`.
pub fn boot_with_galfs(
    image: &Image,
) -> (Option<i32>, String, Vec<u8>, Option<i32>, String) {
    boot_with_galfs_inner(image, false)
}

/// Like [`boot_with_galfs`], but after the write boot the host destroys the
/// newest GALF slot's magic so the verify boot must recover from the older
/// dual-slot copy.
pub fn boot_with_galfs_recover(
    image: &Image,
) -> (Option<i32>, String, Vec<u8>, Option<i32>, String) {
    boot_with_galfs_inner(image, true)
}

fn boot_with_galfs_inner(
    image: &Image,
    corrupt_newest: bool,
) -> (Option<i32>, String, Vec<u8>, Option<i32>, String) {
    let galfs_path = std::env::temp_dir().join(format!(
        "galexy-galfs-{}-{}.img",
        image.name.replace('-', "_"),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos()
    ));
    // 1 MiB zeroed IDE slave — covers both 80-sector GALF slots.
    std::fs::write(&galfs_path, vec![0u8; 1024 * 1024]).expect("create galfs.img");

    let (code1, serial1) = boot_once_with_galfs(&image.bios, &galfs_path, &image.name);
    if let Ok(file) = std::fs::File::options().write(true).open(&galfs_path) {
        let _ = file.sync_all();
    }
    let img_after_write = std::fs::read(&galfs_path).expect("read galfs.img after write");
    if corrupt_newest {
        corrupt_newest_galfs_slot(&galfs_path);
    }
    let (code2, serial2) = boot_once_with_galfs(&image.bios, &galfs_path, &image.name);
    let _ = std::fs::remove_file(&galfs_path);
    (code1, serial1, img_after_write, code2, serial2)
}

/// Dual-slot layout must match `galfs::DISK_SECTORS` (80 × 512).
const GALFS_SLOT_SECTORS: usize = 80;
const GALFS_SECTOR: usize = 512;

fn corrupt_newest_galfs_slot(path: &PathBuf) {
    let mut data = std::fs::read(path).expect("read galfs.img");
    let mut best_gen = 0u64;
    let mut best_off: Option<usize> = None;
    for slot in 0..2 {
        let off = slot * GALFS_SLOT_SECTORS * GALFS_SECTOR;
        if data.len() < off + 28 || &data[off..off + 4] != b"GALF" {
            continue;
        }
        let gen = u64::from_le_bytes(data[off + 16..off + 24].try_into().unwrap());
        if best_off.is_none() || gen >= best_gen {
            best_gen = gen;
            best_off = Some(off);
        }
    }
    let off = best_off.expect("expected at least one GALF slot after write boot");
    data[off] ^= 0xFF; // break magic; CRC/path will reject the slot
    std::fs::write(path, &data).expect("write corrupted galfs.img");
    if let Ok(file) = std::fs::File::options().write(true).open(path) {
        let _ = file.sync_all();
    }
}

fn boot_once_with_galfs(
    img_path: &str,
    galfs_path: &PathBuf,
    name: &str,
) -> (Option<i32>, String) {
    let serial_path = serial_log_path(name);
    let mut child = qemu_command_with_galfs(img_path, galfs_path, &serial_path)
        .spawn()
        .expect("failed to launch qemu-system-x86_64 (galfs disk)");

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

/* ------------- QMP-driven typing (true end-to-end input) ------------- */

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;

/// Types `qcodes` into the guest through QEMU's QMP `send-key`, asserting
/// each key was accepted (QMP replies `{"return":{}}` per command).
///
/// Reads replies through the SAME buffered reader the handshake used — a
/// second reader over the same socket would race bytes with the first and
/// silently desynchronize the command/reply stream.
///
/// ASYNC EVENTS: QMP interleaves `{"event": ...}` lines (e.g. RTC_CHANGE
/// under OVMF) between replies — the reply read loops until an actual
/// reply line (`return`/`error`), skipping events (which must NOT be
/// silently dropped from the stream's perspective: they are just consumed,
/// exactly as the greeting/handshake reader does).
pub fn qmp_send_keys(reader: &mut BufReader<UnixStream>, qcodes: &[&str]) {
    for code in qcodes {
        // `shift+dot` is one chord (`>`). A code with no `+` is a single key.
        let keys = code
            .split('+')
            .map(|part| format!("{{\"type\":\"qcode\",\"data\":\"{part}\"}}"))
            .collect::<Vec<_>>()
            .join(",");
        let cmd = format!("{{\"execute\":\"send-key\",\"arguments\":{{\"keys\":[{keys}]}}}}\n");
        reader
            .get_mut()
            .write_all(cmd.as_bytes())
            .expect("QMP: send-key write failed");
        // Read lines until an actual reply; skip async event lines.
        loop {
            let mut reply = String::new();
            reader
                .read_line(&mut reply)
                .expect("QMP: send-key reply read failed");
            if reply.contains("\"event\"") {
                continue; // async event, not the reply for our command
            }
            assert!(
                reply.contains("\"return\"") || reply.contains("\"error\""),
                "QMP send-key '{code}' got no reply (stream desync?): {reply}"
            );
            assert!(
                !reply.contains("\"error\""),
                "QMP send-key '{code}' rejected: {reply}"
            );
            break;
        }
    }
}

/// Reads QMP responses until `needle` appears (with a deadline). QMP emits
/// one JSON line per command; the greeting includes `"QMP"`.
fn qmp_read_until(reader: &mut BufReader<UnixStream>, needle: &[u8], deadline: Instant) -> bool {
    let mut seen = String::new();
    loop {
        if Instant::now() > deadline {
            return false;
        }
        let mut line = String::new();
        match reader.read_line(&mut line) {
            Ok(0) => return false, // EOF: QEMU died
            Ok(_) => seen.push_str(&line),
            Err(ref e)
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut =>
            {
                std::thread::sleep(Duration::from_millis(20));
                continue;
            }
            Err(_) => return false,
        }
        if seen.as_bytes().windows(needle.len()).any(|w| w == needle) {
            return true;
        }
    }
}

/// Shell transcript with heartbeat lines and NUL padding removed.
///
/// The timer IRQ prints `[timer] Ns up` on COM1, sometimes in the middle
/// of an echoed key, and the serial file can contain a run of NULs. Either
/// one pulls a single-character cursor past the prompt. Indexes into this
/// string stay stable as the log grows: only shell and boot text remain,
/// in order.
fn typing_visible(raw: &str) -> String {
    let bytes = raw.as_bytes();
    let mut out = String::with_capacity(raw.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == 0 {
            i += 1;
            continue;
        }
        if bytes[i..].starts_with(b"[timer]") {
            while i < bytes.len() && bytes[i] != b'\n' {
                i += 1;
            }
            if i < bytes.len() {
                i += 1;
            }
            continue;
        }
        out.push(bytes[i] as char);
        i += 1;
    }
    out
}

/// Boots the interactive kernel with a QMP monitor; once the machine is
/// up, waits for `ready_marker` in serial (proves the shell's key consumer
/// is live), then types the `sync_pairs` — each `(qcode, echo)` pair is a
/// send-key followed by a wait for the guest's ECHO of that key on COM1
/// (the shell mirrors typed keys to serial). Syncing per key is REQUIRED:
/// pacing injection in wall time while the guest drains at TCG speed
/// overflows QEMU's 16-deep PS/2 queue and silently drops tail keystrokes
/// under host load. The last pair's `echo` may be a completion marker
/// (e.g. the program's output) instead of a typed char. Finally waits for
/// `final_marker` (kernel-side async work — tombstone/reap — lags the
/// last program output by a scheduling quantum or two under slow TCG).
/// Kills the guest at `timeout`. Returns the full serial output.
pub fn boot_and_type(
    image: &Image,
    sync_pairs: &[(&str, &str)],
    ready_marker: &str,
    final_marker: &str,
    key_delay: Duration,
    timeout: Duration,
) -> String {
    boot_and_type_on(
        image.bios.clone(),
        false,
        sync_pairs,
        ready_marker,
        final_marker,
        key_delay,
        timeout,
    )
}

/// UEFI variant of [`boot_and_type`]: boots the image's UEFI disk under
/// OVMF with a QMP monitor (same typing discipline, same marker syncs).
pub fn boot_and_type_uefi(
    image: &Image,
    sync_pairs: &[(&str, &str)],
    ready_marker: &str,
    final_marker: &str,
    key_delay: Duration,
    timeout: Duration,
) -> String {
    boot_and_type_on(
        image.uefi.clone(),
        true,
        sync_pairs,
        ready_marker,
        final_marker,
        key_delay,
        timeout,
    )
}

/// Kills the guest if a typing test panics before its normal teardown.
struct KillOnDrop(std::process::Child);

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

impl std::ops::Deref for KillOnDrop {
    type Target = std::process::Child;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl std::ops::DerefMut for KillOnDrop {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

/// Shared body: boots `img_path` (BIOS unless `uefi`, which adds `-bios
/// OVMF`), QMP monitor attached, types with per-key echo syncs.
fn boot_and_type_on(
    img_path: String,
    uefi: bool,
    sync_pairs: &[(&str, &str)],
    ready_marker: &str,
    final_marker: &str,
    key_delay: Duration,
    timeout: Duration,
) -> String {
    let serial_path = serial_log_path("typing");
    let sock = std::env::temp_dir().join(format!(
        "galexy-qmp-{}.sock",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos()
    ));
    let _ = std::fs::remove_file(&sock);

    let mut cmd = qemu_command(&img_path, &serial_path);
    if uefi {
        const OVMF_FD_DEFAULT: &str = "/usr/share/ovmf/x64/OVMF.4m.fd";
        let ovmf_fd = std::env::var("OVMF_FD").unwrap_or_else(|_| OVMF_FD_DEFAULT.into());
        cmd.arg("-bios").arg(ovmf_fd);
    }
    cmd.arg("-qmp")
        .arg(format!("unix:{},server,nowait", sock.display()));
    let mut child = KillOnDrop(
        cmd.spawn()
            .expect("failed to launch qemu-system-x86_64 (QMP typing test)"),
    );

    let deadline = Instant::now() + timeout;
    // QMP server comes up as QEMU starts the machine; retry-connect until
    // the deadline.
    let stream = loop {
        if Instant::now() > deadline {
            panic!("QMP socket never appeared: {}", sock.display());
        }
        match UnixStream::connect(&sock) {
            Ok(s) => break s,
            Err(_) => std::thread::sleep(Duration::from_millis(100)),
        }
    };
    stream
        .set_read_timeout(Some(Duration::from_millis(250)))
        .expect("QMP: read timeout");
    let mut reader = BufReader::new(stream);

    // Handshake: hello contains "QMP"; capabilities ack contains "return".
    assert!(
        qmp_read_until(&mut reader, b"\"QMP\"", deadline),
        "QMP greeting missing"
    );
    reader
        .get_mut()
        .write_all(b"{\"execute\":\"qmp_capabilities\"}\n")
        .expect("QMP: capabilities write failed");
    assert!(
        qmp_read_until(&mut reader, b"\"return\"", deadline),
        "QMP capabilities not acknowledged"
    );

    // Wait for the kernel to finish init (serial marker) — typing before
    // the consumer loop exists loses keystrokes.
    loop {
        if Instant::now() > deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!(
                "ready marker '{ready_marker}' never appeared; serial:\n{}",
                std::fs::read_to_string(&serial_path).unwrap_or_default()
            );
        }
        if std::fs::read_to_string(&serial_path)
            .map(|s| s.contains(ready_marker))
            .unwrap_or(false)
        {
            break;
        }
        if child.try_wait().expect("try_wait failed").is_some() {
            panic!("guest exited before the ready marker '{ready_marker}'");
        }
        std::thread::sleep(Duration::from_millis(50));
    }

    // Type, syncing each key on the guest's echo of it (serial, with
    // heartbeats and NUL padding stripped). The cursor advances
    // monotonically. Single-char echoes can still false-match the boot
    // log; that only makes one sync complete a beat early. A longer
    // marker may overlap the cursor when earlier keys already consumed
    // its prefix.
    let mut seen = 0usize;
    for (qcode, echo) in sync_pairs {
        qmp_send_keys(&mut reader, &[qcode]);
        std::thread::sleep(key_delay);
        loop {
            if Instant::now() > deadline {
                let serial =
                    typing_visible(&std::fs::read_to_string(&serial_path).unwrap_or_default());
                let lo = seen.saturating_sub(400).min(serial.len());
                let hi = (seen + 200).min(serial.len());
                panic!(
                    "echo '{echo}' never appeared after key '{qcode}' (seen {seen}/{}):\n{}",
                    serial.len(),
                    &serial[lo..hi]
                );
            }
            if child.try_wait().expect("try_wait failed").is_some() {
                panic!("guest exited mid-typing (key '{qcode}')");
            }
            let data = typing_visible(&std::fs::read_to_string(&serial_path).unwrap_or_default());
            // A multi-byte marker may start before `seen` (the per-key
            // cursor already ate its first characters) and finish after.
            // Accept the earliest match that ends past the cursor.
            let start = seen.saturating_sub(echo.len().saturating_sub(1));
            let start = start.min(data.len());
            if let Some(at) = data[start..].find(echo) {
                seen = start + at + echo.len();
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    // Wait for the kernel-side completion marker (async: the exit handoff
    // + reaper run on the rotation's schedule, not synchronously with the
    // program's output).
    if !final_marker.is_empty() {
        loop {
            if Instant::now() > deadline {
                panic!(
                    "final marker '{final_marker}' never appeared; serial tail:\n{}",
                    std::fs::read_to_string(&serial_path).unwrap_or_default()
                );
            }
            if child.try_wait().expect("try_wait failed").is_some() {
                panic!("guest exited before the final marker '{final_marker}'");
            }
            if std::fs::read_to_string(&serial_path)
                .map(|s| s.contains(final_marker))
                .unwrap_or(false)
            {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    let _ = child.kill();
    let _ = child.wait();
    let _ = std::fs::remove_file(&sock);
    std::fs::read_to_string(&serial_path).unwrap_or_default()
}
