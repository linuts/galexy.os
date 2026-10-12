//! Idle lock uses `timer_ticks`. The test backdates the last keystroke
//! (tick injection) instead of waiting out [`sched::IDLE_LOGOUT_MS`].
//! The blob also locks itself, proves `sync` returns `Locked`, then
//! unlocks before the idle scan freezes the seat again.

#![no_std]
#![no_main]

extern crate alloc;

use bootloader_api::{entry_point, BootInfo};
use galexy_abi::{SysError, Syscall, USER_LOCK, USER_LOCKED, USER_LOGIN, USER_UNLOCK_SESSION};
use galexy_os::{
    arch::mm, drivers::screen, exit_qemu, println, sched, serial_println, QemuExitCode,
};

entry_point!(test_main_entry, config = &galexy_os::BOOTLOADER_CONFIG);

const TRIPPED: u64 = 0x10C0_1D1E;
const POLL_LIMIT: u64 = 80_000;

fn test_main_entry(boot_info: &'static mut BootInfo) -> ! {
    galexy_os::init();
    screen::init(boot_info);
    println!("[test-idle] running");
    serial_println!("[test-idle] running");

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

    let (region, owner) = sched::spawn_user_task("idle1", |gr| {
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

    let queried = unsafe { core::ptr::read_volatile(base.as_ptr::<u64>().add(2)) };
    let denied = unsafe { core::ptr::read_volatile(base.as_ptr::<u64>().add(3)) };
    assert_eq!(queried, 1, "USER_LOCKED reports the freeze");
    assert_eq!(
        denied,
        SysError::Locked as u64,
        "sync is refused while the session is locked"
    );
    assert!(
        !sched::test_session_locked("idle1"),
        "unlock opens the same seat"
    );

    // Boot is only a few seconds in, so backdating a full idle window
    // saturates to 0 and the scan treats that as "no sample". Shrink the
    // window instead.
    sched::test_set_idle_limit(1);
    sched::test_backdate_input("idle1", 5);
    sched::test_poll_idle_all();
    assert!(
        sched::test_session_locked("idle1"),
        "idle lock freezes the seat"
    );
    assert!(
        sched::test_auth_flags("idle1").is_some(),
        "idle lock keeps the session"
    );
    serial_println!("[test-idle] locked");

    loop {
        x86_64::instructions::hlt();
        sched::reap();
        if sched::unreaped_threads() == 0 {
            break;
        }
    }

    println!("[test-idle] locked");
    serial_println!("[test-idle] passed");
    exit_qemu(QemuExitCode::Success);
}

fn build_blob(code_base: u64, scratch: u64) -> alloc::vec::Vec<u8> {
    let admin = b"admin";
    let mut code = alloc::vec::Vec::new();
    let data_len = admin.len() * 2;
    code.push(0xEB);
    code.push(data_len as u8);
    let name_addr = code_base + 2;
    code.extend_from_slice(admin);
    let pass_addr = name_addr + admin.len() as u64;
    code.extend_from_slice(admin);

    mov_r64_imm(&mut code, 15, scratch);
    mov_eax(&mut code, Syscall::User as u32);
    mov_r64_imm(&mut code, 7, name_addr);
    mov_r64_imm(&mut code, 6, admin.len() as u64);
    mov_r64_imm(&mut code, 2, USER_LOGIN);
    mov_r64_imm(&mut code, 8, pass_addr);
    mov_r64_imm(&mut code, 9, admin.len() as u64);
    code.extend_from_slice(&[0x0F, 0x05]);

    // Lock, prove a non-console syscall is refused, then unlock.
    mov_eax(&mut code, Syscall::User as u32);
    mov_r64_imm(&mut code, 7, 0);
    mov_r64_imm(&mut code, 6, 0);
    mov_r64_imm(&mut code, 2, USER_LOCK);
    mov_r64_imm(&mut code, 8, 0);
    mov_r64_imm(&mut code, 9, 0);
    code.extend_from_slice(&[0x0F, 0x05]);

    mov_eax(&mut code, Syscall::User as u32);
    mov_r64_imm(&mut code, 2, USER_LOCKED);
    code.extend_from_slice(&[0x0F, 0x05]);
    store(&mut code, 0, 0x10);

    mov_eax(&mut code, Syscall::Sync as u32);
    code.extend_from_slice(&[0x0F, 0x05]);
    store(&mut code, 0, 0x18);

    mov_eax(&mut code, Syscall::User as u32);
    mov_r64_imm(&mut code, 7, 0);
    mov_r64_imm(&mut code, 6, 0);
    mov_r64_imm(&mut code, 2, USER_UNLOCK_SESSION);
    mov_r64_imm(&mut code, 8, pass_addr);
    mov_r64_imm(&mut code, 9, admin.len() as u64);
    code.extend_from_slice(&[0x0F, 0x05]);

    mov_r64_imm(&mut code, 0, TRIPPED);
    store(&mut code, 0, 0x08);

    // Park so the BSP can idle-lock this task without it exiting.
    mov_eax(&mut code, Syscall::Sleep as u32);
    mov_r64_imm(&mut code, 7, 30_000);
    code.extend_from_slice(&[0x0F, 0x05]);

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
