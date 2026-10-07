//! Integration test: Milestone 55 foreground job Cap / Ctrl-C.
//!
//! Spawns `linger`, pins it as TTY 0's foreground job, then synthesizes
//! Ctrl-C via [`sched::interrupt_foreground`]. Linger must exit (137)
//! while the test harness keeps running.

#![no_std]
#![no_main]

extern crate alloc;

use bootloader_api::{entry_point, BootInfo};
use galexy_os::{
    arch::mm, drivers::screen, exit_qemu, println, sched, serial_println, QemuExitCode,
};

entry_point!(test_main_entry, config = &galexy_os::BOOTLOADER_CONFIG);

const TICK_TIMEOUT: u64 = 12_000;

fn test_main_entry(boot_info: &'static mut BootInfo) -> ! {
    galexy_os::init();
    screen::init(boot_info);
    println!("[test-jobcap] running");
    serial_println!("[test-jobcap] running");

    let Some(ramdisk_addr) = boot_info.ramdisk_addr.into_option() else {
        panic!("no ramdisk");
    };
    mm::init(boot_info);
    galexy_os::arch::init(boot_info);
    sched::init();
    let archive = unsafe {
        core::slice::from_raw_parts(
            x86_64::VirtAddr::new(ramdisk_addr).as_ptr::<u8>(),
            boot_info.ramdisk_len as usize,
        )
    };
    sched::ramdisk::init(archive);

    let Some(bytes) = sched::ramdisk::find("linger") else {
        panic!("linger missing");
    };
    let _ = sched::loader::spawn_program("linger", bytes);

    let mut elapsed = 0u64;
    let slot = loop {
        x86_64::instructions::hlt();
        sched::reap();
        if let Some(s) = sched::slot_of_name("linger") {
            break s;
        }
        elapsed += 1;
        if elapsed > TICK_TIMEOUT {
            panic!("linger never started");
        }
    };

    sched::set_foreground_for_test(0, slot);
    assert!(
        sched::interrupt_foreground(0),
        "Ctrl-C must stop the foreground job"
    );
    assert!(
        !sched::interrupt_foreground(0),
        "second Ctrl-C is a no-op without a new foreground"
    );

    elapsed = 0;
    loop {
        x86_64::instructions::hlt();
        sched::reap();
        if !sched::is_name_live("linger") {
            break;
        }
        elapsed += 1;
        if elapsed > TICK_TIMEOUT {
            panic!("linger never exited after Ctrl-C");
        }
    }

    println!("[test-jobcap] foreground Ctrl-C works");
    serial_println!("[test-jobcap] passed");
    exit_qemu(QemuExitCode::Success);
}
