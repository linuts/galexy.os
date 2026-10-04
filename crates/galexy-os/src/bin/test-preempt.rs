//! Integration test kernel: timer-preemptive kernel threads — two threads
//! that NEVER yield, both must make progress (round-robin via the PIT).

#![no_std]
#![no_main]

use bootloader_api::{entry_point, BootInfo};
use core::sync::atomic::{AtomicU64, Ordering};
use galexy_os::{drivers::screen, exit_qemu, println, sched, serial_println, QemuExitCode};

entry_point!(test_main_entry, config = &galexy_os::BOOTLOADER_CONFIG);

static COUNT_A: AtomicU64 = AtomicU64::new(0);
static COUNT_B: AtomicU64 = AtomicU64::new(0);

/// Thread A: increments forever, never yields.
extern "C" fn thread_a() {
    loop {
        COUNT_A.fetch_add(1, Ordering::Relaxed);
    }
}

/// Thread B: increments forever, never yields.
extern "C" fn thread_b() {
    loop {
        COUNT_B.fetch_add(1, Ordering::Relaxed);
    }
}

/// Progress threshold: both counters must exceed this (a non-preempting
/// scheduler leaves the second counter at 0 and fails the test).
const PROGRESS: u64 = 5_000_000;

fn test_main_entry(boot_info: &'static mut BootInfo) -> ! {
    galexy_os::init();
    screen::init(boot_info);
    println!("[test-preempt] running");
    serial_println!("[test-preempt] running");

    galexy_os::arch::mm::init(boot_info); // frames + paging + heap
    galexy_os::arch::init(); // interrupts: the timer is the preemptor
    sched::init();
    // Spawn the threads.
    sched::spawn_thread("count-a", thread_a);
    sched::spawn_thread("count-b", thread_b);
    assert_eq!(sched::threads_count(), 2, "two threads registered");

    // Main loop: wait (preempted by the timer while threads run) until both
    // threads made progress.
    loop {
        x86_64::instructions::hlt();
        let a = COUNT_A.load(Ordering::Relaxed);
        let b = COUNT_B.load(Ordering::Relaxed);
        if a > PROGRESS && b > PROGRESS {
            break;
        }
    }

    // Round-robin fairness: both threads got meaningful CPU time.
    let a = COUNT_A.load(Ordering::Relaxed);
    let b = COUNT_B.load(Ordering::Relaxed);
    let (hi, lo) = (a.max(b), a.min(b));
    assert!(lo > PROGRESS, "both threads must progress: a={a} b={b}");
    assert!(hi / lo < 4, "round-robin fairness: a={a} b={b}");

    println!("[test-preempt] a={} b={}", a, b);
    println!("[test-preempt] preemption works");
    serial_println!("[test-preempt] passed");
    exit_qemu(QemuExitCode::Success);
}
