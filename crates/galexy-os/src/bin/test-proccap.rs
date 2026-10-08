//! Integration test: spawn returns a process Cap; Cap-wait and Cap-kill.
//!
//! A launcher-grade blob `spawn`s `hello`, Cap-waits for exit 0, proves a
//! forged Cap fails, then `spawn`s `linger`, kills it, and Cap-waits for
//! the kill status.

#![no_std]
#![no_main]

extern crate alloc;

use bootloader_api::{entry_point, BootInfo};
use galexy_abi::{reserved, Cap, CapRights, SysError, Syscall, PROC_CAP_BASE};
use galexy_os::{
    arch::mm, drivers::screen, exit_qemu, println, sched, serial_println, QemuExitCode,
};

entry_point!(test_main_entry, config = &galexy_os::BOOTLOADER_CONFIG);

const DONE: u64 = 0x0C_F10C_CA91;
const TICK_TIMEOUT: u64 = 12_000;

#[repr(C)]
struct Report {
    done: u64,
    spawn_ok: u64,
    cap_bits: u64,
    wait_ok: u64,
    wait_code: u64,
    forge_ok: u64,
    forge_err: u64,
    kill_spawn_ok: u64,
    kill_ok: u64,
    kill_wait_ok: u64,
    kill_wait_code: u64,
}

fn test_main_entry(boot_info: &'static mut BootInfo) -> ! {
    galexy_os::init();
    screen::init(boot_info);
    println!("[test-proccap] running");
    serial_println!("[test-proccap] running");

    let Some(ramdisk_addr) = boot_info.ramdisk_addr.into_option() else {
        panic!("no ramdisk");
    };
    mm::init(boot_info);
    galexy_os::arch::init(boot_info);
    sched::init();
    let archive = unsafe {
        core::slice::from_raw_parts(
            x86_64::VirtAddr::new(ramdisk_addr).as_ptr::<u8>(),
            boot_info.ramdisk_len as usize,
        )
    };
    sched::ramdisk::init(archive);

    assert!(
        sched::ramdisk::find("hello").is_some(),
        "hello missing from ramdisk"
    );
    assert!(
        sched::ramdisk::find("linger").is_some(),
        "linger missing from ramdisk"
    );

    let (region, _) = sched::spawn_user_launcher("proccap", |gr| {
        unsafe {
            core::ptr::write_bytes(mm::frame_virt(gr.scratch_phys).as_mut_ptr::<u8>(), 0, 4096);
        }
        build_blob(gr.code.as_u64(), gr.scratch.as_u64())
    });

    let scratch: *const Report = mm::frame_virt(region.scratch_phys).as_ptr();
    let mut elapsed = 0u64;
    // Copy the report before reap — exit frees (and wipes) the scratch tree.
    let report = loop {
        sched::arm_timer_for_load();
        x86_64::instructions::hlt();
        sched::drain_spawn();
        let done = unsafe { core::ptr::read_volatile(core::ptr::addr_of!((*scratch).done)) };
        if done == DONE {
            break unsafe { core::ptr::read_volatile(scratch) };
        }
        sched::reap();
        elapsed += 1;
        if elapsed > TICK_TIMEOUT {
            panic!("proccap blob never finished");
        }
    };

    assert_eq!(report.spawn_ok, 1, "spawn hello must succeed");
    let cap = Cap::from_bits(report.cap_bits);
    assert_eq!(cap.index(), PROC_CAP_BASE, "first process Cap is PROC_CAP_BASE");
    assert!(cap.rights().contains(CapRights::PROC_WAIT));
    assert!(cap.rights().contains(CapRights::PROC_KILL));

    assert_eq!(report.wait_ok, 1, "Cap-wait must succeed");
    assert_eq!(report.wait_code, 0, "hello exits 0");

    assert_eq!(report.forge_ok, 0, "forged Cap wait must fail");
    assert_eq!(
        report.forge_err,
        SysError::BadCap as u64,
        "forged Cap is BadCap"
    );

    assert_eq!(report.kill_spawn_ok, 1, "spawn linger must succeed");
    assert_eq!(report.kill_ok, 1, "kill must succeed");
    assert_eq!(report.kill_wait_ok, 1, "wait after kill must succeed");
    assert_eq!(report.kill_wait_code, 137, "killed task exits 137");

    loop {
        x86_64::instructions::hlt();
        sched::drain_spawn();
        sched::reap();
        if sched::unreaped_threads() == 0 {
            break;
        }
    }

    println!("[test-proccap] Cap-wait and Cap-kill work");
    serial_println!("[test-proccap] passed");
    exit_qemu(QemuExitCode::Success);
}

fn build_blob(code_base: u64, scratch: u64) -> alloc::vec::Vec<u8> {
    let hello = b"hello";
    let linger = b"linger";
    let mut code: alloc::vec::Vec<u8> = alloc::vec::Vec::new();
    code.push(0xEB);
    code.push((hello.len() + linger.len()) as u8);
    let hello_addr = code_base + 2;
    code.extend_from_slice(hello);
    let linger_addr = hello_addr + hello.len() as u64;
    code.extend_from_slice(linger);

    mov_r64_imm(&mut code, 15, scratch); // r15 = scratch
    let loader = reserved::loader(CapRights::EXEC).bits();

    // spawn("hello") — no wait; Cap in rax
    spawn_call(&mut code, loader, hello_addr, hello.len() as u64, 0);
    store(&mut code, 2, 0x08); // spawn_ok
    store(&mut code, 0, 0x10); // cap_bits
    code.extend_from_slice(&[0x49, 0x89, 0xC4]); // mov r12, rax

    // wait(cap)
    mov_eax(&mut code, Syscall::Wait as u32);
    code.extend_from_slice(&[0x4C, 0x89, 0xE7]); // mov rdi, r12
    code.extend_from_slice(&[0x0F, 0x05]);
    store(&mut code, 2, 0x18); // wait_ok
    store(&mut code, 0, 0x20); // wait_code

    // Forged Cap: PROC_CAP_BASE with PROC_WAIT rights, empty table slot.
    let forged = Cap::new(PROC_CAP_BASE, CapRights::PROC_WAIT).bits();
    mov_eax(&mut code, Syscall::Wait as u32);
    mov_r64_imm(&mut code, 7, forged);
    code.extend_from_slice(&[0x0F, 0x05]);
    store(&mut code, 2, 0x28); // forge_ok
    store(&mut code, 0, 0x30); // forge_err

    // spawn("linger")
    spawn_call(&mut code, loader, linger_addr, linger.len() as u64, 0);
    store(&mut code, 2, 0x38); // kill_spawn_ok
    code.extend_from_slice(&[0x49, 0x89, 0xC4]); // mov r12, rax

    // kill(cap)
    mov_eax(&mut code, Syscall::Kill as u32);
    code.extend_from_slice(&[0x4C, 0x89, 0xE7]);
    code.extend_from_slice(&[0x0F, 0x05]);
    store(&mut code, 2, 0x40); // kill_ok

    // wait(cap) → 137
    mov_eax(&mut code, Syscall::Wait as u32);
    code.extend_from_slice(&[0x4C, 0x89, 0xE7]);
    code.extend_from_slice(&[0x0F, 0x05]);
    store(&mut code, 2, 0x48); // kill_wait_ok
    store(&mut code, 0, 0x50); // kill_wait_code

    mov_r64_imm(&mut code, 0, DONE);
    store(&mut code, 0, 0x00);
    mov_eax(&mut code, Syscall::Exit as u32);
    code.extend_from_slice(&[0x31, 0xFF, 0x0F, 0x05]);
    code
}

fn spawn_call(code: &mut alloc::vec::Vec<u8>, loader: u64, name: u64, len: u64, grants: u64) {
    mov_eax(code, Syscall::Spawn as u32);
    mov_r64_imm(code, 7, loader); // rdi
    mov_r64_imm(code, 6, name); // rsi
    mov_r64_imm(code, 2, len); // rdx
    mov_r64_imm(code, 8, 0); // r8 arg
    mov_r64_imm(code, 9, 0); // r9 arg len
    mov_r64_imm(code, 10, grants); // r10
    code.extend_from_slice(&[0x0F, 0x05]);
}

fn mov_r64_imm(code: &mut alloc::vec::Vec<u8>, reg: u8, imm: u64) {
    let rex = 0x48 | u8::from(reg >= 8);
    code.push(rex);
    code.push(0xB8 + (reg & 7));
    code.extend_from_slice(&imm.to_le_bytes());
}

fn mov_eax(code: &mut alloc::vec::Vec<u8>, imm: u32) {
    code.push(0xB8);
    code.extend_from_slice(&imm.to_le_bytes());
}

fn store(code: &mut alloc::vec::Vec<u8>, reg: u8, disp: i32) {
    let modrm = 0x80 | (reg << 3) | 7;
    code.extend_from_slice(&[0x49, 0x89, modrm]);
    code.extend_from_slice(&disp.to_le_bytes());
}
