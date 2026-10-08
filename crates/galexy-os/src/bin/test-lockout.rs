//! Login cool-down: five misses lock the actor and the TTY; the correct
//! password is refused until monotonic `timer_ticks` passes the deadline.
//!
//! The blob sleeps (same clock) across the cool-down, then logs in. The
//! kernel does not reap until `done` is visible — reap frees the scratch
//! page the report lives on.

#![no_std]
#![no_main]

extern crate alloc;

use bootloader_api::{entry_point, BootInfo};
use galexy_abi::{SysError, Syscall, USER_ADD, USER_LOGIN, USER_WHOAMI};
use galexy_core::LOCKOUT_COOLDOWN_MS;
use galexy_os::{
    arch, arch::mm, drivers::screen, exit_qemu, println, sched, serial_println, QemuExitCode,
};

entry_point!(test_main_entry, config = &galexy_os::BOOTLOADER_CONFIG);

const DONE: u64 = 0x10C0_0A11;
const TRIPPED: u64 = 0x10C0_719;
/// Iterations of the poll loop, not milliseconds. Busy KDF ticks are ~1–2 ms.
const POLL_LIMIT: u64 = 80_000;

#[repr(C)]
#[derive(Clone, Copy)]
struct Report {
    done: u64,
    tripped: u64,
    under_err: u64,
    under_ok: u64,
    who_n: u64,
    who: [u8; 16],
    fifth_err: u64,
    locked_err: u64,
    add_ok: u64,
    after_ok: u64,
    who2_n: u64,
    who2: [u8; 16],
}

fn test_main_entry(boot_info: &'static mut BootInfo) -> ! {
    galexy_os::init();
    screen::init(boot_info);
    println!("[test-lockout] running");
    serial_println!("[test-lockout] running");

    let Some(ramdisk_addr) = boot_info.ramdisk_addr.into_option() else {
        panic!("no ramdisk");
    };
    mm::init(boot_info);
    arch::init(boot_info);
    sched::init();
    let archive = unsafe {
        core::slice::from_raw_parts(
            x86_64::VirtAddr::new(ramdisk_addr).as_ptr::<u8>(),
            boot_info.ramdisk_len as usize,
        )
    };
    sched::ramdisk::init(archive);

    let (region, owner) = sched::spawn_user_task("lock1", |gr| {
        unsafe {
            core::ptr::write_bytes(mm::frame_virt(gr.scratch_phys).as_mut_ptr::<u8>(), 0, 4096);
        }
        build_blob(gr.code.as_u64(), gr.scratch.as_u64())
    });
    assert_eq!(
        owner, 0,
        "first blob stays on the BSP so the AP cannot reap it"
    );

    let base = mm::frame_virt(region.scratch_phys);
    // No reap in this loop: an exited task's scratch is wiped on reap.
    let mut elapsed = 0u64;
    loop {
        x86_64::instructions::hlt();
        let tripped = unsafe { core::ptr::read_volatile(base.as_ptr::<u64>().add(1)) };
        if tripped == TRIPPED {
            break;
        }
        elapsed += 1;
        if elapsed > POLL_LIMIT {
            panic!("lockout never tripped");
        }
    }

    let now = arch::timer_ticks();
    let actor_until = sched::lockout::lockout_actor_until("eve");
    let tty_until = sched::lockout::lockout_tty_until(0);
    assert!(actor_until > now, "actor deadline {actor_until} now {now}");
    assert!(tty_until > now, "tty deadline {tty_until} now {now}");
    let remain = actor_until - now;
    assert!(remain <= LOCKOUT_COOLDOWN_MS);
    assert!(
        LOCKOUT_COOLDOWN_MS - remain < 2_000,
        "cool-down not armed from timer_ticks (remain {remain})"
    );
    assert!(
        tty_until.abs_diff(actor_until) < 50,
        "tty and actor deadlines diverged"
    );
    assert_eq!(sched::lockout::lockout_active(), 2, "actor + tty");

    elapsed = 0;
    let report = loop {
        x86_64::instructions::hlt();
        let done = unsafe { core::ptr::read_volatile(base.as_ptr::<u64>()) };
        if done == DONE {
            break unsafe { core::ptr::read_volatile(base.as_ptr::<Report>()) };
        }
        elapsed += 1;
        if elapsed > POLL_LIMIT {
            panic!("login after cool-down never finished");
        }
    };

    assert_eq!(report.add_ok, 1, "useradd eve");
    assert_eq!(report.under_err, SysError::AccessDenied as u64);
    assert_eq!(report.under_ok, 1, "four misses must not lock");
    assert_eq!(&report.who[..report.who_n as usize], b"eve");
    assert_eq!(report.fifth_err, SysError::AccessDenied as u64);
    assert_eq!(report.locked_err, SysError::Locked as u64);
    assert_eq!(report.after_ok, 1, "login after cool-down");
    assert_eq!(&report.who2[..report.who2_n as usize], b"eve");
    assert_eq!(sched::lockout::lockout_active(), 0);

    loop {
        x86_64::instructions::hlt();
        sched::reap();
        if sched::unreaped_threads() == 0 {
            break;
        }
    }

    println!("[test-lockout] cool-down elapsed");
    serial_println!("[test-lockout] passed");
    exit_qemu(QemuExitCode::Success);
}

fn build_blob(code_base: u64, scratch: u64) -> alloc::vec::Vec<u8> {
    let eve = b"eve";
    let secret = b"secret";
    let wrong = b"wrong";
    let mut code = alloc::vec::Vec::new();
    let data_len = eve.len() + secret.len() + wrong.len();
    code.push(0xEB);
    code.push(data_len as u8);
    let eve_addr = code_base + 2;
    code.extend_from_slice(eve);
    let secret_addr = eve_addr + eve.len() as u64;
    code.extend_from_slice(secret);
    let wrong_addr = secret_addr + secret.len() as u64;
    code.extend_from_slice(wrong);

    mov_r64_imm(&mut code, 15, scratch);
    call_user_pass(
        &mut code,
        eve_addr,
        eve.len() as u64,
        secret_addr,
        secret.len() as u64,
        USER_ADD,
    );
    store(&mut code, 2, 0x48);

    for i in 0..4 {
        call_user_pass(
            &mut code,
            eve_addr,
            eve.len() as u64,
            wrong_addr,
            wrong.len() as u64,
            USER_LOGIN,
        );
        if i == 3 {
            store_err(&mut code, 0x10);
        }
    }
    call_user_pass(
        &mut code,
        eve_addr,
        eve.len() as u64,
        secret_addr,
        secret.len() as u64,
        USER_LOGIN,
    );
    store(&mut code, 2, 0x18);
    call_user(&mut code, scratch + 0x28, 16, USER_WHOAMI);
    store(&mut code, 0, 0x20);

    for i in 0..5 {
        call_user_pass(
            &mut code,
            eve_addr,
            eve.len() as u64,
            wrong_addr,
            wrong.len() as u64,
            USER_LOGIN,
        );
        if i == 4 {
            store_err(&mut code, 0x38);
        }
    }
    call_user_pass(
        &mut code,
        eve_addr,
        eve.len() as u64,
        secret_addr,
        secret.len() as u64,
        USER_LOGIN,
    );
    store_err(&mut code, 0x40);

    mov_r64_imm(&mut code, 0, TRIPPED);
    store(&mut code, 0, 0x08);
    // Cool-down is `LOCKOUT_COOLDOWN_MS` from the arming guess, which
    // already happened. Sleeping that long on `timer_ticks` lands past it.
    call_sleep(&mut code, LOCKOUT_COOLDOWN_MS);

    call_user_pass(
        &mut code,
        eve_addr,
        eve.len() as u64,
        secret_addr,
        secret.len() as u64,
        USER_LOGIN,
    );
    store(&mut code, 2, 0x50);
    call_user(&mut code, scratch + 0x60, 16, USER_WHOAMI);
    store(&mut code, 0, 0x58);

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

fn call_sleep(code: &mut alloc::vec::Vec<u8>, ms: u64) {
    mov_eax(code, Syscall::Sleep as u32);
    mov_r64_imm(code, 7, ms);
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

/// Store the syscall error code, or 0 when `rdx` says success.
fn store_err(code: &mut alloc::vec::Vec<u8>, disp: i32) {
    code.extend_from_slice(&[0x48, 0x85, 0xD2]);
    code.extend_from_slice(&[0x48, 0xC7, 0xC1, 0, 0, 0, 0]);
    code.extend_from_slice(&[0x48, 0x0F, 0x44, 0xC8]);
    code.extend_from_slice(&[0x48, 0x89, 0xC8]);
    store(code, 0, disp);
}
