//! Integration test kernel: the FIRST ring-3 code. A hand-assembled flat
//! blob runs in user mode, spinning forever; the timer preempts it (the CPU
//! pushes the IRQ frame onto the task's own kernel stack via TSS.RSP0), the
//! kernel keeps round-robining, tick attribution accumulates for both the
//! user task and the main loop. Any GPF/page fault wedges out loudly (the
//! parked fault handler kills the test's liveness).

#![no_std]
#![no_main]

use bootloader_api::{entry_point, BootInfo};
use galexy_os::{drivers::screen, exit_qemu, println, sched, serial_println, QemuExitCode};

entry_point!(test_main_entry, config = &galexy_os::BOOTLOADER_CONFIG);

/// Hand-assembled user blob (NASM-ish equivalent):
/// ```text
///     mov rax, 0x1000        ; scratch value, no memory access
/// loop:
///     add rax, 1             ; 48 83 C0 01
///     jmp rel8 loop          ; EB rel8
/// ```
/// Zero memory accesses — ring 3 cannot touch anything the kernel owns,
/// and we have no syscall path yet. Encoded top-down:
/// - offset 0: 48 C7 C0 00 10 00 00  (mov rax, 0x1000, imm32) — 7 bytes
/// - offset 7: 48 83 C0 01           (add rax, 1) — 4 bytes
/// - offset 11: EB FA                (jmp: next IP 13, rel8 −6 → offset 7)
const USER_BLOB: [u8; 13] = [
    0x48, 0xC7, 0xC0, 0x00, 0x10, 0x00, 0x00, // mov rax, 0x1000
    0x48, 0x83, 0xC0, 0x01, // add rax, 1
    0xEB, 0xFA, // jmp loop
];

/// How many timer ticks (~1 kHz, i.e. milliseconds of rotation) the blob
/// must accumulate before we call preemption proven.
const TICK_TARGET: u64 = 50;

fn test_main_entry(boot_info: &'static mut BootInfo) -> ! {
    galexy_os::init();
    screen::init(boot_info);
    println!("[test-userpreempt] running");
    serial_println!("[test-userpreempt] running");

    galexy_os::arch::mm::init(boot_info);
    // GDT with user segments loads here — must be up before the user task
    // exists (its selectors already carry DPL 3 regardless, but the CPU's
    // iretq target must exist in the descriptor table).
    galexy_os::arch::init();
    sched::init();

    sched::spawn_user_task("user-blob", &USER_BLOB);
    let stats = sched::thread_stats();
    assert!(
        stats.iter().any(|&(name, _)| name == "user-blob"),
        "user task registered in the rotation"
    );

    // Main loop: hlt + let the timer rotate us into the blob and back
    // repeatedly. The blob never yields; only preemption moves us along.
    let main_start = sched::main_ticks();
    loop {
        x86_64::instructions::hlt();
        let stats = sched::thread_stats();
        let user_ticks = stats
            .iter()
            .find(|&&(n, _)| n == "user-blob")
            .map(|&(_, t)| t)
            .unwrap_or(0);
        if user_ticks >= TICK_TARGET {
            break;
        }
    }

    // The main loop must ALSO have kept receiving quanta (round-robin
    // fairness across the ring boundary).
    let main_ticks = sched::main_ticks() - main_start;
    assert!(
        main_ticks > 1,
        "main loop consumed no quanta: {main_ticks}"
    );

    println!("[test-userpreempt] ring 3 preempted round-robin works");
    println!("[test-userpreempt] all assertions passed");
    serial_println!("[test-userpreempt] passed");
    exit_qemu(QemuExitCode::Success);
}
