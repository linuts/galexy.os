//! Absent ATA slave: read, write, and flush return `Unsupported`.
//!
//! A missing disk must not panic. Status and timeout failures on a
//! present drive log `[ata] I/O …` and return the same error.

#![no_std]
#![no_main]

use bootloader_api::{entry_point, BootInfo};
use galexy_abi::SysError;
use galexy_os::drivers::ata::{self, SECTOR};
use galexy_os::{drivers::screen, exit_qemu, println, serial_println, QemuExitCode};

entry_point!(test_main_entry, config = &galexy_os::BOOTLOADER_CONFIG);

fn test_main_entry(boot_info: &'static mut BootInfo) -> ! {
    galexy_os::init();
    screen::init(boot_info);
    println!("[test-ata] running");
    serial_println!("[test-ata] running");

    assert!(!ata::present(), "this boot has no IDE slave");
    let mut buf = [[0u8; SECTOR]; 1];
    assert_eq!(ata::read_sectors(0, &mut buf), Err(SysError::Unsupported));
    assert_eq!(ata::write_sectors(0, &buf), Err(SysError::Unsupported));
    assert_eq!(ata::flush(), Err(SysError::Unsupported));
    let empty: &mut [[u8; SECTOR]] = &mut [];
    assert_eq!(
        ata::read_sectors(0, empty),
        Err(SysError::BadValue),
        "empty I/O is a bad argument, not a panic"
    );

    println!("[test-ata] errors surfaced");
    serial_println!("[test-ata] passed");
    exit_qemu(QemuExitCode::Success);
}
