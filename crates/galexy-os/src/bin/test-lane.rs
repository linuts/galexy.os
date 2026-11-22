//! Syscall 35 is `net` only. A lane moves bytes between its two ends.

#![no_std]
#![no_main]

extern crate alloc;

use bootloader_api::{entry_point, BootInfo};
use galexy_abi::{SysError, Syscall};
use galexy_os::arch::mm;
use galexy_os::{drivers::screen, exit_qemu, serial_println, QemuExitCode};

entry_point!(test_main_entry, config = &galexy_os::BOOTLOADER_CONFIG);

const MARK: u32 = 0x1A4E;

fn test_main_entry(boot_info: &'static mut BootInfo) -> ! {
    galexy_os::init();
    screen::init(boot_info);
    mm::init(boot_info);
    galexy_os::arch::init(boot_info);
    galexy_os::sched::init();
    serial_println!("[test-lane] running");

    let denied = run_blob("shell", |gr| deny_blob(gr.scratch.as_u64()));
    assert_eq!(denied, SysError::AccessDenied as u32);
    serial_println!("[test-lane] shell denied");

    let (n, body) = run_blob_body("net", |gr| net_blob(gr.scratch.as_u64(), gr.code.as_u64()));
    assert_eq!(n, 7);
    assert_eq!(&body[..7], b"lane ok");
    serial_println!("[test-lane] passed");
    exit_qemu(QemuExitCode::Success);
}

fn run_blob(
    name: &str,
    build: impl FnOnce(galexy_os::sched::UserRegion) -> alloc::vec::Vec<u8>,
) -> u32 {
    let (region, _) = galexy_os::sched::spawn_user_task_on(name, 0, build);
    let scratch = mm::frame_virt(region.scratch_phys).as_u64() as *const u32;
    loop {
        x86_64::instructions::hlt();
        // SAFETY: scratch stays mapped until the mark is visible.
        let mark = unsafe { core::ptr::read_volatile(scratch.add(1)) };
        if mark == MARK {
            break;
        }
        galexy_os::sched::reap();
    }
    let value = unsafe { core::ptr::read_volatile(scratch) };
    loop {
        x86_64::instructions::hlt();
        galexy_os::sched::reap();
        if galexy_os::sched::unreaped_threads() == 0 {
            break;
        }
    }
    value
}

fn run_blob_body(
    name: &str,
    build: impl FnOnce(galexy_os::sched::UserRegion) -> alloc::vec::Vec<u8>,
) -> (u32, [u8; 8]) {
    let (region, _) = galexy_os::sched::spawn_user_task_on(name, 0, build);
    let scratch = mm::frame_virt(region.scratch_phys).as_u64() as *const u32;
    loop {
        x86_64::instructions::hlt();
        let mark = unsafe { core::ptr::read_volatile(scratch.add(1)) };
        if mark == MARK {
            break;
        }
        galexy_os::sched::reap();
    }
    let value = unsafe { core::ptr::read_volatile(scratch) };
    let mut body = [0u8; 8];
    unsafe {
        core::ptr::copy_nonoverlapping(scratch.cast::<u8>().add(32), body.as_mut_ptr(), 8);
    }
    loop {
        x86_64::instructions::hlt();
        galexy_os::sched::reap();
        if galexy_os::sched::unreaped_threads() == 0 {
            break;
        }
    }
    (value, body)
}

/// `lane` from a task that is not `net`. Stores the error code.
fn deny_blob(scratch: u64) -> alloc::vec::Vec<u8> {
    let mut code = alloc::vec::Vec::new();
    code.extend_from_slice(&[0x48, 0xBF]);
    code.extend_from_slice(&scratch.to_le_bytes());
    code.extend_from_slice(&[0xB8]);
    code.extend_from_slice(&(Syscall::Lane as u32).to_le_bytes());
    code.extend_from_slice(&[0x0F, 0x05]);
    code.extend_from_slice(&[0x48, 0xBF]);
    code.extend_from_slice(&scratch.to_le_bytes());
    code.extend_from_slice(&[0x89, 0x07]);
    code.extend_from_slice(&[0xB8]);
    code.extend_from_slice(&MARK.to_le_bytes());
    code.extend_from_slice(&[0x89, 0x47, 0x04]);
    code.extend_from_slice(&[0xB8]);
    code.extend_from_slice(&(Syscall::Exit as u32).to_le_bytes());
    code.extend_from_slice(&[0x0F, 0x05]);
    code
}

/// `net` mints a lane, writes `lane ok` on end 0, reads it on end 1.
fn net_blob(scratch: u64, code_base: u64) -> alloc::vec::Vec<u8> {
    let msg = b"lane ok";
    let mut code = alloc::vec::Vec::new();
    code.extend_from_slice(&[0xEB, msg.len() as u8]);
    let msg_at = code_base + 2;
    code.extend_from_slice(msg);
    // caps live at scratch+16
    let caps = scratch + 16;
    // mov rdi, caps; mov eax, Lane; syscall
    code.extend_from_slice(&[0x48, 0xBF]);
    code.extend_from_slice(&caps.to_le_bytes());
    code.extend_from_slice(&[0xB8]);
    code.extend_from_slice(&(Syscall::Lane as u32).to_le_bytes());
    code.extend_from_slice(&[0x0F, 0x05]);
    // mov rdi, [caps]; mov rsi, msg; mov edx, 7; mov eax, Write; syscall
    code.extend_from_slice(&[0x48, 0x8B, 0x3D]);
    // This RIP-relative form is awkward. Use an absolute load instead.
    // Reset that prefix: pop the three bytes we just pushed.
    code.truncate(code.len() - 3);
    code.extend_from_slice(&[0x48, 0xBF]);
    code.extend_from_slice(&caps.to_le_bytes());
    code.extend_from_slice(&[0x48, 0x8B, 0x3F]); // mov rdi, [rdi]
    code.extend_from_slice(&[0x48, 0xBE]);
    code.extend_from_slice(&msg_at.to_le_bytes());
    code.extend_from_slice(&[0xBA, 7, 0, 0, 0]);
    code.extend_from_slice(&[0xB8]);
    code.extend_from_slice(&(Syscall::Write as u32).to_le_bytes());
    code.extend_from_slice(&[0x0F, 0x05]);
    // mov rdi, caps+8; mov rdi, [rdi]; mov rsi, scratch; mov edx, 16; read
    code.extend_from_slice(&[0x48, 0xBF]);
    code.extend_from_slice(&(caps + 8).to_le_bytes());
    code.extend_from_slice(&[0x48, 0x8B, 0x3F]);
    code.extend_from_slice(&[0x48, 0xBE]);
    code.extend_from_slice(&(scratch + 32).to_le_bytes());
    code.extend_from_slice(&[0xBA, 16, 0, 0, 0]);
    code.extend_from_slice(&[0xB8]);
    code.extend_from_slice(&(Syscall::Read as u32).to_le_bytes());
    code.extend_from_slice(&[0x0F, 0x05]);
    // mov rdi, scratch; mov [rdi], eax; mov eax, MARK; mov [rdi+4], eax; exit
    code.extend_from_slice(&[0x48, 0xBF]);
    code.extend_from_slice(&scratch.to_le_bytes());
    code.extend_from_slice(&[0x89, 0x07]);
    code.extend_from_slice(&[0xB8]);
    code.extend_from_slice(&MARK.to_le_bytes());
    code.extend_from_slice(&[0x89, 0x47, 0x04]);
    code.extend_from_slice(&[0xB8]);
    code.extend_from_slice(&(Syscall::Exit as u32).to_le_bytes());
    code.extend_from_slice(&[0x0F, 0x05]);
    code
}
