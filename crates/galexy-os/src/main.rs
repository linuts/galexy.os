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

    // Ramdisk: the bootloader-mapped tar, published so a typed name can start.
    if let Some(ramdisk_addr) = boot_info.ramdisk_addr.into_option() {
        // SAFETY: the bootloader mapped the contiguous ramdisk image at
        // [ramdisk_addr, +len) into the kernel's (and thus every) space.
        let archive = unsafe {
            core::slice::from_raw_parts(
                x86_64::VirtAddr::new(ramdisk_addr).as_ptr::<u8>(),
                boot_info.ramdisk_len as usize,
            )
        };
        sched::ramdisk::init(archive);
    } else {
        serial_println!("[boot] no ramdisk handed to the kernel");
    }

    galexy_os::arch::mm::init(boot_info); // frames + paging + heap
    galexy_os::arch::init(boot_info); // interrupts last to init: handlers depend on drivers
    sched::init();
    sched::demo::spawn_all(); // silent preemptive threads
    banner::show();
    // The interactive shell is a ring-3 program pinned to the BSP. The
    // kernel loop only drains its spawn requests and keeps the status bar.
    // Without that ELF, the in-kernel line editor stays the consumer.
    let user_shell = if let Some(bytes) = sched::ramdisk::find("shell") {
        sched::loader::spawn_program_bsp("shell", bytes);
        true
    } else {
        serial_println!("[boot] no shell program on the ramdisk; kernel shell stays");
        shell::init();
        false
    };
    serial_println!("[boot] main loop ready");
    let mut last_second = 0u64;
    loop {
        // Status bar refresh, once per second (timer-driven).
        let second = galexy_os::arch::timer_ticks() / 1000;
        if second != last_second {
            last_second = second;
            shell::render_status_bar();
        }
        // A queued launch loads on this loop (kernel page table). The
        // in-kernel editor only consumes keys when no ring-3 shell owns them.
        // A faulted shell is loaded again; other tasks keep running.
        sched::drain_spawn();
        if user_shell {
            sched::ensure_shell();
        }
        if !user_shell {
            shell::poll();
        }
        sched::reap();
        sched::run();
        x86_64::instructions::hlt();
    }
}
