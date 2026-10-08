//! Integration test: steal fairness under load (Milestone 51). Under
//! QEMU `-smp 2`, both CPUs must do useful work, and when one CPU's
//! rotation drains early it must steal from the other instead of idling
//! while runnable threads queue there.
//!
//! Three never-yielding workers count iterations per (worker, CPU):
//! `w0` and `w2` pin to the BSP (first and third spawn), `w1` to the AP
//! (second spawn). `w1` stops after a short deadline, the other two run
//! to a long one. Asserts: every worker made progress; both CPUs
//! accumulated work; after `w1` left, a BSP-pinned worker ran on the AP
//! (a real steal, confirmed by `steal_count`); and the two long workers
//! finished within a 4× band of each other.

#![no_std]
#![no_main]

use bootloader_api::{entry_point, BootInfo};
use core::sync::atomic::{AtomicU64, Ordering};
use galexy_os::{arch, drivers::screen, exit_qemu, println, sched, serial_println, QemuExitCode};

entry_point!(test_main_entry, config = &galexy_os::BOOTLOADER_CONFIG);

const WORKERS: usize = 3;
const CPUS: usize = 2;
/// Ticks (ms) the short worker runs. An idle AP sleeps tickless until
/// the next whole second, so the short deadline must outlive that.
const SHORT_TICKS: u64 = 1_500;
/// Ticks (ms) the long workers run.
const LONG_TICKS: u64 = 4_000;

static COUNT: [[AtomicU64; CPUS]; WORKERS] =
    [const { [const { AtomicU64::new(0) }; CPUS] }; WORKERS];
static DEADLINE: [AtomicU64; WORKERS] = [const { AtomicU64::new(0) }; WORKERS];

extern "C" fn worker<const I: usize>() {
    let until = DEADLINE[I].load(Ordering::Relaxed);
    while arch::timer_ticks() < until {
        let cpu = arch::cpu::current_index().min(CPUS - 1);
        COUNT[I][cpu].fetch_add(1, Ordering::Relaxed);
    }
}

fn total(i: usize) -> u64 {
    COUNT[i].iter().map(|c| c.load(Ordering::Relaxed)).sum()
}

fn on_cpu(cpu: usize) -> u64 {
    (0..WORKERS)
        .map(|i| COUNT[i][cpu].load(Ordering::Relaxed))
        .sum()
}

fn test_main_entry(boot_info: &'static mut BootInfo) -> ! {
    galexy_os::init();
    screen::init(boot_info);
    println!("[test-fairness] running");
    serial_println!("[test-fairness] running");

    arch::mm::init(boot_info);
    arch::init(boot_info);
    sched::init();
    assert_eq!(arch::cpu::online(), 2, "two CPUs must be live");

    let now = arch::timer_ticks();
    DEADLINE[0].store(now + LONG_TICKS, Ordering::Relaxed);
    DEADLINE[1].store(now + SHORT_TICKS, Ordering::Relaxed);
    DEADLINE[2].store(now + LONG_TICKS, Ordering::Relaxed);
    let steals_before = sched::steal_count();

    sched::spawn_thread("w0", worker::<0>);
    sched::spawn_thread("w1", worker::<1>);
    sched::spawn_thread("w2", worker::<2>);

    // Idle until every worker returned and was reaped.
    let give_up = now + LONG_TICKS * 3;
    loop {
        x86_64::instructions::hlt();
        sched::reap();
        if sched::unreaped_threads() == 0 {
            break;
        }
        assert!(
            arch::timer_ticks() < give_up,
            "workers never finished: {} unreaped",
            sched::unreaped_threads()
        );
    }

    let steals = sched::steal_count() - steals_before;
    for (i, counts) in COUNT.iter().enumerate() {
        serial_println!(
            "[test-fairness] w{} bsp={} ap={}",
            i,
            counts[0].load(Ordering::Relaxed),
            counts[1].load(Ordering::Relaxed)
        );
        assert!(total(i) > 0, "w{i} never ran");
    }
    serial_println!(
        "[test-fairness] cpu0={} cpu1={} steals={}",
        on_cpu(0),
        on_cpu(1),
        steals
    );

    assert!(on_cpu(0) > 0, "the BSP did no work");
    assert!(on_cpu(1) > 0, "the AP did no work");
    assert!(
        COUNT[1][1].load(Ordering::Relaxed) > 0,
        "w1 (second spawn) must have run on the AP"
    );
    let migrated = COUNT[0][1].load(Ordering::Relaxed) + COUNT[2][1].load(Ordering::Relaxed);
    assert!(
        migrated > 0,
        "after w1 left, the idle AP must steal a BSP worker (steals={steals})"
    );
    assert!(steals >= 1, "steal_count must record the migration");
    let (hi, lo) = (total(0).max(total(2)), total(0).min(total(2)));
    assert!(
        hi / lo.max(1) < 4,
        "long workers must share within 4x: w0={} w2={}",
        total(0),
        total(2)
    );

    println!("[test-fairness] both CPUs busy; idle AP stole a BSP worker");
    serial_println!("[test-fairness] passed");
    exit_qemu(QemuExitCode::Success);
}
