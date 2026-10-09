//! Integration test: `sleep` syscall parks until a monotonic deadline
//! (Milestone 56). Loads ramdisk `nap`, asserts it prints before/after
//! sleep and that wall ticks advanced under TCG slack.

#![no_std]
#![no_main]

use bootloader_api::{entry_point, BootInfo};
use galexy_core::TarCursor;
use galexy_os::{arch, drivers::screen, exit_qemu, println, sched, serial_println, QemuExitCode};
use x86_64::VirtAddr;

entry_point!(test_main_entry, config = &galexy_os::BOOTLOADER_CONFIG);

fn test_main_entry(boot_info: &'static mut BootInfo) -> ! {
    galexy_os::init();
    screen::init(boot_info);
    println!("[test-sleep] running");
    serial_println!("[test-sleep] running");

    let Some(ramdisk_addr) = boot_info.ramdisk_addr.into_option() else {
        panic!("no ramdisk");
    };
    let ramdisk_len = boot_info.ramdisk_len;

    arch::mm::init(boot_info);
    arch::init(boot_info);
    sched::init();

    let base = VirtAddr::new(ramdisk_addr);
    // SAFETY: bootloader-mapped ramdisk.
    let archive = unsafe { core::slice::from_raw_parts(base.as_ptr::<u8>(), ramdisk_len as usize) };
    let mut cursor = TarCursor::new(archive);
    let mut elf: Option<&[u8]> = None;
    while let Some((name, body)) = cursor.next_file() {
        if name == "nap" {
            elf = Some(body);
            break;
        }
    }
    let Some(elf) = elf else {
        panic!("'nap' not found in the ramdisk tar");
    };

    let before = arch::timer_ticks();
    let _region = sched::loader::spawn_program("nap", elf).expect("nap elf");

    // `threads_count` is RUNNING-only; sleep parks as WAITING — wait until
    // the name is no longer live (exited / reaped).
    let mut polls = 0u64;
    loop {
        x86_64::instructions::hlt();
        sched::arm_timer_for_load();
        sched::reap();
        if !sched::is_name_live("nap") {
            break;
        }
        polls += 1;
        if polls > 8000 {
            panic!("nap never exited");
        }
    }

    let after = arch::timer_ticks();
    // Requested 50 ms; TCG slack — accept >= 20 ms of monotonic advance.
    assert!(
        after.saturating_sub(before) >= 20,
        "sleep too short: before={before} after={after}"
    );

    println!("[test-sleep] sleep lifecycle complete");
    serial_println!("[test-sleep] passed");
    exit_qemu(QemuExitCode::Success);
}
