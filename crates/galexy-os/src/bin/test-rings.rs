//! Integration test kernel: privilege-ring groundwork (roadmap Step A
//! prerequisites). Proves GDT user segments + TSS.RSP0 plumbing + frame
//! shape (CPL) introspection, all kernel-side:
//! 1. `user_cs_ss()` selectors carry DPL 3 + consecutive GDT layout
//!    (SYSRET requires `user SS == user CS + 8`).
//! 2. `set_tss_rsp0` roundtrips through the live TSS.
//! 3. `Context::cpl()` decodes fabricated frames (kernel + user shape).
//! 4. Per-CPU substrate (SMP M18): GS-base identity + kstack registry
//!    roundtrips on CPU 0 — the syscall naked entry switches to `gs:[8]`.

#![no_std]
#![no_main]

use bootloader_api::{entry_point, BootInfo};
use galexy_os::arch;
use galexy_os::sched::context;
use galexy_os::{drivers::screen, exit_qemu, println, serial_println, QemuExitCode};
use x86_64::VirtAddr;

entry_point!(test_main_entry, config = &galexy_os::BOOTLOADER_CONFIG);

fn test_main_entry(boot_info: &'static mut BootInfo) -> ! {
    galexy_os::init();
    screen::init(boot_info);
    println!("[test-rings] running");
    serial_println!("[test-rings] running");

    arch::mm::init(boot_info);
    arch::init(boot_info); // GDT/IDT bring-up lives here

    // 1: user selectors: DPL3 (selectors carry their RPL from user_cs_ss),
    // consecutive in the GDT (CSS+8 == SS) for the SYSRET quirk.
    let (user_cs, user_ss) = arch::user_cs_ss();
    assert_ne!(user_cs, 0, "user CS must be live after GDT load");
    assert_eq!(user_ss, user_cs + 8, "SYSRET quirk: user SS = user CS + 8");
    assert_eq!(user_cs & 0b11, 3, "user CS carries RPL 3");
    assert_eq!(user_ss & 0b11, 3, "user SS carries RPL 3");

    // 2: TSS RSP0 write/read roundtrip through the live TSS.
    let scratch: [u8; 4096] = [0; 4096];
    let scratch_top = VirtAddr::from_ptr(&scratch) + 4096;
    arch::set_tss_rsp0(scratch_top);
    assert_eq!(arch::tss_rsp0(), scratch_top, "TSS RSP0 roundtrip");
    // Restore 0: nothing should live in RSP0 while everything stays ring 0.
    arch::set_tss_rsp0(VirtAddr::zero());
    assert_eq!(arch::tss_rsp0(), VirtAddr::zero(), "RSP0 cleared");

    // 3: frame shape: cpl() decodes both rings identically.
    let mut kframe: context::Context = unsafe { core::mem::zeroed() };
    kframe.cs = 0x08; // arbitrary ring-0-selectored CS
    assert_eq!(kframe.cpl(), 0, "kernel frame decodes CPL 0");
    let mut uframe: context::Context = unsafe { core::mem::zeroed() };
    uframe.cs = user_cs; // includes RPL 3 bits
    assert_eq!(uframe.cpl(), 3, "user frame decodes CPL 3");

    // 4: per-CPU substrate (gs:[0] identity + gs:[8] kstack roundtrip).
    let cpu0 = arch::cpu::current();
    let cpu0 = cpu0 as *const _ as u64;
    serial_println!("[test-rings] per-cpu slot @ {:#x}", cpu0);
    assert_eq!(
        cpu0,
        arch::cpu::current()
            .self_ptr()
            .load(core::sync::atomic::Ordering::Relaxed),
        "gs base == gs:[0] (self-referential per-cpu struct)"
    );
    arch::syscall::set_task_kstack(0xDEAD_BEEF_CAFE_0000);
    assert_eq!(
        arch::cpu::current()
            .kstack_top()
            .load(core::sync::atomic::Ordering::Relaxed),
        0xDEAD_BEEF_CAFE_0000,
        "set_task_kstack must route through the per-CPU slot"
    );
    arch::syscall::set_task_kstack(0); // cleared: main-loop semantics

    println!("[test-rings] selectors + TSS.RSP0 + frame shape checked");
    println!("[test-rings] all assertions passed");
    serial_println!("[test-rings] passed");
    exit_qemu(QemuExitCode::Success);
}
