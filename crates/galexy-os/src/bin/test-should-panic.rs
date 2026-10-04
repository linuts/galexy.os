//! Integration test kernel: verifies the panic handler treats an expected
//! panic as Success (the should_panic mechanism).

#![no_std]
#![no_main]

use bootloader_api::{entry_point, BootInfo};
use galexy_os::{expect_panic, serial_println};

entry_point!(test_main_entry);

fn test_main_entry(_boot_info: &'static mut BootInfo) -> ! {
    galexy_os::init();
    serial_println!("[test-should-panic] running");
    expect_panic("test-should-panic.rs");
    panic!("intentional should_panic");
}
