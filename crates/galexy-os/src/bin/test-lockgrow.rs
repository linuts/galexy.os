//! Integration test kernel: heap growth under a held lock versus an IF=0
//! lock waiter on the other CPU (galexy.os#86).
//!
//! The production hang: the BSP's 1 Hz status bar allocated while holding
//! `THREADS` (IF=0); the allocation missed the fast path and grew the heap,
//! which broadcasts a TLB-shootdown IPI and spins for the AP's ack; the AP
//! was inside the `passwd` syscall (IF=0) spinning on `THREADS`. Neither
//! CPU could move: the AP cannot take the IPI with IF=0, the BSP will not
//! release the lock until the IPI is acked. No panic, uptime frozen.
//!
//! This kernel reproduces that shape deterministically, under `-smp 2`:
//! - the AP runs a contender that disables interrupts and takes/drops a
//!   shared lock in a tight loop, never re-enabling IF;
//! - the BSP takes the same lock inside an IRQ gate and allocates more
//!   than the heap has free, forcing `grow()` and its broadcast while the
//!   lock is held.
//!
//! Without the fix the BSP spins forever (the harness times out and the
//! serial shows `[shootdown] cpu 0 waiting on cpu 1`). With it, the AP's
//! lock spin services the mailbox, the broadcast completes, the BSP drops
//! the lock, and the contender's acquire count keeps climbing. Asserts the
//! heap grew, broadcasts happened, and the contender made progress across
//! every growth.

#![no_std]
#![no_main]

extern crate alloc;

use bootloader_api::{entry_point, BootInfo};
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use galexy_os::sync::Mutex;
use galexy_os::{arch, drivers::screen, exit_qemu, println, sched, serial_println, QemuExitCode};
use x86_64::instructions::interrupts;

/// The contended lock. Payload is irrelevant; the spin is the test.
static LOCK: Mutex<u64> = Mutex::new(0);
/// Set by the BSP when the contender should re-enable IF and return.
static STOP: AtomicBool = AtomicBool::new(false);
/// Set by the contender once it is spinning IF=0 on the AP.
static CONTENDING: AtomicBool = AtomicBool::new(false);
/// Successful acquisitions by the contender.
static ACQUIRED: AtomicU64 = AtomicU64::new(0);

/// Growth rounds the BSP forces while holding `LOCK`.
const ROUNDS: usize = 4;

/// Returns immediately: pads the round-robin so the next spawn pins to the AP.
extern "C" fn pad() {}

/// Lives on the AP. Interrupts stay OFF for the whole loop: this is the
/// `passwd`-in-a-syscall shape (the timer cannot preempt it, and the
/// shootdown IPI cannot be delivered). Only the lock's own spin loop can
/// ack a broadcast.
extern "C" fn contender() {
    interrupts::disable();
    CONTENDING.store(true, Ordering::Release);
    while !STOP.load(Ordering::Acquire) {
        {
            let mut g = LOCK.lock();
            *g += 1;
        }
        ACQUIRED.fetch_add(1, Ordering::Release);
    }
    interrupts::enable();
}

fn test_main_entry(boot_info: &'static mut BootInfo) -> ! {
    galexy_os::init();
    screen::init(boot_info);
    println!("[test-lockgrow] running");
    serial_println!("[test-lockgrow] running");

    arch::mm::init(boot_info);
    arch::init(boot_info);
    sched::init();
    assert_eq!(arch::cpu::online(), 2, "two CPUs must be live");

    let pad_owner = sched::spawn_thread("pad", pad);
    let contender_owner = sched::spawn_thread("contender", contender);
    assert_eq!(pad_owner, 0, "first spawn pins to the BSP");
    assert_eq!(contender_owner, 1, "the contender must live on the AP");

    // Wait until the AP is really inside the IF=0 loop.
    let deadline = arch::timer_ticks() + 5000;
    while !CONTENDING.load(Ordering::Acquire) {
        assert!(
            arch::timer_ticks() < deadline,
            "the contender never started on the AP"
        );
        x86_64::instructions::hlt();
    }
    serial_println!("[test-lockgrow] contender spinning IF=0 on the AP");

    let heap_before = arch::mm::heap::stats().1;
    let broadcasts_before = arch::mm::shootdown::broadcast_count();

    for round in 0..ROUNDS {
        let acquired_before = ACQUIRED.load(Ordering::Acquire);
        let size_before = arch::mm::heap::stats().1;
        // Hold LOCK with IF=0 and allocate past the free space: the
        // fast path fails, `grow()` maps a chunk and broadcasts while we
        // still hold the lock the AP is spinning on.
        interrupts::without_interrupts(|| {
            let mut g = LOCK.lock();
            let want = arch::mm::heap::free_bytes() + 4096;
            let buf: alloc::vec::Vec<u8> = alloc::vec![0xA5u8; want];
            *g += u64::from(buf[want / 2]);
            drop(buf);
        });
        let size_after = arch::mm::heap::stats().1;
        serial_println!(
            "[test-lockgrow] round {}: heap {} -> {} KiB, broadcasts {}",
            round,
            size_before / 1024,
            size_after / 1024,
            arch::mm::shootdown::broadcast_count()
        );
        assert!(
            size_after > size_before,
            "round {round}: the allocation must have grown the heap"
        );
        // The contender must get the lock back after each growth.
        let progress_deadline = arch::timer_ticks() + 2000;
        while ACQUIRED.load(Ordering::Acquire) == acquired_before {
            assert!(
                arch::timer_ticks() < progress_deadline,
                "round {round}: the contender never re-acquired the lock"
            );
            core::hint::spin_loop();
        }
    }

    let heap_after = arch::mm::heap::stats().1;
    let broadcasts_after = arch::mm::shootdown::broadcast_count();
    assert!(heap_after > heap_before, "the heap must have grown");
    assert!(
        broadcasts_after >= broadcasts_before + ROUNDS as u64,
        "every growth must have broadcast: {broadcasts_before} -> {broadcasts_after}"
    );
    // The AP acked every one of them while IF=0 (it never left the loop).
    let seen = arch::mm::shootdown::seen_by(1);
    assert!(
        seen.iter().any(|&s| s > 0),
        "the AP must have acked shootdowns from its IF=0 lock spin: {seen:?}"
    );

    STOP.store(true, Ordering::Release);
    let drain_deadline = arch::timer_ticks() + 5000;
    loop {
        x86_64::instructions::hlt();
        sched::reap();
        if sched::unreaped_threads() == 0 {
            break;
        }
        assert!(
            arch::timer_ticks() < drain_deadline,
            "the contender never exited"
        );
    }
    serial_println!(
        "[test-lockgrow] contender acquired {} times across {} growths",
        ACQUIRED.load(Ordering::Relaxed),
        ROUNDS
    );

    println!("[test-lockgrow] all assertions passed");
    serial_println!("[test-lockgrow] passed");
    exit_qemu(QemuExitCode::Success);
}

entry_point!(test_main_entry, config = &galexy_os::BOOTLOADER_CONFIG);
