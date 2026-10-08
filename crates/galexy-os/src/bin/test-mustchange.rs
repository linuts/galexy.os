//! Default admin password: `create` is `AccessDenied` until `passwd`.
//! The flag lives on the actor (quota bit 15), so a non-shell client
//! cannot skip the shell prompt gate.

#![no_std]
#![no_main]

extern crate alloc;

use bootloader_api::{entry_point, BootInfo};
use galexy_abi::{SysError, Syscall, USER_LOGIN, USER_PASSWD};
use galexy_os::{
    arch::mm, drivers::screen, exit_qemu, println, sched, serial_println, QemuExitCode,
};

entry_point!(test_main_entry, config = &galexy_os::BOOTLOADER_CONFIG);

const DONE: u64 = 0x10C0_0C43;
const TRIPPED: u64 = 0x010C_071A;
const POLL_LIMIT: u64 = 80_000;

#[repr(C)]
#[derive(Clone, Copy)]
struct Report {
    done: u64,
    tripped: u64,
    create_err: u64,
    pass_ok: u64,
    create_ok: u64,
}

fn test_main_entry(boot_info: &'static mut BootInfo) -> ! {
    galexy_os::init();
    screen::init(boot_info);
    println!("[test-mustchange] running");
    serial_println!("[test-mustchange] running");

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

    let (region, owner) = sched::spawn_user_task("chg1", |gr| {
        unsafe {
            core::ptr::write_bytes(mm::frame_virt(gr.scratch_phys).as_mut_ptr::<u8>(), 0, 4096);
        }
        build_blob(gr.code.as_u64(), gr.scratch.as_u64())
    });
    assert_eq!(owner, 0, "first blob stays on the BSP");

    let base = mm::frame_virt(region.scratch_phys);
    let mut elapsed = 0u64;
    loop {
        x86_64::instructions::hlt();
        let tripped = unsafe { core::ptr::read_volatile(base.as_ptr::<u64>().add(1)) };
        if tripped == TRIPPED {
            break;
        }
        elapsed += 1;
        if elapsed > POLL_LIMIT {
            panic!("login never finished");
        }
    }

    let (gen, must) = sched::test_auth_flags("chg1").expect("chg1 live after login");
    assert!(gen > 0, "session generation assigned");
    assert!(must, "default admin password sets must-change");

    elapsed = 0;
    let report = loop {
        x86_64::instructions::hlt();
        let done = unsafe { core::ptr::read_volatile(base.as_ptr::<u64>()) };
        if done == DONE {
            break unsafe { core::ptr::read_volatile(base.as_ptr::<Report>()) };
        }
        elapsed += 1;
        if elapsed > POLL_LIMIT {
            panic!("passwd never finished");
        }
    };

    assert_eq!(report.create_err, SysError::AccessDenied as u64);
    assert_eq!(report.pass_ok, 1, "passwd");
    assert_eq!(report.create_ok, 1, "create after passwd clears must-change");

    loop {
        x86_64::instructions::hlt();
        sched::reap();
        if sched::unreaped_threads() == 0 {
            break;
        }
    }

    println!("[test-mustchange] touch blocked until passwd");
    serial_println!("[test-mustchange] passed");
    exit_qemu(QemuExitCode::Success);
}

fn build_blob(code_base: u64, scratch: u64) -> alloc::vec::Vec<u8> {
    let admin = b"admin";
    let secret = b"secret";
    let path = b"Desktop/n";
    let mut code = alloc::vec::Vec::new();
    let data_len = admin.len() + admin.len() + secret.len() + path.len();
    code.push(0xEB);
    code.push(data_len as u8);
    let admin_addr = code_base + 2;
    code.extend_from_slice(admin);
    let pass_addr = admin_addr + admin.len() as u64;
    code.extend_from_slice(admin);
    let secret_addr = pass_addr + admin.len() as u64;
    code.extend_from_slice(secret);
    let path_addr = secret_addr + secret.len() as u64;
    code.extend_from_slice(path);

    mov_r64_imm(&mut code, 15, scratch);
    call_user_pass(
        &mut code,
        admin_addr,
        admin.len() as u64,
        pass_addr,
        admin.len() as u64,
        USER_LOGIN,
    );
    mov_r64_imm(&mut code, 0, TRIPPED);
    store(&mut code, 0, 0x08);

    call_create(&mut code, path_addr, path.len() as u64);
    store_err(&mut code, 0x10);

    call_passwd_self(&mut code, secret_addr, secret.len() as u64);
    store(&mut code, 2, 0x18);

    call_create(&mut code, path_addr, path.len() as u64);
    store(&mut code, 2, 0x20);

    mov_r64_imm(&mut code, 0, DONE);
    store(&mut code, 0, 0x00);
    mov_eax(&mut code, Syscall::Exit as u32);
    code.extend_from_slice(&[0x31, 0xFF, 0x0F, 0x05]);
    code
}

fn call_user_pass(
    code: &mut alloc::vec::Vec<u8>,
    name: u64,
    name_len: u64,
    pass: u64,
    pass_len: u64,
    op: u64,
) {
    mov_eax(code, Syscall::User as u32);
    mov_r64_imm(code, 7, name);
    mov_r64_imm(code, 6, name_len);
    mov_r64_imm(code, 2, op);
    mov_r64_imm(code, 8, pass);
    mov_r64_imm(code, 9, pass_len);
    code.extend_from_slice(&[0x0F, 0x05]);
}

fn call_passwd_self(code: &mut alloc::vec::Vec<u8>, pass: u64, pass_len: u64) {
    mov_eax(code, Syscall::User as u32);
    code.extend_from_slice(&[0x48, 0x31, 0xFF]);
    code.extend_from_slice(&[0x48, 0x31, 0xF6]);
    mov_r64_imm(code, 2, USER_PASSWD);
    mov_r64_imm(code, 8, pass);
    mov_r64_imm(code, 9, pass_len);
    code.extend_from_slice(&[0x0F, 0x05]);
}

fn call_create(code: &mut alloc::vec::Vec<u8>, addr: u64, len: u64) {
    mov_eax(code, Syscall::Create as u32);
    mov_r64_imm(code, 7, addr);
    mov_r64_imm(code, 6, len);
    code.extend_from_slice(&[0x31, 0xD2]); // xor edx, edx
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

fn store_err(code: &mut alloc::vec::Vec<u8>, disp: i32) {
    code.extend_from_slice(&[0x48, 0x85, 0xD2]);
    code.extend_from_slice(&[0x48, 0xC7, 0xC1, 0, 0, 0, 0]);
    code.extend_from_slice(&[0x48, 0x0F, 0x44, 0xC8]);
    code.extend_from_slice(&[0x48, 0x89, 0xC8]);
    store(code, 0, disp);
}
