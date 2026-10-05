//! Integration test kernel: ADDRESS-SPACE LIFECYCLE. Spawns a user task,
//! lets it run (write → yield → write → scratch → exit), reaps it, and
//! asserts the frame allocator returns to baseline EXACTLY — repeated N
//! times. The tree walk must recover page-table frames AND data frames:
//! no leaks, no cross-task leftovers.

#![no_std]
#![no_main]

extern crate alloc;

use bootloader_api::{entry_point, BootInfo};
use galexy_abi::{CapRights, Syscall};
use galexy_os::{drivers::screen, exit_qemu, println, sched, serial_println, QemuExitCode};

entry_point!(test_main_entry, config = &galexy_os::BOOTLOADER_CONFIG);

const MSG: &[u8] = b"churn";

/// Blob: jmp over message → write(msg) → yield → write(msg) →
/// scratch mark → exit. (Same shape as test-user's program.)
fn build_blob(console_cap_bits: u64, gr: sched::UserRegion) -> alloc::vec::Vec<u8> {
    let mut code: alloc::vec::Vec<u8> = alloc::vec::Vec::new();
    code.extend_from_slice(&[0xEB, MSG.len() as u8]);
    code.extend_from_slice(MSG);
    code.extend_from_slice(&[0x48, 0xBF]);
    code.extend_from_slice(&(gr.code.as_u64() + 2).to_le_bytes()); // mov rsi, msg
    code.extend_from_slice(&[0xBA]);
    code.extend_from_slice(&(MSG.len() as u32).to_le_bytes()); // mov rdx, len
    code.extend_from_slice(&[0x48, 0xBF]);
    code.extend_from_slice(&console_cap_bits.to_le_bytes()); // mov rdi, cap
    code.extend_from_slice(&[0xB8]);
    code.extend_from_slice(&(Syscall::Write as u32).to_le_bytes());
    code.extend_from_slice(&[0x0F, 0x05]);
    code.extend_from_slice(&[0xB8]);
    code.extend_from_slice(&(Syscall::Yield as u32).to_le_bytes());
    code.extend_from_slice(&[0x0F, 0x05]);
    code.extend_from_slice(&[0x48, 0xBF]);
    code.extend_from_slice(&gr.scratch.as_u64().to_le_bytes()); // mov rdi, scratch
    code.extend_from_slice(&[0xB8]);
    code.extend_from_slice(&0xBEEF_u32.to_le_bytes());
    code.extend_from_slice(&[0x48, 0x89, 0x07]); // mov [rdi], rax
    code.extend_from_slice(&[0xB8]);
    code.extend_from_slice(&(Syscall::Exit as u32).to_le_bytes());
    code.extend_from_slice(&[0x0F, 0x05]);
    code
}

/// How many spawn/exit/reap cycles.
const CYCLES: usize = 5;

fn test_main_entry(boot_info: &'static mut BootInfo) -> ! {
    galexy_os::init();
    screen::init(boot_info);
    println!("[test-treechurn] running");
    serial_println!("[test-treechurn] running");

    galexy_os::arch::mm::init(boot_info);
    galexy_os::arch::init(boot_info);
    sched::init();

    let console_cap_bits = galexy_abi::reserved::console(CapRights::WRITE).bits();
    let baseline = galexy_os::arch::mm::free_frames();

    for cycle in 0..CYCLES {
        let (region, _) = sched::spawn_user_task("churn", |gr| build_blob(console_cap_bits, gr));
        let scratch_virt: *const u32 =
            galexy_os::arch::mm::frame_virt(region.scratch_phys).as_ptr();

        // Peek then reap (the reaper frees the scratch frame).
        loop {
            x86_64::instructions::hlt();
            // SAFETY: mapped until the reaper frees it; peek precedes reap.
            let mark = unsafe { core::ptr::read_volatile(scratch_virt) };
            if mark == 0xBEEF {
                break;
            }
            sched::reap();
        }
        // Sweep until the rotation is empty.
        loop {
            x86_64::instructions::hlt();
            sched::reap();
            if sched::threads_count() == 0 {
                break;
            }
        }

        let now = galexy_os::arch::mm::free_frames();
        assert_eq!(
            now, baseline,
            "cycle {cycle}: frames must return to baseline exactly (got {now}, want {baseline})"
        );
        println!("[test-treechurn] cycle {}: accounting closed", cycle);
    }

    println!("[test-treechurn] no leaks across {} cycles", CYCLES);
    serial_println!("[test-treechurn] passed");
    exit_qemu(QemuExitCode::Success);
}
