//! Integration test kernel: galfs tokens and the `grant` syscall.
//!
//! Actor `dan` has a Desktop. Alex opens `/dan@Desktop` without a token
//! and gets AccessDenied. A dan-credentialed granter installs list+read
//! on a live reader via `grant`; the reader then opens the secret.

#![no_std]
#![no_main]

extern crate alloc;

use bootloader_api::{entry_point, BootInfo};
use galexy_abi::{SysError, Syscall, TOKEN_LIST, TOKEN_READ};
use galexy_os::sched::galfs::{self, FsCred};
use galexy_os::{
    arch::mm, drivers::screen, exit_qemu, println, sched, serial_println, QemuExitCode,
};

entry_point!(test_main_entry, config = &galexy_os::BOOTLOADER_CONFIG);

const DONE: u64 = 0x5C4A_7C20;
const TICK_TIMEOUT: u64 = 8000;

#[repr(C)]
struct Report {
    done: u64,
    open_dir_ok: u64,
    open_dir_err: u64,
    open_file_ok: u64,
    open_file_err: u64,
}

#[repr(C)]
struct GrantReport {
    done: u64,
    grant_ok: u64,
    grant_err: u64,
}

fn test_main_entry(boot_info: &'static mut BootInfo) -> ! {
    galexy_os::init();
    screen::init(boot_info);
    println!("[test-galfs] running");
    serial_println!("[test-galfs] running");

    let Some(ramdisk_addr) = boot_info.ramdisk_addr.into_option() else {
        panic!("no ramdisk handed to the kernel");
    };
    let ramdisk_len = boot_info.ramdisk_len as usize;

    mm::init(boot_info);
    galexy_os::arch::init(boot_info);
    sched::init();

    // SAFETY: the bootloader mapped the ramdisk for the kernel's lifetime.
    let archive = unsafe {
        core::slice::from_raw_parts(
            x86_64::VirtAddr::new(ramdisk_addr).as_ptr::<u8>(),
            ramdisk_len,
        )
    };
    sched::ramdisk::init(archive);

    let dan_root = galfs::add_actor_named("dan").expect("dan actor");
    let desktop = galfs::mkdir_under_root(dan_root, "Desktop").expect("dan Desktop");
    let _secret = galfs::create_file_under(desktop, "secret").expect("secret");

    let (region, _) = sched::spawn_user_task("denied", |gr| {
        unsafe {
            core::ptr::write_bytes(mm::frame_virt(gr.scratch_phys).as_mut_ptr::<u8>(), 0, 4096);
        }
        build_denied_blob(gr.code.as_u64(), gr.scratch.as_u64())
    });

    let scratch: *const Report = mm::frame_virt(region.scratch_phys).as_ptr();
    let mut elapsed = 0u64;
    let report = loop {
        x86_64::instructions::hlt();
        let done = unsafe { core::ptr::read_volatile(core::ptr::addr_of!((*scratch).done)) };
        if done == DONE {
            break unsafe { core::ptr::read_volatile(scratch) };
        }
        sched::reap();
        elapsed += 1;
        if elapsed > TICK_TIMEOUT {
            panic!("denied task never finished");
        }
    };

    assert_eq!(report.open_dir_ok, 0, "open of /dan@Desktop must fail");
    assert_eq!(
        report.open_dir_err,
        SysError::AccessDenied as u64,
        "open of /dan@Desktop is AccessDenied without a token"
    );
    assert_eq!(report.open_file_ok, 0, "open of secret must fail");
    assert_eq!(
        report.open_file_err,
        SysError::AccessDenied as u64,
        "open of /dan@Desktop/secret is AccessDenied without a token"
    );

    let mut saw = false;
    galfs::for_each_visible(
        galfs::alex_cred().root,
        &galfs::alex_cred().tokens,
        |path| {
            if path.starts_with(b"dan@") {
                saw = true;
            }
        },
    );
    assert!(!saw, "alex must not list dan's tree without a token");

    // Reader has alex credentials (no dan token). It yields until grant lands.
    let (reader_region, _) = sched::spawn_user_task("reader", |gr| {
        unsafe {
            core::ptr::write_bytes(mm::frame_virt(gr.scratch_phys).as_mut_ptr::<u8>(), 0, 4096);
        }
        build_reader_blob(gr.code.as_u64(), gr.scratch.as_u64())
    });

    let dan_fs = FsCred::launcher(dan_root);
    let (granter_region, _) = sched::spawn_user_with("granter", dan_fs, |gr| {
        unsafe {
            core::ptr::write_bytes(mm::frame_virt(gr.scratch_phys).as_mut_ptr::<u8>(), 0, 4096);
        }
        build_granter_blob(gr.code.as_u64(), gr.scratch.as_u64())
    });

    let granter_scratch: *const GrantReport = mm::frame_virt(granter_region.scratch_phys).as_ptr();
    elapsed = 0;
    let grant_report = loop {
        x86_64::instructions::hlt();
        let done =
            unsafe { core::ptr::read_volatile(core::ptr::addr_of!((*granter_scratch).done)) };
        if done == DONE {
            break unsafe { core::ptr::read_volatile(granter_scratch) };
        }
        sched::reap();
        elapsed += 1;
        if elapsed > TICK_TIMEOUT {
            panic!("granter never finished");
        }
    };
    assert_eq!(grant_report.grant_ok, 1, "grant must succeed");
    assert_eq!(grant_report.grant_err, 0, "grant must not report an error");

    let reader_scratch: *const Report = mm::frame_virt(reader_region.scratch_phys).as_ptr();
    elapsed = 0;
    let reader_report = loop {
        x86_64::instructions::hlt();
        let done = unsafe { core::ptr::read_volatile(core::ptr::addr_of!((*reader_scratch).done)) };
        if done == DONE {
            break unsafe { core::ptr::read_volatile(reader_scratch) };
        }
        sched::reap();
        elapsed += 1;
        if elapsed > TICK_TIMEOUT {
            panic!("reader never finished after grant");
        }
    };
    assert_eq!(
        reader_report.open_file_ok, 1,
        "reader must open /dan@Desktop/secret after grant"
    );

    loop {
        x86_64::instructions::hlt();
        sched::reap();
        if sched::unreaped_threads() == 0 {
            break;
        }
    }

    println!("[test-galfs] grant hands dan@Desktop to reader");
    serial_println!("[test-galfs] passed");
    exit_qemu(QemuExitCode::Success);
}

fn build_denied_blob(code_base: u64, scratch: u64) -> alloc::vec::Vec<u8> {
    let dir = b"/dan@Desktop";
    let file = b"/dan@Desktop/secret";
    let mut code: alloc::vec::Vec<u8> = alloc::vec::Vec::new();
    let data_len = dir.len() + file.len();
    code.push(0xEB);
    code.push(data_len as u8);
    let dir_addr = code_base + 2;
    let file_addr = dir_addr + dir.len() as u64;
    code.extend_from_slice(dir);
    code.extend_from_slice(file);

    mov_r64_imm(&mut code, 15, scratch);

    syscall_imm(&mut code, Syscall::Open as u64, dir_addr, dir.len() as u64);
    store(&mut code, 2, 0x08);
    store(&mut code, 0, 0x10);

    syscall_imm(
        &mut code,
        Syscall::Open as u64,
        file_addr,
        file.len() as u64,
    );
    store(&mut code, 2, 0x18);
    store(&mut code, 0, 0x20);

    mov_r64_imm(&mut code, 0, DONE);
    store(&mut code, 0, 0x00);
    mov_eax(&mut code, Syscall::Exit as u32);
    code.extend_from_slice(&[0x31, 0xFF]);
    code.extend_from_slice(&[0x0F, 0x05]);
    code
}

fn build_reader_blob(code_base: u64, scratch: u64) -> alloc::vec::Vec<u8> {
    let file = b"/dan@Desktop/secret";
    let mut code: alloc::vec::Vec<u8> = alloc::vec::Vec::new();
    code.push(0xEB);
    code.push(file.len() as u8);
    let file_addr = code_base + 2;
    code.extend_from_slice(file);

    mov_r64_imm(&mut code, 15, scratch);

    let loop_at = code.len();
    syscall_imm(
        &mut code,
        Syscall::Open as u64,
        file_addr,
        file.len() as u64,
    );
    // test rdx, rdx ; jnz got_it
    code.extend_from_slice(&[0x48, 0x85, 0xD2]);
    let jnz_at = code.len();
    code.extend_from_slice(&[0x0F, 0x85, 0x00, 0x00, 0x00, 0x00]);

    mov_eax(&mut code, Syscall::Yield as u32);
    code.extend_from_slice(&[0x0F, 0x05]);
    // jmp loop_at
    let after_jmp = code.len() + 5;
    let rel = loop_at as i32 - after_jmp as i32;
    code.push(0xE9);
    code.extend_from_slice(&rel.to_le_bytes());

    let got_it = code.len();
    let jnz_rel = got_it as i32 - (jnz_at as i32 + 6);
    code[jnz_at + 2..jnz_at + 6].copy_from_slice(&jnz_rel.to_le_bytes());

    // open succeeded: rax is the cap, rdx is 1
    store(&mut code, 2, 0x18);

    mov_r64_imm(&mut code, 0, DONE);
    store(&mut code, 0, 0x00);
    mov_eax(&mut code, Syscall::Exit as u32);
    code.extend_from_slice(&[0x31, 0xFF]);
    code.extend_from_slice(&[0x0F, 0x05]);
    code
}

fn build_granter_blob(code_base: u64, scratch: u64) -> alloc::vec::Vec<u8> {
    let path = b"/Desktop";
    let task = b"reader";
    let mut code: alloc::vec::Vec<u8> = alloc::vec::Vec::new();
    let data_len = path.len() + task.len();
    code.push(0xEB);
    code.push(data_len as u8);
    let path_addr = code_base + 2;
    let task_addr = path_addr + path.len() as u64;
    code.extend_from_slice(path);
    code.extend_from_slice(task);

    mov_r64_imm(&mut code, 15, scratch);

    mov_eax(&mut code, Syscall::Grant as u32);
    mov_r64_imm(&mut code, 7, path_addr);
    mov_r64_imm(&mut code, 6, path.len() as u64);
    mov_r64_imm(&mut code, 2, TOKEN_LIST | TOKEN_READ);
    mov_r64_imm(&mut code, 8, task_addr);
    mov_r64_imm(&mut code, 9, task.len() as u64);
    code.extend_from_slice(&[0x0F, 0x05]);
    store(&mut code, 2, 0x08);
    store(&mut code, 0, 0x10);

    mov_r64_imm(&mut code, 0, DONE);
    store(&mut code, 0, 0x00);
    mov_eax(&mut code, Syscall::Exit as u32);
    code.extend_from_slice(&[0x31, 0xFF]);
    code.extend_from_slice(&[0x0F, 0x05]);
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

fn syscall_imm(code: &mut alloc::vec::Vec<u8>, number: u64, a0: u64, a1: u64) {
    mov_eax(code, number as u32);
    mov_r64_imm(code, 7, a0);
    mov_r64_imm(code, 6, a1);
    code.extend_from_slice(&[0x0F, 0x05]);
}

fn store(code: &mut alloc::vec::Vec<u8>, reg: u8, disp: i32) {
    let modrm = 0x80 | (reg << 3) | 7;
    code.extend_from_slice(&[0x49, 0x89, modrm]);
    code.extend_from_slice(&disp.to_le_bytes());
}
