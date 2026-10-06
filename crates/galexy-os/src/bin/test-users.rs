//! Integration test: user management (whoami, users, add, del, su).

#![no_std]
#![no_main]

extern crate alloc;

use bootloader_api::{entry_point, BootInfo};
use galexy_abi::{SysError, Syscall, USER_ADD, USER_DEL, USER_SU, USER_USERS, USER_WHOAMI};
use galexy_os::{
    arch::mm, drivers::screen, exit_qemu, println, sched, serial_println, QemuExitCode,
};

entry_point!(test_main_entry, config = &galexy_os::BOOTLOADER_CONFIG);

const DONE: u64 = 0x5C4A_7C30;
const TICK_TIMEOUT: u64 = 8000;

#[repr(C)]
struct Report {
    done: u64,
    who_n: u64,
    who: [u8; 16],
    users_n: u64,
    users: [u8; 64],
    add_ok: u64,
    su_ok: u64,
    who2_n: u64,
    who2: [u8; 16],
    add_as_eve_err: u64,
    su_admin_ok: u64,
    del_ok: u64,
    del_admin_err: u64,
}

fn test_main_entry(boot_info: &'static mut BootInfo) -> ! {
    galexy_os::init();
    screen::init(boot_info);
    println!("[test-users] running");
    serial_println!("[test-users] running");

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

    let (region, _) = sched::spawn_user_task("mgr", |gr| {
        unsafe {
            core::ptr::write_bytes(mm::frame_virt(gr.scratch_phys).as_mut_ptr::<u8>(), 0, 4096);
        }
        build_blob(gr.code.as_u64(), gr.scratch.as_u64())
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
            panic!("mgr never finished");
        }
    };

    assert_eq!(&report.who[..report.who_n as usize], b"admin");
    let users = &report.users[..report.users_n as usize];
    assert!(users.windows(6).any(|w| w == b"admin\n"));
    assert_eq!(report.add_ok, 1);
    assert_eq!(report.su_ok, 1);
    assert_eq!(&report.who2[..report.who2_n as usize], b"eve");
    assert_eq!(report.add_as_eve_err, SysError::AccessDenied as u64);
    assert_eq!(report.su_admin_ok, 1);
    assert_eq!(report.del_ok, 1);
    assert_eq!(report.del_admin_err, SysError::Unsupported as u64);

    loop {
        x86_64::instructions::hlt();
        sched::reap();
        if sched::unreaped_threads() == 0 {
            break;
        }
    }

    println!("[test-users] accounts work");
    serial_println!("[test-users] passed");
    exit_qemu(QemuExitCode::Success);
}

fn build_blob(code_base: u64, scratch: u64) -> alloc::vec::Vec<u8> {
    let eve = b"eve";
    let admin = b"admin";
    let bob = b"bob";
    let mut code = alloc::vec::Vec::new();
    let data_len = eve.len() + admin.len() + bob.len();
    code.push(0xEB);
    code.push(data_len as u8);
    let eve_addr = code_base + 2;
    code.extend_from_slice(eve);
    let admin_addr = eve_addr + eve.len() as u64;
    code.extend_from_slice(admin);
    let bob_addr = admin_addr + admin.len() as u64;
    code.extend_from_slice(bob);

    mov_r64_imm(&mut code, 15, scratch);

    call_user(&mut code, scratch + 0x10, 16, USER_WHOAMI);
    store(&mut code, 0, 0x08);

    call_user(&mut code, scratch + 0x28, 64, USER_USERS);
    store(&mut code, 0, 0x20);

    call_user_name(&mut code, eve_addr, eve.len() as u64, USER_ADD);
    store(&mut code, 2, 0x68);

    call_user_name(&mut code, eve_addr, eve.len() as u64, USER_SU);
    store(&mut code, 2, 0x70);

    call_user(&mut code, scratch + 0x80, 16, USER_WHOAMI);
    store(&mut code, 0, 0x78);

    call_user_name(&mut code, bob_addr, bob.len() as u64, USER_ADD);
    code.extend_from_slice(&[0x48, 0x85, 0xD2]);
    code.extend_from_slice(&[0x48, 0xC7, 0xC1, 0, 0, 0, 0]);
    code.extend_from_slice(&[0x48, 0x0F, 0x44, 0xC8]);
    code.extend_from_slice(&[0x48, 0x89, 0xC8]);
    store(&mut code, 0, 0x90);

    // born_admin may return to admin
    call_user_name(&mut code, admin_addr, admin.len() as u64, USER_SU);
    store(&mut code, 2, 0x98);

    call_user_name(&mut code, eve_addr, eve.len() as u64, USER_DEL);
    store(&mut code, 2, 0xa0);

    call_user_name(&mut code, admin_addr, admin.len() as u64, USER_DEL);
    code.extend_from_slice(&[0x48, 0x85, 0xD2]);
    code.extend_from_slice(&[0x48, 0xC7, 0xC1, 0, 0, 0, 0]);
    code.extend_from_slice(&[0x48, 0x0F, 0x44, 0xC8]);
    code.extend_from_slice(&[0x48, 0x89, 0xC8]);
    store(&mut code, 0, 0xa8);

    mov_r64_imm(&mut code, 0, DONE);
    store(&mut code, 0, 0x00);
    mov_eax(&mut code, Syscall::Exit as u32);
    code.extend_from_slice(&[0x31, 0xFF, 0x0F, 0x05]);
    code
}

fn call_user(code: &mut alloc::vec::Vec<u8>, addr: u64, len: u64, op: u64) {
    mov_eax(code, Syscall::User as u32);
    mov_r64_imm(code, 7, addr);
    mov_r64_imm(code, 6, len);
    mov_r64_imm(code, 2, op);
    code.extend_from_slice(&[0x0F, 0x05]);
}

fn call_user_name(code: &mut alloc::vec::Vec<u8>, addr: u64, len: u64, op: u64) {
    call_user(code, addr, len, op);
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
