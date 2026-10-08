//! Integration test: Cap forge battery for reserved + file Caps.
//!
//! A console-only blob tries keyboard / loader / stats (no grants) and a
//! WRITE-forged file Cap — all must be AccessDenied or BadCap as appropriate.

#![no_std]
#![no_main]

extern crate alloc;

use bootloader_api::{entry_point, BootInfo};
use galexy_abi::{reserved, Cap, CapRights, SysError, Syscall, FILE_CAP_BASE};
use galexy_os::{
    arch::mm, drivers::screen, exit_qemu, println, sched, serial_println, QemuExitCode,
};

entry_point!(test_main_entry, config = &galexy_os::BOOTLOADER_CONFIG);

const DONE: u64 = 0x000F_066E_0001;
const TICK_TIMEOUT: u64 = 4000;

#[repr(C)]
struct Report {
    done: u64,
    kbd_ok: u64,
    kbd_err: u64,
    loader_ok: u64,
    loader_err: u64,
    stats_ok: u64,
    stats_err: u64,
    open_ok: u64,
    forge_ok: u64,
    forge_err: u64,
}

fn test_main_entry(boot_info: &'static mut BootInfo) -> ! {
    galexy_os::init();
    screen::init(boot_info);
    println!("[test-capforge] running");
    serial_println!("[test-capforge] running");

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

    let (region, _) = sched::spawn_user_task("capforge", |gr| {
        unsafe {
            core::ptr::write_bytes(mm::frame_virt(gr.scratch_phys).as_mut_ptr::<u8>(), 0, 4096);
        }
        build_blob(gr.code.as_u64(), gr.scratch.as_u64())
    });

    let scratch: *const Report = mm::frame_virt(region.scratch_phys).as_ptr();
    let mut elapsed = 0u64;
    // Spin-poll DONE before any reap: exit frees (and wipes) scratch, and an
    // AP that stole the task may reap it between our hlts. IRQs must stay on
    // so a BSP-owned blob still gets timer quantums.
    x86_64::instructions::interrupts::enable();
    let report = loop {
        let done = unsafe { core::ptr::read_volatile(core::ptr::addr_of!((*scratch).done)) };
        if done == DONE {
            break unsafe { core::ptr::read_volatile(scratch) };
        }
        core::hint::spin_loop();
        elapsed += 1;
        if elapsed > TICK_TIMEOUT.saturating_mul(10_000) {
            panic!("capforge blob never finished");
        }
    };

    assert_eq!(report.kbd_ok, 0, "keyboard without grant must fail");
    assert_eq!(report.kbd_err, SysError::AccessDenied as u64);
    assert_eq!(report.loader_ok, 0, "loader without grant must fail");
    assert_eq!(report.loader_err, SysError::AccessDenied as u64);
    assert_eq!(report.stats_ok, 0, "stats without grant must fail");
    assert_eq!(report.stats_err, SysError::AccessDenied as u64);
    assert_eq!(report.open_ok, 1, "banner.txt must open");
    assert_eq!(report.forge_ok, 0, "WRITE-forged file Cap must fail");
    assert_eq!(report.forge_err, SysError::AccessDenied as u64);

    loop {
        x86_64::instructions::hlt();
        sched::reap();
        if sched::unreaped_threads() == 0 {
            break;
        }
    }

    println!("[test-capforge] reserved+file forge battery ok");
    serial_println!("[test-capforge] passed");
    exit_qemu(QemuExitCode::Success);
}

fn build_blob(code_base: u64, scratch: u64) -> alloc::vec::Vec<u8> {
    let banner = b"banner.txt";
    let hello = b"hello";
    let mut code = alloc::vec::Vec::new();
    code.push(0xEB);
    code.push((banner.len() + hello.len()) as u8);
    let banner_addr = code_base + 2;
    code.extend_from_slice(banner);
    let hello_addr = banner_addr + banner.len() as u64;
    code.extend_from_slice(hello);

    mov_r64_imm(&mut code, 15, scratch);

    // read(keyboard) — no Keyboard grant
    let kbd = reserved::keyboard(CapRights::READ).bits();
    mov_eax(&mut code, Syscall::Read as u32);
    mov_r64_imm(&mut code, 7, kbd);
    mov_r64_imm(&mut code, 6, scratch + 0x80);
    mov_r64_imm(&mut code, 2, 8);
    code.extend_from_slice(&[0x0F, 0x05]);
    store(&mut code, 2, 0x08);
    store(&mut code, 0, 0x10);

    // spawn via loader — no Loader grant
    let loader = reserved::loader(CapRights::EXEC).bits();
    mov_eax(&mut code, Syscall::Spawn as u32);
    mov_r64_imm(&mut code, 7, loader);
    mov_r64_imm(&mut code, 6, hello_addr);
    mov_r64_imm(&mut code, 2, hello.len() as u64);
    mov_r64_imm(&mut code, 8, 0);
    mov_r64_imm(&mut code, 9, 0);
    mov_r64_imm(&mut code, 10, 0);
    code.extend_from_slice(&[0x0F, 0x05]);
    store(&mut code, 2, 0x18);
    store(&mut code, 0, 0x20);

    // read(stats) — no Query grant
    let stats = reserved::stats(CapRights::READ).bits();
    mov_eax(&mut code, Syscall::Read as u32);
    mov_r64_imm(&mut code, 7, stats);
    mov_r64_imm(&mut code, 6, scratch + 0x80);
    mov_r64_imm(&mut code, 2, 32);
    code.extend_from_slice(&[0x0F, 0x05]);
    store(&mut code, 2, 0x28);
    store(&mut code, 0, 0x30);

    // open banner + WRITE-only forgery of the index
    mov_eax(&mut code, Syscall::Open as u32);
    mov_r64_imm(&mut code, 7, banner_addr);
    mov_r64_imm(&mut code, 6, banner.len() as u64);
    code.extend_from_slice(&[0x0F, 0x05]);
    store(&mut code, 2, 0x38);
    code.extend_from_slice(&[0x49, 0x89, 0xC4]); // r12 = cap

    let forged = Cap::new(FILE_CAP_BASE, CapRights::WRITE).bits();
    mov_eax(&mut code, Syscall::Read as u32);
    mov_r64_imm(&mut code, 7, forged);
    mov_r64_imm(&mut code, 6, scratch + 0x80);
    mov_r64_imm(&mut code, 2, 4);
    code.extend_from_slice(&[0x0F, 0x05]);
    store(&mut code, 2, 0x40);
    store(&mut code, 0, 0x48);

    mov_r64_imm(&mut code, 0, DONE);
    store(&mut code, 0, 0x00);
    mov_eax(&mut code, Syscall::Exit as u32);
    code.extend_from_slice(&[0x31, 0xFF, 0x0F, 0x05]);
    code
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
