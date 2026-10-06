//! Integration test kernel: galfs tokens and owner-qualified paths.
//!
//! Actor `dan` has a Desktop. Alex opens `/dan@Desktop` without a token
//! and gets AccessDenied. After a list+read token is installed, the
//! files snapshot shows `dan@Desktop/` and the secret file opens.

#![no_std]
#![no_main]

extern crate alloc;

use bootloader_api::{entry_point, BootInfo};
use galexy_abi::{SysError, Syscall};
use galexy_os::sched::galfs::{self, FsCred, RIGHT_LIST, RIGHT_READ};
use galexy_os::{
    arch::mm, drivers::screen, exit_qemu, println, sched, serial_println, QemuExitCode,
};

entry_point!(test_main_entry, config = &galexy_os::BOOTLOADER_CONFIG);

const DONE: u64 = 0x5C4A_7C20;
const TICK_TIMEOUT: u64 = 4000;

#[repr(C)]
struct Report {
    done: u64,
    open_dir_ok: u64,
    open_dir_err: u64,
    open_file_ok: u64,
    open_file_err: u64,
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

    let mut fs = FsCred::launcher(galfs::alex_cred().root);
    galfs::push_token(&mut fs.tokens, desktop, RIGHT_LIST | RIGHT_READ).expect("token slot");

    let mut listed = false;
    galfs::for_each_visible(fs.root, &fs.tokens, |path| {
        if path == b"dan@Desktop/" || path == b"dan@Desktop/secret" {
            listed = true;
        }
    });
    assert!(listed, "a list token on dan@Desktop must show that path");

    let (region2, _) = sched::spawn_user_with("allowed", fs, |gr| {
        unsafe {
            core::ptr::write_bytes(mm::frame_virt(gr.scratch_phys).as_mut_ptr::<u8>(), 0, 4096);
        }
        build_allowed_blob(gr.code.as_u64(), gr.scratch.as_u64())
    });

    let scratch2: *const Report = mm::frame_virt(region2.scratch_phys).as_ptr();
    elapsed = 0;
    let report2 = loop {
        x86_64::instructions::hlt();
        let done = unsafe { core::ptr::read_volatile(core::ptr::addr_of!((*scratch2).done)) };
        if done == DONE {
            break unsafe { core::ptr::read_volatile(scratch2) };
        }
        sched::reap();
        elapsed += 1;
        if elapsed > TICK_TIMEOUT {
            panic!("allowed task never finished");
        }
    };

    assert_eq!(
        report2.open_file_ok, 1,
        "open of /dan@Desktop/secret must succeed with a read token"
    );

    loop {
        x86_64::instructions::hlt();
        sched::reap();
        if sched::unreaped_threads() == 0 {
            break;
        }
    }

    println!("[test-galfs] tokens gate dan@Desktop");
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

fn build_allowed_blob(code_base: u64, scratch: u64) -> alloc::vec::Vec<u8> {
    let file = b"/dan@Desktop/secret";
    let mut code: alloc::vec::Vec<u8> = alloc::vec::Vec::new();
    code.push(0xEB);
    code.push(file.len() as u8);
    let file_addr = code_base + 2;
    code.extend_from_slice(file);

    mov_r64_imm(&mut code, 15, scratch);

    syscall_imm(
        &mut code,
        Syscall::Open as u64,
        file_addr,
        file.len() as u64,
    );
    store(&mut code, 2, 0x18);

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
