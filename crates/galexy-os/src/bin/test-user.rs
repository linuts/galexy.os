//! Integration test kernel: the first user PROGRAM. A ring-3 blob prints
//! via `write(console_cap, msg, len)`, yields (handing the CPU through the
//! full rotation), prints again, signals its scratch page, and exits via
//! `exit()` — after which the kernel reaps it (user stack, scratch page,
//! code page + kernel stack all returned). Every step asserts kernel-side.

#![no_std]
#![no_main]

extern crate alloc;

use bootloader_api::{entry_point, BootInfo};
use galexy_abi::{CapRights, Syscall};
use galexy_os::{drivers::screen, exit_qemu, println, sched, serial_println, QemuExitCode};

entry_point!(test_main_entry, config = &galexy_os::BOOTLOADER_CONFIG);
/// Blob message written to the console through the write syscall. Cast to
/// bytes via the closure.
const MSG: &[u8] = b"Hello from ring 3!";

/// Scratch-page completion signal the blob writes right before exiting.
const DONE_MARK: u32 = 0x00BEEF;

fn test_main_entry(boot_info: &'static mut BootInfo) -> ! {
    galexy_os::init();
    screen::init(boot_info);
    println!("[test-user] running");
    serial_println!("[test-user] running");

    galexy_os::arch::mm::init(boot_info);
    galexy_os::arch::init();
    sched::init();

    // Hand-assembled blob (offsets from the region base):
    //    0: EB 12          jmp over the 18-byte message  → target 20
    //    2: message
    //   20: mov rsi, msg_addr        (48 BE imm64)
    //   30: mov rdx, 18              (BA imm32)
    //   35: mov rdi, <console cap>   (48 BF imm64)
    //   45: mov eax, 2               (write)
    //   50: syscall
    //   52: mov eax, 1               (yield)
    //   57: syscall
    //   59: mov rdi, <scratch addr>  (48 BF imm64)
    //   69: mov eax, 0xBEEF
    //   74: mov [rdi], rax           (48 89 07)
    //   77: mov eax, 0               (exit — never returns)
    //   82: syscall
    let console_cap = galexy_abi::reserved::console(CapRights::WRITE);
    let region = sched::spawn_user_task("uhello", |gr| {
        let mut code: alloc::vec::Vec<u8> = alloc::vec::Vec::new();
        code.extend_from_slice(&[0xEB, MSG.len() as u8]); // jmp over message
        code.extend_from_slice(MSG);
        code.extend_from_slice(&[0x48, 0xBF]);
        code.extend_from_slice(&(gr.code.as_u64() + 2).to_le_bytes()); // mov rsi, msg
        code.extend_from_slice(&[0xBA]);
        code.extend_from_slice(&(MSG.len() as u32).to_le_bytes()); // mov rdx, len
        code.extend_from_slice(&[0x48, 0xBF]);
        code.extend_from_slice(&console_cap.bits().to_le_bytes()); // mov rdi, cap
        code.extend_from_slice(&[0xB8]);
        code.extend_from_slice(&(Syscall::Write as u32).to_le_bytes()); // mov eax, 2
        code.extend_from_slice(&[0x0F, 0x05]); // syscall
        code.extend_from_slice(&[0xB8]);
        code.extend_from_slice(&(Syscall::Yield as u32).to_le_bytes()); // mov eax, 1
        code.extend_from_slice(&[0x0F, 0x05]); // syscall
        code.extend_from_slice(&[0x48, 0xBF]);
        code.extend_from_slice(&gr.scratch.as_u64().to_le_bytes()); // mov rdi, scratch
        code.extend_from_slice(&[0xB8]);
        code.extend_from_slice(&DONE_MARK.to_le_bytes()); // mov eax, 0xBEEF
        code.extend_from_slice(&[0x48, 0x89, 0x07]); // mov [rdi], rax
        code.extend_from_slice(&[0xB8]);
        code.extend_from_slice(&(Syscall::Exit as u32).to_le_bytes()); // mov eax, 0
        code.extend_from_slice(&[0x0F, 0x05]); // syscall
        code
    });

    // Frame accounting: the 6 DATA frames (code 1 + stack 4 + scratch 1)
    // plus the tree's ROOT frame come back. The spawn's page-table frames
    // deeper in the tree (P3/P2/P1) stay allocated until free_user_tree
    // (next commit) — snapshot AFTER the spawn so tables are excluded.
    let frames_after_spawn = galexy_os::arch::mm::free_frames();
    const RETURNED_FRAMES: usize = 1 + 4 + 1 + 1;

    // Main loop: hlt + rotations while the user task runs. Poll the scratch
    // page through its PHYSICAL frame (the phys map is present in every
    // address space; the task's own table is active while it runs). Peek,
    // then reap — the reaper frees the scratch frame, so peek first.
    let scratch_virt: *const u32 =
        galexy_os::arch::mm::frame_virt(region.scratch_phys).as_ptr();
    loop {
        x86_64::instructions::hlt();
        // SAFETY: scratch is mapped until the reaper frees it; our peek
        // happens before any reap that matters (break skips the reap).
        let mark = unsafe { core::ptr::read_volatile(scratch_virt) };
        if mark == DONE_MARK {
            break;
        }
        sched::reap();
    }

    // The task must have exited through the syscall (never a spin/trap);
    // sweep the reaper until it's gone from the rotation.
    loop {
        x86_64::instructions::hlt();
        sched::reap();
        if sched::threads_count() == 0 {
            break;
        }
    }

    let frames_after = galexy_os::arch::mm::free_frames();
    assert_eq!(
        frames_after,
        frames_after_spawn + RETURNED_FRAMES,
        "user task's data frames must return to the allocator: after_spawn={} final={}",
        frames_after_spawn,
        frames_after
    );

    println!("[test-user] user program ran the full lifecycle");
    println!("[test-user] all assertions passed");
    serial_println!("[test-user] passed");
    exit_qemu(QemuExitCode::Success);
}
