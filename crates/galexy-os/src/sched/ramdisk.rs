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

use galexy_core::TarCursor;
use spin::Mutex;

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
