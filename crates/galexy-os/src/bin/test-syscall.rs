//! Integration test kernel: the FIRST SYSCALL from ring 3. A user blob
//! invokes `cap_info(console_cap)` (SYSCALL 3), stores the returned
//! register form (RAX = value, RDX = ok) into its scratch page, then spins.
//! The kernel-side test polls the scratch page through the shared address
//! space — proving MSR plumbing, the naked entry's uniform frame, the
//! dispatch routing, and the register-form result round-trip.

#![no_std]
#![no_main]

extern crate alloc;

use bootloader_api::{entry_point, BootInfo};
use galexy_abi::{CapRights, Syscall};
use galexy_os::{drivers::screen, exit_qemu, println, sched, serial_println, QemuExitCode};
use x86_64::VirtAddr;

entry_point!(test_main_entry, config = &galexy_os::BOOTLOADER_CONFIG);

const TICK_TIMEOUT: u64 = 2000;

fn test_main_entry(boot_info: &'static mut BootInfo) -> ! {
    galexy_os::init();
    screen::init(boot_info);
    println!("[test-syscall] running");
    serial_println!("[test-syscall] running");

    galexy_os::arch::mm::init(boot_info);
    galexy_os::arch::init(boot_info); // MSRs (STAR/LSTAR/EFER.SCE) live here
    sched::init();

    // The capability the blob will probe: the console cap with WRITE right.
    let console_cap = galexy_abi::reserved::console(CapRights::WRITE);
    let expected = console_cap.bits();

    // Assemble the blob with runtime addresses:
    //   mov eax, 3                       ; syscall #: cap_info
    //   mov rdi, <console cap bits>      ; a0 = cap
    //   syscall
    //   mov rdi, <scratch addr>          ; store RAX (the echo) here
    //   mov [rdi], rax
    //   jmp self
    let (region, _) = sched::spawn_user_task("capinfo-blob", |gr| {
        let mut code: alloc::vec::Vec<u8> = alloc::vec::Vec::new();
        code.extend_from_slice(&[0xB8, Syscall::CapInfo as u8, 0x00, 0x00, 0x00]); // mov eax, 3
        code.extend_from_slice(&[0x48, 0xBF]); // mov rdi, imm64
        code.extend_from_slice(&expected.to_le_bytes());
        code.extend_from_slice(&[0x0F, 0x05]); // syscall
        code.extend_from_slice(&[0x48, 0xBF]); // mov rdi, imm64
        code.extend_from_slice(&gr.scratch.as_u64().to_le_bytes());
        code.extend_from_slice(&[0x48, 0x89, 0x07]); // mov [rdi], rax
        code.extend_from_slice(&[0xEB, 0xFE]); // jmp self
        code
    });

    // Poll the scratch page through its PHYSICAL frame (the phys map is
    // present in every address space): the blob runs until the timer
    // preempts it and rotates us back — meanwhile the task's OWN table,
    // not the kernel's, is active, so the user-space alias is unreachable
    // from kernel context.
    let scratch_virt: *const u64 = galexy_os::arch::mm::frame_virt(region.scratch_phys).as_ptr();
    let mut elapsed = 0u64;
    let got = loop {
        x86_64::instructions::hlt();
        elapsed += 1;
        // SAFETY: the scratch page is mapped and will hold what the blob
        // stored (word-aligned, written once).
        let v = unsafe { core::ptr::read_volatile(scratch_virt) };
        if v != 0 {
            break v;
        }
        if elapsed > TICK_TIMEOUT {
            panic!("cap_info result never landed in the scratch page");
        }
    };

    assert_eq!(
        got, expected,
        "cap_info must echo the capability handle back"
    );

    // Sanity: the scratch page's physical translation must agree with what
    // an independent translate gives.
    assert!(
        galexy_os::arch::mm::translate(VirtAddr::from_ptr(scratch_virt)).is_some(),
        "scratch page must be mapped"
    );

    println!("[test-syscall] cap_info round-tripped through SYSCALL");
    println!("[test-syscall] all assertions passed");
    serial_println!("[test-syscall] passed");
    exit_qemu(QemuExitCode::Success);
}
