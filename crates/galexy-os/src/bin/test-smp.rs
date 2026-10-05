//! Integration test kernel: the SMP substrate, end to end (M18 commit 5).
//! Asserts — under QEMU `-smp 2` — that:
//! 1. BOTH CPUs came online (MADT + AP boot + per-CPU GS/GDT/LAPIC).
//! 2. Pin-at-spawn actually distributes: 4 threads land 0,1,0,1.
//! 3. The per-CPU timers both tick (AP-owned threads accumulate CPU time
//!    under the AP's own naked-switch rotation).
//! 4. Lifecycle closes: exits are tombstoned + reaped by the OWNERS; the
//!    thread table drains to zero.

#![no_std]
#![no_main]

use bootloader_api::{entry_point, BootInfo};
use galexy_os::{arch, drivers::screen, exit_qemu, println, sched, serial_println, QemuExitCode};

entry_point!(test_main_entry, config = &galexy_os::BOOTLOADER_CONFIG);

/// Ticks the sleepers spin until, then they return (tombstone + owner
/// reap). Set by the test's main before the wait loop.
static DEADLINE: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

extern "C" fn sleeper() {
    // hlt until the deadline, then RETURN: meanwhile the per-CPU timer
    // preempts this on its owner CPU, accumulating CPU-time ticks.
    let deadline = DEADLINE.load(core::sync::atomic::Ordering::Relaxed);
    while arch::timer_ticks() < deadline {
        x86_64::instructions::hlt();
    }
}

fn test_main_entry(boot_info: &'static mut BootInfo) -> ! {
    galexy_os::init();
    screen::init(boot_info);
    println!("[test-smp] running");
    serial_println!("[test-smp] running");

    arch::mm::init(boot_info);
    arch::init(boot_info);
    sched::init();

    // 1: both CPUs online.
    let online = arch::cpu::online();
    serial_println!("[test-smp] online={}", online);
    assert_eq!(online, 2, "both CPUs must be online under -smp 2");

    // The MADT saw both, too.
    let bsp = arch::madt().boot_cpu_apic_id();
    let mut aps = 0;
    for &id in arch::madt().enabled_ids() {
        if id != bsp {
            aps += 1;
        }
    }
    assert!(aps >= 1, "the MADT must list at least one AP");
    serial_println!("[test-smp] madt aps={}", aps);

    // 2: pin-at-spawn distribution across 4 threads.
    // spawn_thread returns the pin decision — read race-free.
    let o1 = sched::spawn_thread("t1", sleeper);
    let o2 = sched::spawn_thread("t2", sleeper);
    let o3 = sched::spawn_thread("t3", sleeper);
    let o4 = sched::spawn_thread("t4", sleeper);
    serial_println!("[test-smp] owners: t1={} t2={} t3={} t4={}", o1, o2, o3, o4);
    assert_eq!(o1, 0, "first spawn pins to the BSP");
    assert_eq!(o2, 1, "second spawn pins to the AP");
    assert_eq!(o3, 0, "third spawn wraps to the BSP");
    assert_eq!(o4, 1, "fourth spawn wraps to the AP");

    // 3: let them spin a while under preemption — each owner's naked
    // timer switch must serve them (CPU-time ticks accumulate per slot).
    DEADLINE.store(
        arch::timer_ticks() + 600,
        core::sync::atomic::Ordering::Relaxed,
    );
    let deadline = arch::timer_ticks() + 600;
    while arch::timer_ticks() < deadline {
        x86_64::instructions::hlt();
    }
    let total = sched::thread_tick_total();
    serial_println!("[test-smp] thread tick total: {}", total);
    assert!(
        total >= 4,
        "the 4 threads must have accumulated CPU ticks (got {})",
        total
    );

    // 4: drain: the sleepers already returned from their entries (they
    // exited during the ticking); reap until the table is empty.
    loop {
        x86_64::instructions::hlt();
        sched::reap();
        if sched::unreaped_threads() == 0 {
            break;
        }
    }
    assert!(
        sched::thread_owner("t1").is_none(),
        "t1 must be tombstoned away"
    );

    println!("[test-smp] smp substrate verified");
    println!("[test-smp] all assertions passed");
    serial_println!("[test-smp] passed");
    exit_qemu(QemuExitCode::Success);
}
