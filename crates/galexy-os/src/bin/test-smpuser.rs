//! Integration test kernel: userland on BOTH CPUs (M18). Two ring-3 blobs
//! (natural pin-RR: first spawns on the BSP, second on the AP) each run the
//! full syscall lifecycle (write → yield → scratch-mark → exit). Asserts
//! both completed, both trees were reclaimed, and the AP-side ring-3 path
//! (per-CPU TSS.RSP0 / kstack slot / syscall MSRs / CR3) ran a program to
//! completion.

#![no_std]
#![no_main]

extern crate alloc;

use bootloader_api::{entry_point, BootInfo};
use galexy_abi::{CapRights, Syscall};
use galexy_os::{arch, drivers::screen, exit_qemu, println, sched, serial_println, QemuExitCode};

entry_point!(test_main_entry, config = &galexy_os::BOOTLOADER_CONFIG);

const DONE_MARK: u32 = 0x00BEEF;
const WELCOME_A: &[u8] = b"cpu0: ring3 lives";
const WELCOME_B: &[u8] = b"cpu1: ring3 lives";

fn blob(msg: &[u8], code_vaddr: u64, console_cap: u64, scratch_addr: u64) -> alloc::vec::Vec<u8> {
    let mut code: alloc::vec::Vec<u8> = alloc::vec::Vec::new();
    code.extend_from_slice(&[0xEB, msg.len() as u8]); // jmp over message
    code.extend_from_slice(msg);
    code.extend_from_slice(&[0x48, 0xBF]);
    code.extend_from_slice(&(code_vaddr + 2).to_le_bytes()); // mov rsi, msg
    code.extend_from_slice(&[0xBA]);
    code.extend_from_slice(&(msg.len() as u32).to_le_bytes()); // mov rdx, len
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

#[derive(Clone, Copy, PartialEq)]
enum Phase {
    Running,
    Marked,
}

fn test_main_entry(boot_info: &'static mut BootInfo) -> ! {
    galexy_os::init();
    screen::init(boot_info);
    println!("[test-smpuser] running");
    serial_println!("[test-smpuser] running");

    arch::mm::init(boot_info);
    arch::init(boot_info);
    sched::init();
    assert_eq!(arch::cpu::online(), 2, "two CPUs must be live");

    let console_cap = galexy_abi::reserved::console(CapRights::WRITE);
    // Before either spawn: an idle AP runs its pinned task immediately, so
    // a sample taken after spawn can already include a finished reap.
    let baseline = arch::mm::free_frames();

    // Spawn A (pins BSP) then B (pins AP) — natural RR order.
    // spawn_user_task returns the pin decision — read it race-free.
    let (region_a, owner_a) = sched::spawn_user_task("cpu0", |gr| {
        blob(
            WELCOME_A,
            gr.code.as_u64(),
            console_cap.bits(),
            gr.scratch.as_u64(),
        )
    });
    let (region_b, owner_b) = sched::spawn_user_task("cpu1", |gr| {
        blob(
            WELCOME_B,
            gr.code.as_u64(),
            console_cap.bits(),
            gr.scratch.as_u64(),
        )
    });
    assert_eq!(owner_a, 0, "first user task must pin to the BSP");
    assert_eq!(owner_b, 1, "second user task must pin to the AP");
    serial_println!("[test-smpuser] pinned: a->cpu{} b->cpu{}", owner_a, owner_b);

    // The scratch addrs (user VAs) fold to the same bits per task; the
    // PHYSICAL frame addresses differ. Keep them for peeking.
    let scratch_phys = [region_a.scratch_phys, region_b.scratch_phys];
    let mut state = [Phase::Running; 2];

    // Main loop: peek scratch before reap — exit frees (and wipes) the tree.
    loop {
        sched::arm_timer_for_load();
        x86_64::instructions::hlt();
        for idx in 0..2 {
            if state[idx] == Phase::Marked {
                continue;
            }
            // SAFETY: phys map is present in every address space; we peek
            // before reap so the wiped free path cannot clear DONE_MARK.
            let ptr: *const u32 = arch::mm::frame_virt(scratch_phys[idx]).as_ptr();
            let mark = unsafe { core::ptr::read_volatile(ptr) };
            if mark == DONE_MARK {
                state[idx] = Phase::Marked;
                serial_println!("[test-smpuser] task {} marked done at cpu time", idx);
            }
        }
        if state[0] == Phase::Marked && state[1] == Phase::Marked {
            break;
        }
        sched::reap();
    }
    println!("[test-smpuser] both ring-3 tasks completed on their owners");

    // Drain: reap until both slots are gone (each owner reaps its own).
    loop {
        x86_64::instructions::hlt();
        sched::reap();
        if sched::unreaped_threads() == 0 {
            break;
        }
    }

    let frames_after = arch::mm::free_frames();
    assert_eq!(
        frames_after, baseline,
        "both user tasks' frames must return to the allocator: baseline={baseline} final={frames_after}"
    );

    println!("[test-smpuser] all assertions passed");
    serial_println!("[test-smpuser] passed");
    exit_qemu(QemuExitCode::Success);
}
