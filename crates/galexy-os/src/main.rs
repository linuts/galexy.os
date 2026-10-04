//! galexy.os kernel entry point.
//!
//! `kernel_main` receives the `BootInfo` handed over by the bootloader
//! (physical memory map, framebuffer, ...) and is where all subsystems get
//! initialized, in dependency order.

#![no_std]
#![no_main]
#![feature(abi_x86_interrupt)]
#![deny(clippy::all)]

mod echo;
mod interrupts;
mod keyboard;
mod macros;
mod screen;
mod serial;

use bootloader_api::{entry_point, BootInfo};
use core::panic::PanicInfo;

entry_point!(kernel_main);

/// Runs once at boot: initializes subsystems in dependency order, then serves
/// as the main loop.
fn kernel_main(boot_info: &'static mut BootInfo) -> ! {
    init();
    screen::init(boot_info);
    println!("Hello from galexy.os!");
    serial_println!(
        "boot info: rsdp_addr = {:?}, physical_memory_offset = {:?}",
        boot_info.rsdp_addr,
        boot_info.physical_memory_offset
    );
    interrupts::init();
    echo::init();
    loop {
        // Serve input while keys are queued, then sleep until the next
        // interrupt wakes us.
        echo::poll();
        x86_64::instructions::hlt();
    }
}

/// Brings up subsystems; called before any I/O.
pub fn init() {
    serial::init();
}

/// Panic handler: last resort. Reports the panic over serial (safe while
/// other locks may be held) and spins forever.
#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    serial_println!("[PANIC] {}", info);
    loop {
        x86_64::instructions::hlt();
    }
}
