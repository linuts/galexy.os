//! Two boots of this image must print different kernel bases.
//!
//! `BOOTLOADER_CONFIG.mappings.aslr` randomizes the PIE kernel. The
//! runner compares the line across two boots.

#![no_std]
#![no_main]

use bootloader_api::{entry_point, BootInfo};
use galexy_os::{exit_qemu, serial_println, QemuExitCode};

entry_point!(test_main_entry, config = &galexy_os::BOOTLOADER_CONFIG);

fn test_main_entry(boot_info: &'static mut BootInfo) -> ! {
    galexy_os::init();
    serial_println!(
        "[kaslr] kernel_image_offset={:#x}",
        boot_info.kernel_image_offset
    );
    serial_println!("[test-kaslr] passed");
    exit_qemu(QemuExitCode::Success);
}
