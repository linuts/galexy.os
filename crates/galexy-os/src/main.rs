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
    // Hand the framebuffer to the seats: wipe the banner so init/shell
    // cannot race mid-glyph with leftover [ok] lines beside Login as:.
    screen::clear_screen();
    // One ring-3 shell per F-key, each pinned to the BSP. F1 stays named
    // `shell`. The kernel loop paints a console switch, drains launches,
    // and keeps the status bar. Without that ELF, the in-kernel line
    // editor stays the consumer of TTY 0.
    // Milestone 53/54: load userspace init as orphan root. When init is
    // present it owns seat lifecycle (no kernel ensure_shell).
    let have_init = sched::spawn_init();
    let user_shell = sched::ramdisk::find("shell").is_some();
    if have_init {
        serial_println!("[boot] seats supervised by init");
    } else if user_shell {
        sched::spawn_all_shells();
    } else {
        serial_println!("[boot] no shell program on the ramdisk; kernel shell stays");
        shell::init();
    }
    serial_println!("[boot] main loop ready");
    let mut last_second = 0u64;
    loop {
        // Status bar refresh, once per second (timer-driven).
        let second = galexy_os::arch::timer_ticks() / 1000;
        if second != last_second {
            last_second = second;
            shell::render_status_bar();
            // Write-back: coalesce dirty galfs mutates into one dual-slot flush/sec.
            sched::galfs::sync_if_dirty();
        }
        // A queued launch loads on this loop (kernel page table). The
        // in-kernel editor only consumes keys when no ring-3 shell owns them.
        // F1–F12 only record a switch; this loop paints it.
        sched::drain_spawn();
        sched::poll_idle_logouts();
        screen::apply_tty_switch();
        if user_shell && !have_init {
            sched::ensure_shell();
        }
        if !user_shell && !have_init {
            shell::poll();
        }
        sched::reap();
        sched::run();
        // Tickless idle: one-shot until next second (or quantum if busy).
        sched::arm_timer_for_load();
        x86_64::instructions::hlt();
    }
}
