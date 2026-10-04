//! galexy.os kernel entry point.
//!
//! This file is **wiring only**: subsystem init in dependency order, then the
//! main loop. All logic lives in `kcore/`, `arch/`, `drivers/`, `sched/`,
//! and the shell module — see `docs/DESIGN.md` for the boundary rules.

#![no_std]
#![no_main]
#![feature(abi_x86_interrupt)]
#![deny(clippy::all)]

mod arch;
mod drivers;
mod echo;
mod kcore;
mod macros;
mod sched;

use bootloader_api::{entry_point, BootInfo};
use core::panic::PanicInfo;

entry_point!(kernel_main);

/// Runs once at boot: initializes subsystems in dependency order, then serves
/// as the main loop.
fn kernel_main(boot_info: &'static mut BootInfo) -> ! {
    drivers::serial::init(); // serial first: everything logs through it
    drivers::screen::init(boot_info);
    println!("Hello from galexy.os!");
    serial_println!(
        "boot info: rsdp_addr = {:?}, physical_memory_offset = {:?}",
        boot_info.rsdp_addr,
        boot_info.physical_memory_offset
    );
    arch::init(); // interrupts last to init: handlers depend on drivers
    echo::init();
    loop {
        // Serve input while keys are queued, then sleep until the next
        // interrupt wakes us.
        echo::poll();
        x86_64::instructions::hlt();
    }
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
