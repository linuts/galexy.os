//! ACPI S5: the FADT power block must name a sleep type, and programming
//! it must make QEMU exit. A return is a failure (the panic that follows).

#![no_std]
#![no_main]

use bootloader_api::{entry_point, BootInfo};
use galexy_os::{arch, drivers::screen, serial_println};

entry_point!(test_main_entry, config = &galexy_os::BOOTLOADER_CONFIG);

fn test_main_entry(boot_info: &'static mut BootInfo) -> ! {
    galexy_os::init();
    screen::init(boot_info);
    serial_println!("[test-shutdown] running");

    arch::mm::init(boot_info);
    arch::init(boot_info);

    let info = arch::acpi::power_info().expect("firmware published no FADT");
    serial_println!(
        "[test-shutdown] pm1a {:#x} has_s5 {} typ {}",
        info.pm1a_cnt,
        info.has_s5 as u8,
        info.slp_typa
    );
    assert!(info.pm1a_cnt != 0, "PM1a control port is required");
    assert!(info.has_s5, "DSDT must name _S5_");

    serial_println!("[test-shutdown] powering off");
    arch::power::shutdown();
    panic!("shutdown returned; the machine stayed up");
}
