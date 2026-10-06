//! More exits than thread slots. The table holds 64 records; this exits
//! 80 short threads, one after another, and still reaches the success
//! exit. One thread stays running so its name is still listed.

#![no_std]
#![no_main]

use bootloader_api::{entry_point, BootInfo};
use core::sync::atomic::{AtomicU64, Ordering};
use galexy_os::{drivers::screen, exit_qemu, println, sched, serial_println, QemuExitCode};

entry_point!(test_main_entry, config = &galexy_os::BOOTLOADER_CONFIG);

/// Past the 64-slot cap. One slot stays with [`keeper`].
const WAVES: u64 = 80;

static DONE: AtomicU64 = AtomicU64::new(0);

extern "C" fn wave() {
    DONE.fetch_add(1, Ordering::Release);
}

extern "C" fn keeper() {
    loop {
        x86_64::instructions::hlt();
    }
}

fn test_main_entry(boot_info: &'static mut BootInfo) -> ! {
    galexy_os::init();
    screen::init(boot_info);
    println!("[test-reuse] running");
    serial_println!("[test-reuse] running");

    galexy_os::arch::mm::init(boot_info);
    galexy_os::arch::init(boot_info);
    sched::init();

    sched::spawn_thread("keeper", keeper);

    for i in 0..WAVES {
        sched::spawn_thread("wave", wave);
        loop {
            x86_64::instructions::hlt();
            sched::reap();
            let exited = DONE.load(Ordering::Acquire);
            // The keeper is the only thread that must still hold a slot.
            if exited > i && sched::unreaped_threads() == 1 {
                break;
            }
        }
    }

    let live = sched::thread_stats()
        .iter()
        .any(|(name, _)| name == "keeper");
    assert!(live, "keeper must still be listed");
    serial_println!("[test-reuse] keeper live, exited {}", WAVES);
    serial_println!("[test-reuse] passed");
    exit_qemu(QemuExitCode::Success);
}
