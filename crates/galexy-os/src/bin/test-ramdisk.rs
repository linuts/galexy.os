//! Integration test kernel: the ramdisk lands in memory and its tar
//! archive is readable. Asserts `banner.txt` (packed by the runner's
//! build.rs) round-trips byte-for-byte through the phys map.

#![no_std]
#![no_main]

use bootloader_api::{entry_point, BootInfo};
use galexy_core::TarCursor;
use galexy_os::{drivers::screen, exit_qemu, println, serial_println, QemuExitCode};
use x86_64::VirtAddr;

entry_point!(test_main_entry, config = &galexy_os::BOOTLOADER_CONFIG);

/// The runner packs this exact file into the tar (see build.rs).
const BANNER_TEXT: &[u8] = b"galexy ramdisk plumbing works";

fn test_main_entry(boot_info: &'static mut BootInfo) -> ! {
    galexy_os::init();
    screen::init(boot_info);
    println!("[test-ramdisk] running");
    serial_println!("[test-ramdisk] running");

    let Some(ramdisk_addr) = boot_info.ramdisk_addr.into_option() else {
        panic!("no ramdisk handed to the kernel");
    };
    let ramdisk_len = boot_info.ramdisk_len;
    serial_println!(
        "[test-ramdisk] archive at {:#x}, {} bytes",
        ramdisk_addr,
        ramdisk_len
    );

    // The phys map is up BEFORE the frame allocator matters; mm::init
    // brings everything online (the phys mapping is fixed in the config).
    galexy_os::arch::init();
    galexy_os::arch::mm::init(boot_info);

    // Ramdisk bytes: `ramdisk_addr` is a VIRTUAL address the bootloader
    // already mapped into the kernel's (and thus every task's) space —
    // like the framebuffer, not a physical address.
    let base = VirtAddr::new(ramdisk_addr);
    // SAFETY: the bootloader has mapped the (contiguous) ramdisk image at
    // [ramdisk_addr, +len) for kernel use.
    let archive = unsafe {
        core::slice::from_raw_parts(base.as_ptr::<u8>(), ramdisk_len as usize)
    };

    let mut found_banner = false;
    let mut cursor = TarCursor::new(archive);
    while let Some((name, body)) = cursor.next_file() {
        serial_println!("[test-ramdisk] tar entry: '{}' ({} bytes)", name, body.len());
        if name == "banner.txt" {
            found_banner = true;
            assert_eq!(body, BANNER_TEXT, "ramdisk banner.txt must roundtrip");
        }
    }
    assert!(found_banner, "banner.txt missing from the ramdisk tar");

    println!("[test-ramdisk] tar roundtrip through the phys map works");
    println!("[test-ramdisk] all assertions passed");
    serial_println!("[test-ramdisk] passed");
    exit_qemu(QemuExitCode::Success);
}
