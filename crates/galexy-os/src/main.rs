//! The normal galexy.os kernel: boot, init, feature banner, shell loop.

#![no_std]
#![no_main]

use bootloader_api::{entry_point, BootInfo};
use galexy_os::{banner, drivers::screen, sched, serial_println, shell};

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
    sched::init();
    sched::demo::spawn_all(); // silent preemptive threads
    banner::show();
    let mut last_second = 0u64;
    loop {
        // Status bar refresh, once per second (timer-driven).
        let second = galexy_os::arch::timer_ticks() / 1000;
        if second != last_second {
            last_second = second;
            shell::render_status_bar();
        }
        // Serve input, reap exited threads, sweep tasks, then sleep until
        // the next interrupt.
        shell::poll();
        sched::reap();
        sched::run();
        x86_64::instructions::hlt();
    }
}
