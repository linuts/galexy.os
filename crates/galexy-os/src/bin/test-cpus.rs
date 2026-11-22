//! Milestone 104: one kernel, booted at `-smp 4` and at `-smp 8`.
//!
//! Prints `online=<n>` and relies on `boot_aps` to print `online` for
//! each AP rank. Does not move the shell (this kernel has none). Does
//! not raise `MAX_CPUS`.

#![no_std]
#![no_main]

use bootloader_api::{entry_point, BootInfo};
use galexy_os::{arch, exit_qemu, serial_println, QemuExitCode};

entry_point!(test_main_entry, config = &galexy_os::BOOTLOADER_CONFIG);

fn test_main_entry(boot_info: &'static mut BootInfo) -> ! {
    galexy_os::init();
    arch::mm::init(boot_info);
    arch::init(boot_info);

    let online = arch::cpu::online();
    let madt = arch::madt().cpus() as usize;
    serial_println!("[test-cpus] online={}", online);
    assert_eq!(online, madt, "every enabled CPU must come online");
    assert!(online == 4 || online == 8, "this boot is -smp 4 or -smp 8");
    serial_println!("[test-cpus] passed");
    exit_qemu(QemuExitCode::Success);
}
