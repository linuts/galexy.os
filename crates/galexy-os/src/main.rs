//! The normal galexy.os kernel: boot, init, feature banner, echo shell.

#![no_std]
#![no_main]

use bootloader_api::{entry_point, BootInfo};
use galexy_os::{banner, drivers::screen, echo, serial_println};

entry_point!(kernel_main, config = &galexy_os::BOOTLOADER_CONFIG);

/// Runs once at boot: initializes subsystems in dependency order, shows the
/// banner, then serves as the shell main loop.
fn kernel_main(boot_info: &'static mut BootInfo) -> ! {
    galexy_os::init(); // serial first: everything logs through it
    screen::init(boot_info);
    serial_println!(
        "boot info: rsdp_addr = {:?}, physical_memory_offset = {:?}",
        boot_info.rsdp_addr,
        boot_info.physical_memory_offset
    );
    galexy_os::arch::mm::init(boot_info); // frames + paging + heap
    galexy_os::arch::init(); // interrupts last to init: handlers depend on drivers
    galexy_os::sched::init();
    galexy_os::sched::demo::spawn_all();
    banner::show();
    loop {
        // Serve input, sweep tasks, then sleep until the next interrupt.
        echo::poll();
        galexy_os::sched::run();
        x86_64::instructions::hlt();
    }
}
