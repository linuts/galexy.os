//! The kernel generator picks a source and two draws differ.

#![no_std]
#![no_main]

use bootloader_api::{entry_point, BootInfo};
use galexy_os::{arch, exit_qemu, serial_println, QemuExitCode};

entry_point!(test_main_entry, config = &galexy_os::BOOTLOADER_CONFIG);

fn test_main_entry(boot_info: &'static mut BootInfo) -> ! {
    galexy_os::init();
    arch::mm::init(boot_info);
    arch::init(boot_info);
    serial_println!("[test-rand] source={}", arch::rand::source());
    let mut first = [0u8; 8];
    let mut second = [0u8; 8];
    arch::rand::fill_bytes(&mut first);
    arch::rand::fill_bytes(&mut second);
    assert_ne!(first, second, "two draws matched");
    assert!(first.iter().any(|b| *b != 0), "draw was zeros");
    serial_println!("[test-rand] passed");
    exit_qemu(QemuExitCode::Success);
}
