//! The normal galexy.os kernel: boot, init, echo shell main loop.

#![no_std]
#![no_main]

use bootloader_api::{entry_point, BootInfo};
use galexy_os::{drivers::screen, echo, println, serial_println};

entry_point!(kernel_main);

/// Runs once at boot: initializes subsystems in dependency order, then serves
/// as the main loop.
fn kernel_main(boot_info: &'static mut BootInfo) -> ! {
    galexy_os::init(); // serial first: everything logs through it
    screen::init(boot_info);
    println!("Hello from galexy.os!");
    serial_println!(
        "boot info: rsdp_addr = {:?}, physical_memory_offset = {:?}",
        boot_info.rsdp_addr,
        boot_info.physical_memory_offset
    );
    galexy_os::arch::init(); // interrupts last to init: handlers depend on drivers
    echo::init();
    loop {
        // Serve input while keys are queued, then sleep until the next
        // interrupt wakes us.
        echo::poll();
        x86_64::instructions::hlt();
    }
}
