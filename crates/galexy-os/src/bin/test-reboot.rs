//! Reset: the FADT reset register or the keyboard controller must make
//! QEMU exit (`-no-reboot`). A return is a failure.

#![no_std]
#![no_main]

use bootloader_api::{entry_point, BootInfo};
use galexy_os::{arch, drivers::screen, serial_println};

entry_point!(test_main_entry, config = &galexy_os::BOOTLOADER_CONFIG);

fn test_main_entry(boot_info: &'static mut BootInfo) -> ! {
    galexy_os::init();
    screen::init(boot_info);
    serial_println!("[test-reboot] running");

    arch::mm::init(boot_info);
    arch::init(boot_info);

    serial_println!("[test-reboot] resetting");
    arch::power::reboot();
    panic!("reboot returned; the machine stayed up");
}
