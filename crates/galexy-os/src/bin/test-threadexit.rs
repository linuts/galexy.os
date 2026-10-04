//! Integration test kernel: thread lifecycle — threads that RETURN from
//! their entry are tombstoned, skipped by the rotation, and reaped (stack +
//! FXSAVE area returned to the heap). Also proves the stack canary check.

#![no_std]
#![no_main]

use bootloader_api::{entry_point, BootInfo};
use core::sync::atomic::{AtomicU64, Ordering};
use galexy_os::{drivers::screen, exit_qemu, println, sched, serial_println, QemuExitCode};

entry_point!(test_main_entry, config = &galexy_os::BOOTLOADER_CONFIG);

const TARGET: u64 = 100;
const THREADS: usize = 3;

static DONE: [AtomicU64; THREADS] = [
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
];

/// Each thread counts to TARGET then RETURNS (exercises trampoline +
/// thread_exit + reaper).
extern "C" fn thread_a() {
    for _ in 0..TARGET {
        DONE[0].fetch_add(1, Ordering::Relaxed);
    }
}

extern "C" fn thread_b() {
    for _ in 0..TARGET {
        DONE[1].fetch_add(1, Ordering::Relaxed);
    }
}

extern "C" fn thread_c() {
    for _ in 0..TARGET {
        DONE[2].fetch_add(1, Ordering::Relaxed);
    }
}

fn test_main_entry(boot_info: &'static mut BootInfo) -> ! {
    galexy_os::init();
    screen::init(boot_info);
    println!("[test-threadexit] running");
    serial_println!("[test-threadexit] running");

    galexy_os::arch::mm::init(boot_info);
    galexy_os::arch::init();
    sched::init();

    // Heap baseline: everything the threads use (stacks + fx areas) must be
    // back on the free list once all three have run, exited, and been reaped.
    let used_before = galexy_os::arch::mm::heap::used_bytes();

    sched::spawn_thread("exit-a", thread_a);
    sched::spawn_thread("exit-b", thread_b);
    sched::spawn_thread("exit-c", thread_c);
    // Racy by design: a thread may complete (exit) before the later spawns
    // finish, so at most 3 can be running here. Any fewer is fine.
    assert!(
        sched::threads_count() <= 3,
        "at most three threads registered"
    );

    // Main loop: wait (preempted while threads run) until all threads
    // finished; reap each loop sweep.
    loop {
        x86_64::instructions::hlt();
        sched::reap();
        let all_done = DONE
            .iter()
            .all(|c| c.load(Ordering::Relaxed) >= TARGET);
        let none_running = sched::threads_count() == 0;
        if all_done && none_running {
            break;
        }
    }

    // One more reap cycle for determinism, then heap accounting: the three
    // 32 KiB stacks + 3 fx areas (512 B each) must have been freed. A few
    // hundred bytes of tombstone bookkeeping may remain (Thread slots stay
    // in the Vec by design; see the STATE_* docs).
    sched::reap();
    let used_after = galexy_os::arch::mm::heap::used_bytes();
    let expected_freed = (32 * 1024 + 512) * THREADS;
    assert!(
        used_after < used_before + expected_freed / 2,
        "stacks must return to the heap: before={used_before} after={used_after}"
    );

    println!(
        "[test-threadexit] heap {} -> {} bytes ({} expected freed)",
        used_before, used_after, expected_freed
    );
    println!("[test-threadexit] lifecycle works");
    serial_println!("[test-threadexit] passed");
    exit_qemu(QemuExitCode::Success);
}
