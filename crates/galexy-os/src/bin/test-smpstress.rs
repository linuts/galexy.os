//! Integration test kernel: SMP stress (M19). Under QEMU `-smp 2`:
//!
//! Phase A — steal proof: a spinner pinned to the BSP + a flash task on the
//! AP. The AP's rotation drains, it goes idle, and its idle-pass steal
//! migrates the spinner. Asserts the owner flip + ticks accumulated on the
//! AP + the cooldown holding the owner stable while observed.
//! Phase B — concurrent heap growth: two growers each grab 1 MiB at once;
//! both OOM onto the GROWING serialization (conflict-wait path) and every
//! chunk broadcasts a shootdown IPI. Asserts real broadcasts happened.
//! Phase C — hammer + churn + EXACT frame closure: two hammer threads cycle
//! alloc/free of 64 KiB buffers on both CPUs (touching every page: real TLB
//! traffic through the broadcast mappings) while a ring-3 blob runs its
//! full lifecycle; after a full drain, free_frames must return to the
//! phase-C baseline EXACTLY (heap size unchanged — no growth in the window).

#![no_std]
#![no_main]

extern crate alloc;

use bootloader_api::{entry_point, BootInfo};
use core::sync::atomic::{AtomicU64, Ordering};
use galexy_abi::{CapRights, Syscall};
use galexy_os::{arch, drivers::screen, exit_qemu, println, sched, serial_println, QemuExitCode};

entry_point!(test_main_entry, config = &galexy_os::BOOTLOADER_CONFIG);

/* ---------------- phase A: steal proof ---------------- */

static SPINNER_DEADLINE: AtomicU64 = AtomicU64::new(0);

/// Spins (preempted only) until the deadline, then returns. Never yields —
/// its only "sleep" is the parked-not-current state between quanta, which
/// is exactly the state an idle CPU's steal targets.
extern "C" fn spinner() {
    while arch::timer_ticks() < SPINNER_DEADLINE.load(Ordering::Relaxed) {
        x86_64::instructions::hlt();
    }
}

/// Returns immediately: tombstone + owner reap. Clears the AP's rotation.
extern "C" fn flash() {}

/* ---------------- phase B: concurrent growth ---------------- */

static GROWN: [AtomicU64; 2] = [AtomicU64::new(0), AtomicU64::new(0)];
static DROP_NOW: AtomicU64 = AtomicU64::new(0);

/// Grabs 1 MiB (forcing repeated OOM → grow → broadcast), marks done, holds
/// until told, drops, returns. Two of these run concurrently on both CPUs.
extern "C" fn grower(slot: usize) {
    let mut held = alloc::vec![0u8; 1024 * 1024];
    for (i, b) in held.iter_mut().enumerate() {
        *b = (i & 0xFF) as u8;
    }
    GROWN[slot].store(1, Ordering::Release);
    while DROP_NOW.load(Ordering::Relaxed) == 0 {
        x86_64::instructions::hlt();
    }
    drop(held);
}

extern "C" fn grower0() {
    grower(0);
}

extern "C" fn grower1() {
    grower(1);
}

/* ---------------- phase C: hammer + churn ---------------- */

const HAMMER_BUF: usize = 64 * 1024;
// TCG budget: each loop is a full touch+verify of 64 KiB; 10 loops per CPU
// still exercises the TLB/alloc paths but survives even a 30-way parallel
// suite run under host load (100-loop variant starved the deadline).
const HAMMER_LOOPS: usize = 10;
static HAMMER_DONE: [AtomicU64; 2] = [AtomicU64::new(0), AtomicU64::new(0)];

/// Cycles alloc/free of one 64 KiB buffer, touching every page (the TLB
/// pressure the broadcast machinery exists for). Bounded; then returns.
extern "C" fn hammer(slot: usize) {
    for i in 0..HAMMER_LOOPS {
        let mut buf = alloc::vec![0u8; HAMMER_BUF];
        for (p, b) in buf.iter_mut().enumerate() {
            *b = ((i + p) & 0xFF) as u8;
        }
        for (p, b) in buf.iter().enumerate() {
            assert_eq!(*b, ((i + p) & 0xFF) as u8, "hammer {} buf corrupted", slot);
        }
        drop(buf);
    }
    HAMMER_DONE[slot].store(1, Ordering::Release);
}

extern "C" fn hammer0() {
    hammer(0);
}

extern "C" fn hammer1() {
    hammer(1);
}

/* ---------------- ring-3 blob (phase C churn) ---------------- */

const DONE_MARK: u32 = 0x00BEEF;
const HELLO: &[u8] = b"stress: ring3 alive";

fn blob(code_vaddr: u64, console_cap: u64, scratch_addr: u64) -> alloc::vec::Vec<u8> {
    let mut code: alloc::vec::Vec<u8> = alloc::vec::Vec::new();
    code.extend_from_slice(&[0xEB, HELLO.len() as u8]); // jmp over message
    code.extend_from_slice(HELLO);
    code.extend_from_slice(&[0x48, 0xBF]);
    code.extend_from_slice(&(code_vaddr + 2).to_le_bytes()); // mov rsi, msg
    code.extend_from_slice(&[0xBA]);
    code.extend_from_slice(&(HELLO.len() as u32).to_le_bytes()); // mov rdx, len
    code.extend_from_slice(&[0x48, 0xBF]);
    code.extend_from_slice(&console_cap.to_le_bytes()); // mov rdi, cap
    code.extend_from_slice(&[0xB8]);
    code.extend_from_slice(&(Syscall::Write as u32).to_le_bytes());
    code.extend_from_slice(&[0x0F, 0x05]); // syscall: write
    code.extend_from_slice(&[0xB8]);
    code.extend_from_slice(&(Syscall::Yield as u32).to_le_bytes());
    code.extend_from_slice(&[0x0F, 0x05]); // syscall: yield
    code.extend_from_slice(&[0x48, 0xBF]);
    code.extend_from_slice(&scratch_addr.to_le_bytes());
    code.extend_from_slice(&[0xB8]);
    code.extend_from_slice(&DONE_MARK.to_le_bytes()); // mov eax, 0xBEEF
    code.extend_from_slice(&[0x48, 0x89, 0x07]); // mov [rdi], rax
    code.extend_from_slice(&[0xB8]);
    code.extend_from_slice(&(Syscall::Exit as u32).to_le_bytes());
    code.extend_from_slice(&[0x0F, 0x05]); // syscall: exit
    code
}

/* ---------------- the test ---------------- */

fn test_main_entry(boot_info: &'static mut BootInfo) -> ! {
    galexy_os::init();
    screen::init(boot_info);
    println!("[test-smpstress] running");
    serial_println!("[test-smpstress] running");

    arch::mm::init(boot_info);
    arch::init(boot_info);
    sched::init();
    assert_eq!(arch::cpu::online(), 2, "two CPUs must be live");

    /* ---- phase A: steal proof ---- */
    SPINNER_DEADLINE.store(arch::timer_ticks() + 100_000, Ordering::Relaxed);
    let spinner_owner = sched::spawn_thread("spinner", spinner);
    let flash_owner = sched::spawn_thread("flash", flash);
    assert_eq!(spinner_owner, 0, "first spawn pins to the BSP");
    assert_eq!(flash_owner, 1, "second spawn pins to the AP");

    // The AP drains (flash exits) and goes idle; its idle-pass steal must
    // migrate the spinner. Budget: 3000 ticks (~3 machine-seconds).
    let steal_deadline = arch::timer_ticks() + 3000;
    loop {
        x86_64::instructions::hlt();
        sched::reap();
        if sched::steal_count() >= 1 {
            break;
        }
        assert!(
            arch::timer_ticks() < steal_deadline,
            "the AP never stole the BSP's spinner"
        );
    }
    // The stolen thread sticks (cooldown): owner == AP right now, and its
    // CPU time accrues on the AP's rotation.
    assert_eq!(
        sched::thread_owner("spinner"),
        Some(1),
        "the spinner's owner must have flipped to the AP"
    );
    // Give it a few quanta on the AP, then check ticks accrued THERE.
    let tick_deadline = arch::timer_ticks() + 100;
    while arch::timer_ticks() < tick_deadline {
        x86_64::instructions::hlt();
    }
    let spinner_ticks = sched::thread_stats()
        .iter()
        .find(|&&(n, _)| n == "spinner")
        .map(|&(_, t)| t)
        .unwrap_or(0);
    assert!(spinner_ticks > 0, "the spinner must run on its new owner");

    // Release the spinner (it exits ON the AP; the AP reaps it).
    SPINNER_DEADLINE.store(arch::timer_ticks() + 100, Ordering::Relaxed);
    loop {
        x86_64::instructions::hlt();
        sched::reap();
        if sched::threads_count() == 0 {
            break;
        }
        assert!(
            arch::timer_ticks() < steal_deadline + 5000,
            "phase A never drained"
        );
    }
    serial_println!("[test-smpstress] phase A: steal proven (owner flip + AP ticks)");

    /* ---- phase B: concurrent heap growth + shootdown crossings ---- */
    let broadcasts_before = arch::mm::shootdown::broadcast_count();
    let _ = sched::spawn_thread("grower0", grower0);
    let _ = sched::spawn_thread("grower1", grower1);
    let grown_deadline = arch::timer_ticks() + 5000;
    while GROWN[0].load(Ordering::Relaxed) == 0 || GROWN[1].load(Ordering::Relaxed) == 0 {
        x86_64::instructions::hlt();
        sched::reap();
        assert!(
            arch::timer_ticks() < grown_deadline,
            "the growers never landed their 1 MiB (growth wedged?)"
        );
    }
    let broadcasts_after_growth = arch::mm::shootdown::broadcast_count();
    serial_println!(
        "[test-smpstress] phase B: grown, broadcasts {} -> {}",
        broadcasts_before,
        broadcasts_after_growth
    );
    assert!(
        broadcasts_after_growth > broadcasts_before,
        "heap growth must broadcast shootdowns"
    );
    DROP_NOW.store(1, Ordering::Release);
    loop {
        x86_64::instructions::hlt();
        sched::reap();
        if sched::threads_count() == 0 {
            break;
        }
        assert!(
            arch::timer_ticks() < grown_deadline + 5000,
            "phase B never drained"
        );
    }

    /* ---- phase C: hammer + ring-3 churn + EXACT frame closure ---- */
    let baseline = arch::mm::free_frames();
    let heap_before = arch::mm::heap::stats().1;

    let _ = sched::spawn_thread("hammer0", hammer0);
    let _ = sched::spawn_thread("hammer1", hammer1);

    let console_cap = galexy_abi::reserved::console(CapRights::WRITE);
    let (region, _) = sched::spawn_user_task("stressblob", |gr| {
        blob(gr.code.as_u64(), console_cap.bits(), gr.scratch.as_u64())
    });
    let scratch_phys = region.scratch_phys;

    let churn_deadline = arch::timer_ticks() + 30000;
    let mut blob_marked = false;
    loop {
        x86_64::instructions::hlt();
        sched::reap();
        if !blob_marked {
            // SAFETY: the scratch page stays mapped until the task's OWN
            // reap frees its tree; the peek runs on the kernel tree (the
            // phys map is present in every address space).
            let ptr: *const u32 = arch::mm::frame_virt(scratch_phys).as_ptr();
            if unsafe { core::ptr::read_volatile(ptr) } == DONE_MARK {
                blob_marked = true;
                serial_println!("[test-smpstress] phase C: ring-3 blob completed");
            }
        }
        if HAMMER_DONE[0].load(Ordering::Relaxed) == 1
            && HAMMER_DONE[1].load(Ordering::Relaxed) == 1
            && blob_marked
        {
            break;
        }
        assert!(
            arch::timer_ticks() < churn_deadline,
            "phase C never completed (hammers={:?}, blob={})",
            [
                HAMMER_DONE[0].load(Ordering::Relaxed),
                HAMMER_DONE[1].load(Ordering::Relaxed)
            ],
            blob_marked
        );
    }

    // Drain everything (the blob's exit handoff lags its output by a
    // quantum under slow TCG).
    loop {
        x86_64::instructions::hlt();
        sched::reap();
        if sched::threads_count() == 0 {
            break;
        }
        assert!(
            arch::timer_ticks() < churn_deadline + 5000,
            "phase C never drained"
        );
    }

    // EXACT closure: no heap growth happened in the window (the hammer
    // cycles fit the phase-B-grown heap), so every frame the churn
    // consumed — hammer stacks + fx areas, the blob's tree (data + page
    // tables) — must be back on the allocator.
    let heap_after = arch::mm::heap::stats().1;
    assert_eq!(
        heap_after, heap_before,
        "the heap grew during phase C (the closure budget was wrong)"
    );
    let final_frames = arch::mm::free_frames();
    assert_eq!(
        final_frames, baseline,
        "frame accounting must close exactly: baseline={baseline} final={final_frames}"
    );

    println!("[test-smpstress] all assertions passed");
    serial_println!("[test-smpstress] passed");
    exit_qemu(QemuExitCode::Success);
}
