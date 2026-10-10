//! The ramdisk as a kernel service.
//!
//! One archive, set once at boot from `BootInfo.ramdisk_*` (a VIRTUAL
//! address the bootloader mapped — treated like the framebuffer, never a
//! physical address), then read-only lookups for anything userland wants to
//! load. The lifespan is the kernel's: the bootloader reserves those bytes
//! for as long as the kernel runs, so a `&'static [u8]` view is sound.
//!
//! Read path is `galexy_core::TarCursor`; every access re-walks the tar
//! (no caching yet — programs are small, churn is near zero).

use crate::sync::Mutex;
use galexy_core::TarCursor;

use crate::serial_println;

/// The raw tar archive, set once from `BootInfo`.
static RAMDISK: Mutex<Option<&'static [u8]>> = Mutex::new(None);

/// Publishes the ramdisk bytes handed over by the bootloader.
///
/// `bytes` must be the bootloader-mapped view (`ramdisk_addr..+len`); it is
/// kernel-lifetime data, so only `&'static` is taken.
pub fn init(bytes: &'static [u8]) {
    *RAMDISK.lock() = Some(bytes);
    serial_println!("[ramdisk] set: {} byte(s)", bytes.len());
}

/// SHA-256 of the whole archive — the same bytes `crates/runner/build.rs`
/// hashes when it packs the tar, so a boot can prove it runs the ramdisk
/// the build measured. Costs one pass over the archive; call it from a
/// test kernel, not from the boot path.
pub fn measure() -> Option<[u8; 32]> {
    let archive = (*RAMDISK.lock())?;
    Some(galexy_crypto::sha256(archive))
}

/// Total archive length in bytes, once set.
pub fn len() -> Option<usize> {
    (*RAMDISK.lock()).map(|archive| archive.len())
}

/// Calls `f` with each regular file's name, in archive order.
///
/// The walk does not allocate. `f` runs while the ramdisk lock is held,
/// so it must not call back into this module.
pub fn for_each_name(mut f: impl FnMut(&str)) {
    let guard = RAMDISK.lock();
    let Some(archive) = *guard else {
        return;
    };
    let mut cursor = TarCursor::new(archive);
    while let Some((name, _)) = cursor.next_file() {
        f(name);
    }
}

/// Looks up a regular file by exact name.
pub fn find(name: &str) -> Option<&'static [u8]> {
    let archive = (*RAMDISK.lock())?;
    let mut cursor = TarCursor::new(archive);
    while let Some((file_name, body)) = cursor.next_file() {
        if file_name == name {
            return Some(body);
        }
    }
    None
}
