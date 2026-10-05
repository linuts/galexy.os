//! Integration test kernel: CRASH ISOLATION. A ring-3 blob recurses
//! (`call` loop) until its stack walks into the GUARD page below the user
//! stack — the page fault kills ONLY the faulting task; the kernel keeps
//! running, reaps the task's whole page-table tree, and asserts the
//! allocator returns to baseline exactly. The first "the OS survives user
//! bugs" property.

#![no_std]
#![no_main]

use bootloader_api::{entry_point, BootInfo};
use galexy_os::{drivers::screen, exit_qemu, println, sched, serial_println, QemuExitCode};

entry_point!(test_main_entry, config = &galexy_os::BOOTLOADER_CONFIG);

/// Blob: infinite recursion — `call rel32` to itself. Each nesting level
/// pushes a return address (8 bytes); the 16 KiB stack fills in ~512
/// levels, then the walk hits the guard page below the stack base.
/// Encoded: offset 0: E8 FB FF FF FF (call rel32 −5 → target 0).
const RECURSE_BLOB: [u8; 5] = [0xE8, 0xFB, 0xFF, 0xFF, 0xFF];

fn test_main_entry(boot_info: &'static mut BootInfo) -> ! {
    galexy_os::init();
    screen::init(boot_info);
    println!("[test-userfault] running");
    serial_println!("[test-userfault] running");

    galexy_os::arch::mm::init(boot_info);
    galexy_os::arch::init(boot_info);
    sched::init();

    let baseline = galexy_os::arch::mm::free_frames();
    let main_ticks_before = sched::main_ticks();

    sched::spawn_user_task("boomer", |_| RECURSE_BLOB.to_vec());

    // Main loop: hlt + rotations. The blob faults within its first quantum;
    // the PF path tombstones it; the reaper frees its tree.
    loop {
        x86_64::instructions::hlt();
        sched::reap();
        if sched::threads_count() == 0 {
            break;
        }
    }

    // The kernel must have kept rotating (the fault path is a third
    // context-handoff entry point — it must enter the main loop cleanly).
    let main_ticks_after = sched::main_ticks();
    assert!(
        main_ticks_after > main_ticks_before + 1,
        "main loop must receive quanta after the fault: {} -> {}",
        main_ticks_before,
        main_ticks_after
    );

    // Exact accounting: the faulted task's tree is fully reclaimed
    // (data frames + page-table frames).
    let now = galexy_os::arch::mm::free_frames();
    assert_eq!(
        now, baseline,
        "faulted task's tree must be fully reclaimed: {} vs {}",
        baseline, now
    );

    println!("[test-userfault] the OS survived a ring-3 crash");
    println!("[test-userfault] all assertions passed");
    serial_println!("[test-userfault] passed");
    exit_qemu(QemuExitCode::Success);
}
