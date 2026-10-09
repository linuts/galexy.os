//! Boot-test helper: boots a galexy.os kernel image in headless QEMU and
//! returns (exit code, serial output).

use std::io::Read;
use std::path::{Path, PathBuf};
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
///
/// The counter restarts with every test process, so a name can collide
/// with a log left by an earlier run. QEMU truncates the file only when
/// its chardev opens, some hundred milliseconds after `spawn()`, and the
/// harnesses that poll the serial while the guest runs (crash injection,
/// typing) would act on the stale contents in that window. Remove it up
/// front so every poll sees only this boot's output.
fn serial_log_path(name: &str) -> PathBuf {
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let path =
        std::env::temp_dir().join(format!("galexy-serial-{}-{n}.log", name.replace('-', "_")));
    let _ = std::fs::remove_file(&path);
    path
}

/// Builds the QEMU command for `img_path`: headless, COM1 to `serial_path`,
/// writable-overlays (`-snapshot`) so parallel tests never conflict.
///
/// `-smp 2` plus either `-accel kvm -cpu host` or `-accel tcg -cpu max`.
///
/// KVM when `/dev/kvm` is writable (or `GALEXY_ACCEL=kvm`). Otherwise TCG.
/// `GALEXY_ACCEL=tcg` forces the software path. `-cpu max` / `host` exposes
/// FSGSBASE, which the per-CPU substrate requires. 2 cores exercise those
/// paths in every test.
fn apply_accel(cmd: &mut Command) {
    use std::sync::atomic::{AtomicBool, Ordering};
    static PRINTED: AtomicBool = AtomicBool::new(false);
    let kvm = use_kvm();
    if PRINTED
        .compare_exchange(false, true, Ordering::Relaxed, Ordering::Relaxed)
        .is_ok()
    {
        eprintln!("[runner] accel={}", if kvm { "kvm" } else { "tcg" });
    }
    if kvm {
        cmd.arg("-accel").arg("kvm").arg("-cpu").arg("host");
    } else {
        cmd.arg("-accel").arg("tcg").arg("-cpu").arg("max");
    }
}

/// True when this process will boot QEMU with KVM.
pub fn use_kvm() -> bool {
    match std::env::var("GALEXY_ACCEL").ok().as_deref() {
        Some("tcg") => false,
        Some("kvm") => true,
        _ => std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open("/dev/kvm")
            .is_ok(),
    }
}

fn qemu_command(img_path: &str, serial_path: &Path) -> Command {
    let mut cmd = Command::new("qemu-system-x86_64");
    cmd.arg("-drive")
        .arg(format!("format=raw,file={img_path}"))
        .arg("-snapshot")
        .arg("-smp")
        .arg("2");
    apply_accel(&mut cmd);
    cmd.arg("-display")
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

/// QEMU `-drive` `cache=` mode for the galfs data disk.
///
/// Guest galfs always flushes after writing the inactive dual slot.
/// These modes exercise that barrier against the host cache.
#[derive(Clone, Copy, Debug)]
pub enum GalfsDiskCache {
    /// Default for most tests — host page cache writes through.
    Writethrough,
    /// Host may buffer writes; durability depends on guest flush.
    Writeback,
    /// Bypass host page cache (`O_DIRECT`-ish); still needs guest flush
    /// for any drive-side write cache QEMU models.
    None,
}

impl GalfsDiskCache {
    fn as_qemu(self) -> &'static str {
        match self {
            Self::Writethrough => "writethrough",
            Self::Writeback => "writeback",
            Self::None => "none",
        }
    }
}

/// How the galfs image is attached to QEMU.
#[derive(Clone, Copy, Debug)]
pub enum GalfsBackend {
    /// Primary IDE slave (`if=ide,index=1`) — ATA PIO `PrimarySlave`.
    IdeSlave,
    /// Virtio-blk PCI transitional (legacy IO BAR) — `drivers::virtio_blk`.
    VirtioPci,
}

/// Like [`qemu_command`], but the boot drive uses a per-drive snapshot and
/// `galfs_path` is attached without a snapshot so writes persist for a
/// second boot of the same image.
fn qemu_command_with_galfs(
    img_path: &str,
    galfs_path: &Path,
    serial_path: &Path,
    cache: GalfsDiskCache,
    backend: GalfsBackend,
) -> Command {
    let mut cmd = Command::new("qemu-system-x86_64");
    cmd.arg("-drive").arg(format!(
        "format=raw,file={img_path},if=ide,index=0,snapshot=on"
    ));
    match backend {
        GalfsBackend::IdeSlave => {
            cmd.arg("-drive").arg(format!(
                "format=raw,file={},if=ide,index=1,cache={}",
                galfs_path.display(),
                cache.as_qemu()
            ));
        }
        GalfsBackend::VirtioPci => {
            cmd.arg("-drive").arg(format!(
                "format=raw,file={},if=none,id=galfs,cache={}",
                galfs_path.display(),
                cache.as_qemu()
            ));
            // Force legacy IO BAR so the guest virtio-blk driver can use
            // the transitional register layout (no modern MMIO yet).
            cmd.arg("-device").arg(
                "virtio-blk-pci,drive=galfs,disable-legacy=off,disable-modern=on,queue-size=128",
            );
        }
    }
    cmd.arg("-smp").arg("2");
    apply_accel(&mut cmd);
    cmd.arg("-display")
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
///
/// Uses [`GalfsDiskCache::Writethrough`] (see [`boot_with_galfs_cache`] for
/// the flush matrix).
pub fn boot_with_galfs(image: &Image) -> (Option<i32>, String, Vec<u8>, Option<i32>, String) {
    boot_with_galfs_cache(image, GalfsDiskCache::Writethrough)
}

/// Like [`boot_with_galfs`], but sets the IDE slave `cache=` mode so flush
/// discipline can be tested under `writeback` and `none`.
pub fn boot_with_galfs_cache(
    image: &Image,
    cache: GalfsDiskCache,
) -> (Option<i32>, String, Vec<u8>, Option<i32>, String) {
    boot_with_galfs_inner(
        image,
        CorruptMode::None,
        cache,
        GalfsBackend::IdeSlave,
        GALFS_IMG_BYTES,
    )
}

/// Like [`boot_with_galfs`], but attaches the data disk as virtio-blk-pci
/// (legacy) instead of the IDE slave.
pub fn boot_with_galfs_virtio(
    image: &Image,
) -> (Option<i32>, String, Vec<u8>, Option<i32>, String) {
    boot_with_galfs_inner(
        image,
        CorruptMode::None,
        GalfsDiskCache::Writethrough,
        GalfsBackend::VirtioPci,
        GALFS_IMG_BYTES,
    )
}

/// Like [`boot_with_galfs`], but the guest places GALF at LBA 2048
/// (`DISK_PART_LBA`). Image is 2 MiB so base + dual slots fit.
pub fn boot_with_galfs_part(image: &Image) -> (Option<i32>, String, Vec<u8>, Option<i32>, String) {
    boot_with_galfs_inner(
        image,
        CorruptMode::None,
        GalfsDiskCache::Writethrough,
        GalfsBackend::IdeSlave,
        GALFS_IMG_BYTES_PARTED,
    )
}

/// Byte offset of GALF slot 0 when using [`boot_with_galfs_part`].
pub const GALFS_PART_BYTE_OFF: usize = 2048 * 512;

/// Like [`boot_with_galfs`], but after the write boot the host destroys the
/// newest GALF slot's AEAD tag so the verify boot must recover from the older
/// dual-slot copy.
pub fn boot_with_galfs_recover(
    image: &Image,
) -> (Option<i32>, String, Vec<u8>, Option<i32>, String) {
    boot_with_galfs_inner(
        image,
        CorruptMode::Newest,
        GalfsDiskCache::Writethrough,
        GalfsBackend::IdeSlave,
        GALFS_IMG_BYTES,
    )
}

/// Like [`boot_with_galfs_recover`], but simulates a torn write: the newest
/// slot keeps `GALF` magic while its payload is zeroed from mid-sector.
pub fn boot_with_galfs_torn(image: &Image) -> (Option<i32>, String, Vec<u8>, Option<i32>, String) {
    boot_with_galfs_inner(
        image,
        CorruptMode::TornNewest,
        GalfsDiskCache::Writethrough,
        GalfsBackend::IdeSlave,
        GALFS_IMG_BYTES,
    )
}

/// Write boot with `writer`, then corrupt **both** slots and boot `reader`.
pub fn boot_with_galfs_both_corrupt(
    writer: &Image,
    reader: &Image,
) -> (Option<i32>, String, Vec<u8>, Option<i32>, String, Vec<u8>) {
    let galfs_path = std::env::temp_dir().join(format!(
        "galexy-galfs-both-{}-{}.img",
        writer.name.replace('-', "_"),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos()
    ));
    std::fs::write(&galfs_path, vec![0u8; GALFS_IMG_BYTES]).expect("create galfs.img");
    let cache = GalfsDiskCache::Writethrough;
    let backend = GalfsBackend::IdeSlave;
    let (code1, serial1) =
        boot_once_with_galfs(&writer.bios, &galfs_path, &writer.name, cache, backend);
    let img_after_write = std::fs::read(&galfs_path).expect("read galfs.img after write");
    corrupt_all_galfs_slots(&galfs_path);
    let (code2, serial2) =
        boot_once_with_galfs(&reader.bios, &galfs_path, &reader.name, cache, backend);
    let img_after = std::fs::read(&galfs_path).expect("read galfs.img after corrupt boot");
    let _ = std::fs::remove_file(&galfs_path);
    (code1, serial1, img_after_write, code2, serial2, img_after)
}

enum CorruptMode {
    None,
    Newest,
    TornNewest,
}

/// 1 MiB — covers dual slots at LBA 0.
const GALFS_IMG_BYTES: usize = 1024 * 1024;
/// 2 MiB — covers LBA 2048 partition offset + dual slots.
const GALFS_IMG_BYTES_PARTED: usize = 2 * 1024 * 1024;

fn boot_with_galfs_inner(
    image: &Image,
    corrupt: CorruptMode,
    cache: GalfsDiskCache,
    backend: GalfsBackend,
    img_bytes: usize,
) -> (Option<i32>, String, Vec<u8>, Option<i32>, String) {
    let galfs_path = std::env::temp_dir().join(format!(
        "galexy-galfs-{}-{}.img",
        image.name.replace('-', "_"),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos()
    ));
    std::fs::write(&galfs_path, vec![0u8; img_bytes]).expect("create galfs.img");

    let (code1, serial1) =
        boot_once_with_galfs(&image.bios, &galfs_path, &image.name, cache, backend);
    if let Ok(file) = std::fs::File::options().write(true).open(&galfs_path) {
        let _ = file.sync_all();
    }
    let img_after_write = std::fs::read(&galfs_path).expect("read galfs.img after write");
    match corrupt {
        CorruptMode::None => {}
        CorruptMode::Newest => corrupt_newest_galfs_slot(&galfs_path),
        CorruptMode::TornNewest => tear_newest_galfs_slot(&galfs_path),
    }
    let (code2, serial2) =
        boot_once_with_galfs(&image.bios, &galfs_path, &image.name, cache, backend);
    let _ = std::fs::remove_file(&galfs_path);
    (code1, serial1, img_after_write, code2, serial2)
}

/// Dual-slot layout must match `galfs::DISK_SECTORS` (288 × 512).
const GALFS_SLOT_SECTORS: usize = 288;
const GALFS_SECTOR: usize = 512;
/// Matches kernel / `galexy_galf::DISK_HEADER`.
const GALFS_DISK_HEADER: usize = 128;

/// Keep `GALF` magic so the guest can tell a used volume from empty zeros;
/// flip the AEAD data tag so decode/open fails (header offset 112).
const GALFS_DATA_TAG_OFF: usize = 112;

fn newest_galfs_slot_off(data: &[u8]) -> usize {
    let mut best_gen = 0u64;
    let mut best_off: Option<usize> = None;
    for slot in 0..2 {
        let off = slot * GALFS_SLOT_SECTORS * GALFS_SECTOR;
        if data.len() < off + GALFS_DATA_TAG_OFF + 1 || &data[off..off + 4] != b"GALF" {
            continue;
        }
        let gen = u64::from_le_bytes(data[off + 16..off + 24].try_into().unwrap());
        if best_off.is_none() || gen >= best_gen {
            best_gen = gen;
            best_off = Some(off);
        }
    }
    best_off.expect("expected at least one GALF slot after write boot")
}

fn corrupt_newest_galfs_slot(path: &Path) {
    let mut data = std::fs::read(path).expect("read galfs.img");
    let off = newest_galfs_slot_off(&data);
    data[off + GALFS_DATA_TAG_OFF] ^= 0xFF;
    std::fs::write(path, &data).expect("write corrupted galfs.img");
    if let Ok(file) = std::fs::File::options().write(true).open(path) {
        let _ = file.sync_all();
    }
}

/// Simulate a crash mid-write: keep `GALF` magic + half a payload sector,
/// zero the rest of the newest slot (older sibling stays intact).
fn tear_newest_galfs_slot(path: &Path) {
    let mut data = std::fs::read(path).expect("read galfs.img");
    let off = newest_galfs_slot_off(&data);
    let slot_end = off + GALFS_SLOT_SECTORS * GALFS_SECTOR;
    // Cut mid-sector in the first payload sector (header stays, CRC/AEAD fail).
    let cut = off + GALFS_DISK_HEADER + (GALFS_SECTOR / 2);
    assert!(cut < slot_end, "torn cut must land inside the slot");
    let wipe_end = slot_end.min(data.len());
    data[cut..wipe_end].fill(0);
    std::fs::write(path, &data).expect("write torn galfs.img");
    if let Ok(file) = std::fs::File::options().write(true).open(path) {
        let _ = file.sync_all();
    }
}

fn corrupt_all_galfs_slots(path: &Path) {
    let mut data = std::fs::read(path).expect("read galfs.img");
    let mut any = false;
    for slot in 0..2 {
        let off = slot * GALFS_SLOT_SECTORS * GALFS_SECTOR;
        if data.len() < off + GALFS_DATA_TAG_OFF + 1 || &data[off..off + 4] != b"GALF" {
            continue;
        }
        data[off + GALFS_DATA_TAG_OFF] ^= 0xFF;
        any = true;
    }
    assert!(any, "expected GALF magic in at least one slot");
    std::fs::write(path, &data).expect("write both-corrupt galfs.img");
    if let Ok(file) = std::fs::File::options().write(true).open(path) {
        let _ = file.sync_all();
    }
}

fn boot_once_with_galfs(
    img_path: &str,
    galfs_path: &Path,
    name: &str,
    cache: GalfsDiskCache,
    backend: GalfsBackend,
) -> (Option<i32>, String) {
    let serial_path = serial_log_path(name);
    let mut child = qemu_command_with_galfs(img_path, galfs_path, &serial_path, cache, backend)
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

/// Commit a durable slot, kill QEMU during the next mutate, boot again.
///
/// The guest prints `[test-crash] mutating` and then calls `sync`. This
/// waits until that line is visible, pauses briefly so the commit is in
/// progress (key wrap, before the new slot is published), and kills QEMU.
/// The second boot must observe a consistent slot.
pub fn boot_with_galfs_crash(image: &Image) -> (String, Option<i32>, String) {
    let galfs_path = std::env::temp_dir().join(format!(
        "galexy-galfs-crash-{}-{}.img",
        image.name.replace('-', "_"),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos()
    ));
    std::fs::write(&galfs_path, vec![0u8; GALFS_IMG_BYTES]).expect("create galfs.img");
    let serial_path = serial_log_path(&format!("{}-kill", image.name));
    let mut child = qemu_command_with_galfs(
        &image.bios,
        &galfs_path,
        &serial_path,
        GalfsDiskCache::Writethrough,
        GalfsBackend::IdeSlave,
    )
    .spawn()
    .expect("failed to launch qemu-system-x86_64 (crash injection)");

    let deadline = Instant::now() + TEST_TIMEOUT;
    loop {
        match child.try_wait().expect("try_wait failed") {
            Some(_) => break,
            None if Instant::now() > deadline => {
                let _ = child.kill();
                let _ = child.wait();
                break;
            }
            None => {
                let serial = std::fs::read_to_string(&serial_path).unwrap_or_default();
                if serial.contains("[test-crash] mutating") {
                    // Inside the in-flight commit: PBKDF runs before the
                    // inactive slot is written, so a short pause is still
                    // mid-mutate and the previous slot stays authoritative.
                    std::thread::sleep(Duration::from_millis(250));
                    let _ = child.kill();
                    let _ = child.wait();
                    break;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
        }
    }
    let serial1 = std::fs::read_to_string(&serial_path).unwrap_or_default();
    let (code2, serial2) = boot_once_with_galfs(
        &image.bios,
        &galfs_path,
        &image.name,
        GalfsDiskCache::Writethrough,
        GalfsBackend::IdeSlave,
    );
    let _ = std::fs::remove_file(&galfs_path);
    (serial1, code2, serial2)
}

/// One boot with a fresh zeroed GALF image on the IDE slave.
pub fn boot_galfs_once(image: &Image) -> (Option<i32>, String) {
    let galfs_path = std::env::temp_dir().join(format!(
        "galexy-galfs-once-{}-{}.img",
        image.name.replace('-', "_"),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos()
    ));
    std::fs::write(&galfs_path, vec![0u8; GALFS_IMG_BYTES]).expect("create galfs.img");
    let result = boot_once_with_galfs(
        &image.bios,
        &galfs_path,
        &image.name,
        GalfsDiskCache::Writethrough,
        GalfsBackend::IdeSlave,
    );
    let _ = std::fs::remove_file(&galfs_path);
    result
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

/// Boots the main image with COM1 on a Unix socket and types a login.
///
/// No data disk, so the seat comes up RAM-only and asks `Login as:`
/// directly. Returns everything the guest wrote. The caller kills nothing;
/// this function stops QEMU before returning.
pub fn uart_login_serial(image: &Image) -> String {
    use std::io::Write;
    use std::os::unix::net::UnixStream;

    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    let sock_path = std::env::temp_dir().join(format!("galexy-uart-{nanos}.sock"));
    let _ = std::fs::remove_file(&sock_path);

    let mut cmd = Command::new("qemu-system-x86_64");
    cmd.arg("-drive")
        .arg(format!(
            "format=raw,file={},if=ide,index=0,snapshot=on",
            image.bios
        ))
        .arg("-smp")
        .arg("2");
    apply_accel(&mut cmd);
    let mut child = cmd
        .arg("-display")
        .arg("none")
        .arg("-monitor")
        .arg("none")
        .arg("-no-reboot")
        .arg("-serial")
        .arg(format!("unix:{},server=on,wait=off", sock_path.display()))
        .arg("-device")
        .arg("isa-debug-exit,iobase=0xf4,iosize=0x04")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("failed to launch qemu-system-x86_64 (uart login)");

    let deadline = Instant::now() + TEST_TIMEOUT;
    let stream = loop {
        if let Some(status) = child.try_wait().expect("try_wait failed") {
            let _ = std::fs::remove_file(&sock_path);
            panic!("qemu exited before the serial socket connected: {status:?}");
        }
        if let Ok(stream) = UnixStream::connect(&sock_path) {
            break stream;
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            let _ = child.wait();
            let _ = std::fs::remove_file(&sock_path);
            panic!("serial socket never appeared at {}", sock_path.display());
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    stream
        .set_read_timeout(Some(Duration::from_millis(200)))
        .expect("set_read_timeout");

    let mut buf = Vec::new();
    let mut scratch = [0u8; 1024];
    let mut stream = stream;

    // DEL drops the extra `x`, so the name is `admin`. CR is Enter.
    let steps: &[(&str, Option<&[u8]>)] = &[
        ("Login as:", Some(b"admix\x7fn\r")),
        ("Password:", Some(b"admin\r")),
        ("passwd: change the default password", None),
    ];
    for (needle, then) in steps {
        if !uart_read_until(
            &mut stream,
            &mut buf,
            &mut scratch,
            &mut child,
            needle,
            deadline,
        ) {
            let _ = child.kill();
            let _ = child.wait();
            let _ = std::fs::remove_file(&sock_path);
            panic!(
                "{needle} never appeared on COM1; serial:\n{}",
                String::from_utf8_lossy(&buf)
            );
        }
        if let Some(bytes) = then {
            stream.write_all(bytes).expect("write COM1");
        }
    }

    let _ = child.kill();
    let _ = child.wait();
    let _ = std::fs::remove_file(&sock_path);
    String::from_utf8_lossy(&buf).into_owned()
}

/// Reads the guest's COM1 until `needle` shows up, the deadline passes, or
/// QEMU exits. Timed-out reads are retried; any other I/O error stops.
fn uart_read_until(
    stream: &mut std::os::unix::net::UnixStream,
    buf: &mut Vec<u8>,
    scratch: &mut [u8],
    child: &mut std::process::Child,
    needle: &str,
    deadline: Instant,
) -> bool {
    use std::io::Read;
    while Instant::now() < deadline {
        if child.try_wait().expect("try_wait failed").is_some() {
            return false;
        }
        match stream.read(scratch) {
            Ok(0) => return false,
            Ok(n) => buf.extend_from_slice(&scratch[..n]),
            Err(e)
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut => {}
            Err(_) => return false,
        }
        if std::str::from_utf8(buf).unwrap_or("").contains(needle) {
            return true;
        }
    }
    false
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

/// Shell transcript with kernel timestamp lines and NUL padding removed.
///
/// Kernel `serial_println!` prefixes lines with `Ns: …` (uptime seconds).
/// Those can splice into the middle of an echoed key, and the serial file
/// can contain a run of NULs. Either one pulls a single-character cursor
/// past the prompt. Indexes into this string stay stable as the log grows:
/// only shell and boot text remain, in order.
fn typing_visible(raw: &str) -> String {
    let bytes = raw.as_bytes();
    let mut out = String::with_capacity(raw.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == 0 {
            i += 1;
            continue;
        }
        // Strip the `Ns: ` uptime prefix but keep the payload so markers
        // like `[tty] 2` still sync. Dropping the whole line broke F-key
        // console-switch e2e after write-back made Cap-wait finish quickly.
        if is_uptime_log_prefix(bytes, i) {
            while i < bytes.len() && bytes[i].is_ascii_digit() {
                i += 1;
            }
            if bytes[i..].starts_with(b"s: ") {
                i += 3;
            }
            continue;
        }
        out.push(bytes[i] as char);
        i += 1;
    }
    out
}

/// True when `bytes[i..]` starts with `Ns: ` (kernel serial uptime prefix).
fn is_uptime_log_prefix(bytes: &[u8], i: usize) -> bool {
    let mut j = i;
    if j >= bytes.len() || !bytes[j].is_ascii_digit() {
        return false;
    }
    while j < bytes.len() && bytes[j].is_ascii_digit() {
        j += 1;
    }
    bytes[j..].starts_with(b"s: ")
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
