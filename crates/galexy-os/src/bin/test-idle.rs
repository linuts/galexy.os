//! Idle lock uses `timer_ticks`. The test backdates the last keystroke
//! (tick injection) instead of waiting out [`sched::IDLE_LOGOUT_MS`].
//! The blob also locks itself, proves `sync` returns `Locked`, then
//! unlocks before the idle scan freezes the seat again.
//!
//! A second logged-in task named `probe` (not a console shell) keeps
//! calling `sync` while the seat is locked. The production idle scan
//! leaves that task unlocked.

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
const HELD: u64 = 0x10C0_4E1D;
const READY: u64 = 0x10C0_5EC0;
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

    // The service logs in first and then loops, so its `sync` calls
    // overlap the seat's lock instead of racing the password KDF.
    let (probe_region, probe_owner) = sched::spawn_user_task_on("probe", 0, |gr| {
        unsafe {
            core::ptr::write_bytes(mm::frame_virt(gr.scratch_phys).as_mut_ptr::<u8>(), 0, 4096);
        }
        build_probe(gr.code.as_u64(), gr.scratch.as_u64())
    });
    assert_eq!(probe_owner, 0, "probe stays on the BSP");
    let probe_base = mm::frame_virt(probe_region.scratch_phys);
    wait_word(probe_base, 0, READY, "service never logged in");

    let (region, owner) = sched::spawn_user_task_on("idle1", 0, |gr| {
        unsafe {
            core::ptr::write_bytes(mm::frame_virt(gr.scratch_phys).as_mut_ptr::<u8>(), 0, 4096);
        }
        build_blob(gr.code.as_u64(), gr.scratch.as_u64())
    });
    assert_eq!(owner, 0, "first blob stays on the BSP");

    let base = mm::frame_virt(region.scratch_phys);
    wait_word(base, 4, HELD, "seat never locked");

    let before = read_word(probe_base, 4);
    let mut elapsed = 0u64;
    let after = loop {
        x86_64::instructions::hlt();
        let ticks = read_word(probe_base, 4);
        if ticks > before {
            break ticks;
        }
        elapsed += 1;
        if elapsed > POLL_LIMIT {
            panic!("service stalled while the seat was locked");
        }
    };
    assert!(
        after > before,
        "a service kept running while the seat was locked"
    );
    assert_eq!(
        read_word(probe_base, 2),
        0,
        "service sync value while the seat was locked"
    );
    assert_eq!(
        read_word(probe_base, 3),
        1,
        "service sync succeeded while the seat was locked"
    );
    assert!(
        !sched::test_session_locked("probe"),
        "locking the seat does not lock a service"
    );
    assert!(
        sched::test_session_locked("idle1"),
        "the seat stays locked while the service runs"
    );

    // Production scan: a logged-in non-shell past the idle window stays open.
    sched::test_set_idle_limit(1);
    sched::test_backdate_input("probe", 5);
    sched::poll_idle_locks();
    assert!(
        !sched::test_session_locked("probe"),
        "idle lock skips services"
    );
    serial_println!("[test-idle] service ran");

    unsafe {
        core::ptr::write_volatile(probe_base.as_mut_ptr::<u64>().add(1), 1);
    }
    elapsed = 0;
    loop {
        x86_64::instructions::hlt();
        sched::reap();
        if !sched::is_name_live("probe") {
            break;
        }
        elapsed += 1;
        if elapsed > POLL_LIMIT {
            panic!("service never exited");
        }
    }

    unsafe {
        core::ptr::write_volatile(base.as_mut_ptr::<u64>().add(5), 1);
    }
    wait_word(base, 1, TRIPPED, "login never finished");

    let queried = read_word(base, 2);
    let denied = read_word(base, 3);
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

fn read_word(base: x86_64::VirtAddr, index: usize) -> u64 {
    unsafe { core::ptr::read_volatile(base.as_ptr::<u64>().add(index)) }
}

fn wait_word(base: x86_64::VirtAddr, index: usize, want: u64, what: &str) {
    let mut elapsed = 0u64;
    loop {
        x86_64::instructions::hlt();
        if read_word(base, index) == want {
            return;
        }
        elapsed += 1;
        if elapsed > POLL_LIMIT {
            panic!("{}", what);
        }
    }
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
    login(&mut code, name_addr, pass_addr, admin.len() as u64);

    // Lock, prove a non-console syscall is refused, then hold the freeze
    // until the BSP has watched the service run.
    mov_eax(&mut code, Syscall::User as u32);
    mov_r64_imm(&mut code, 7, 0);
    mov_r64_imm(&mut code, 6, 0);
    mov_r64_imm(&mut code, 2, USER_LOCK);
    mov_r64_imm(&mut code, 8, 0);
    mov_r64_imm(&mut code, 9, 0);
    syscall(&mut code);

    mov_eax(&mut code, Syscall::User as u32);
    mov_r64_imm(&mut code, 2, USER_LOCKED);
    syscall(&mut code);
    store(&mut code, 0, 0x10);

    mov_eax(&mut code, Syscall::Sync as u32);
    syscall(&mut code);
    store(&mut code, 0, 0x18);

    mov_r64_imm(&mut code, 0, HELD);
    store(&mut code, 0, 0x20);
    let hold = code.len();
    load(&mut code, 0, 0x28);
    let resume = test_jnz(&mut code);
    mov_eax(&mut code, Syscall::Yield as u32);
    syscall(&mut code);
    jmp_back(&mut code, hold);
    patch_rel8(&mut code, resume);

    mov_eax(&mut code, Syscall::User as u32);
    mov_r64_imm(&mut code, 7, 0);
    mov_r64_imm(&mut code, 6, 0);
    mov_r64_imm(&mut code, 2, USER_UNLOCK_SESSION);
    mov_r64_imm(&mut code, 8, pass_addr);
    mov_r64_imm(&mut code, 9, admin.len() as u64);
    syscall(&mut code);

    mov_r64_imm(&mut code, 0, TRIPPED);
    store(&mut code, 0, 0x08);

    // Park so the BSP can idle-lock this task without it exiting.
    mov_eax(&mut code, Syscall::Sleep as u32);
    mov_r64_imm(&mut code, 7, 30_000);
    syscall(&mut code);

    mov_eax(&mut code, Syscall::Exit as u32);
    code.extend_from_slice(&[0x31, 0xFF, 0x0F, 0x05]);
    code
}

/// Logged-in non-shell. `sync` each pass, yield, stop when scratch+8 is set.
fn build_probe(code_base: u64, scratch: u64) -> alloc::vec::Vec<u8> {
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
    login(&mut code, name_addr, pass_addr, admin.len() as u64);

    mov_r64_imm(&mut code, 0, READY);
    store(&mut code, 0, 0x00);

    let loop_at = code.len();
    load(&mut code, 0, 0x08);
    let stop = test_jnz(&mut code);
    mov_eax(&mut code, Syscall::Sync as u32);
    syscall(&mut code);
    store(&mut code, 0, 0x10);
    store(&mut code, 2, 0x18);
    inc_qword(&mut code, 0x20);
    mov_eax(&mut code, Syscall::Yield as u32);
    syscall(&mut code);
    jmp_back(&mut code, loop_at);
    patch_rel8(&mut code, stop);

    mov_eax(&mut code, Syscall::Exit as u32);
    code.extend_from_slice(&[0x31, 0xFF, 0x0F, 0x05]);
    code
}

fn login(code: &mut alloc::vec::Vec<u8>, name_addr: u64, pass_addr: u64, len: u64) {
    mov_eax(code, Syscall::User as u32);
    mov_r64_imm(code, 7, name_addr);
    mov_r64_imm(code, 6, len);
    mov_r64_imm(code, 2, USER_LOGIN);
    mov_r64_imm(code, 8, pass_addr);
    mov_r64_imm(code, 9, len);
    syscall(code);
}

fn syscall(code: &mut alloc::vec::Vec<u8>) {
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

fn load(code: &mut alloc::vec::Vec<u8>, reg: u8, disp: i32) {
    let modrm = 0x80 | (reg << 3) | 7;
    code.extend_from_slice(&[0x49, 0x8B, modrm]);
    code.extend_from_slice(&disp.to_le_bytes());
}

fn inc_qword(code: &mut alloc::vec::Vec<u8>, disp: i32) {
    code.extend_from_slice(&[0x49, 0xFF, 0x87]);
    code.extend_from_slice(&disp.to_le_bytes());
}

/// `test rax, rax; jnz rel8`. Returns the index of the rel8 byte.
fn test_jnz(code: &mut alloc::vec::Vec<u8>) -> usize {
    code.extend_from_slice(&[0x48, 0x85, 0xC0, 0x75, 0x00]);
    code.len() - 1
}

fn patch_rel8(code: &mut [u8], rel_at: usize) {
    let next = rel_at + 1;
    let rel = code.len() as isize - next as isize;
    assert!((-128..128).contains(&rel), "rel8 out of range");
    code[rel_at] = rel as u8;
}

fn jmp_back(code: &mut alloc::vec::Vec<u8>, target: usize) {
    let at = code.len();
    code.extend_from_slice(&[0xEB, 0x00]);
    let rel = target as isize - (at as isize + 2);
    assert!((-128..128).contains(&rel), "jmp rel8 out of range");
    code[at + 1] = rel as u8;
}
