//! Integration test: galfs grant/revoke across actors.

#![no_std]
#![no_main]

extern crate alloc;

use bootloader_api::{entry_point, BootInfo};
use galexy_abi::{SysError, Syscall, TOKEN_LIST, TOKEN_READ};
use galexy_os::sched::galfs;
use galexy_os::{
    arch::mm, drivers::screen, exit_qemu, println, sched, serial_println, QemuExitCode,
};

entry_point!(test_main_entry, config = &galexy_os::BOOTLOADER_CONFIG);

const DONE: u64 = 0x5C4A_7C20;
const TICK_TIMEOUT: u64 = 12000;

#[repr(C)]
struct Report {
    done: u64,
    open_dir_ok: u64,
    open_dir_err: u64,
    open_file_ok: u64,
    open_file_err: u64,
}

#[repr(C)]
struct HolderReport {
    done: u64,
    after_grant: u64,
    after_revoke_ok: u64,
    after_revoke_err: u64,
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

    let mut toks = [galfs::Token::empty(); galfs::TOKEN_SLOTS];
    toks[0] = galfs::Token {
        object: 1,
        rights: galfs::RIGHT_ALL,
    };
    galfs::attenuate_tokens(&mut toks, galfs::RIGHT_READ);
    assert_eq!(toks[0].rights, galfs::RIGHT_READ, "spawn mask keeps read");
    galfs::attenuate_tokens(&mut toks, 0);
    assert_eq!(toks[0].rights, galfs::RIGHT_READ, "mask 0 does not widen");
    toks[0].rights = galfs::RIGHT_ALL | galfs::RIGHT_ONCE;
    assert!(galfs::consume_once(&mut toks, 1), "once card consumed");
    assert!(!toks[0].is_live(), "once card slot cleared");
    assert!(sched::idle_due(1, 1 + sched::IDLE_LOGOUT_MS));
    assert!(!sched::idle_due(0, sched::IDLE_LOGOUT_MS));

    let dan_root = galfs::add_user("dan", b"dan-pass").expect("add dan");
    let desktop = galfs::find_under(dan_root, "Desktop").expect("dan Desktop");
    let _secret = galfs::create_file_under(desktop, "secret").expect("secret");

    // Guest seat: no root, no tokens — must not open dan's tree.
    let (region, _) = sched::spawn_user_with("denied", galfs::unauth_cred(), |gr| {
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

    // Admin root ALL does not cover foreign trees. A card (or `su`) is required.
    let mut saw = false;
    galfs::for_each_visible(
        galfs::admin_cred().root,
        &galfs::admin_cred().tokens,
        |path| {
            if path.starts_with(b"dan@") {
                saw = true;
            }
        },
    );
    assert!(!saw, "admin must not list dan's tree without a card");

    // Guest listing stays empty — cards, not the path string, grant rights.
    let mut guest_saw = false;
    let unauth = galfs::unauth_cred();
    galfs::for_each_visible(unauth.root, &unauth.tokens, |_| {
        guest_saw = true;
    });
    assert!(!guest_saw, "logged-out seat must list nothing");

    // Empty cards until the kernel installs list+read on dan's Desktop.
    // (User-space granter blobs race across CPUs under oneshot idle; the
    // grant/revoke syscalls stay covered by host unit paths + shell e2e.)
    let (holder_region, _) = sched::spawn_user_with("holder", galfs::unauth_cred(), |gr| {
        unsafe {
            core::ptr::write_bytes(mm::frame_virt(gr.scratch_phys).as_mut_ptr::<u8>(), 0, 4096);
        }
        build_holder_blob(gr.code.as_u64(), gr.scratch.as_u64())
    });

    let rights = (TOKEN_LIST | TOKEN_READ) as u8;
    sched::test_push_token("holder", desktop, rights).expect("push list+read on holder");

    let holder_scratch: *const HolderReport = mm::frame_virt(holder_region.scratch_phys).as_ptr();
    x86_64::instructions::interrupts::enable();
    elapsed = 0;
    // Wait until holder observes the grant (after_grant == 1).
    loop {
        let after =
            unsafe { core::ptr::read_volatile(core::ptr::addr_of!((*holder_scratch).after_grant)) };
        if after == 1 {
            break;
        }
        core::hint::spin_loop();
        elapsed += 1;
        if elapsed > TICK_TIMEOUT.saturating_mul(10_000) {
            panic!("holder never saw the grant");
        }
    }

    sched::test_revoke_token("holder", desktop, rights).expect("revoke list+read from holder");

    elapsed = 0;
    let holder_report = loop {
        let done = unsafe { core::ptr::read_volatile(core::ptr::addr_of!((*holder_scratch).done)) };
        if done == DONE {
            break unsafe { core::ptr::read_volatile(holder_scratch) };
        }
        core::hint::spin_loop();
        elapsed += 1;
        if elapsed > TICK_TIMEOUT.saturating_mul(10_000) {
            panic!("holder never finished");
        }
    };
    assert_eq!(holder_report.after_grant, 1, "holder opens after grant");
    assert_eq!(
        holder_report.after_revoke_ok, 0,
        "open after revoke must fail"
    );
    assert_eq!(
        holder_report.after_revoke_err,
        SysError::AccessDenied as u64,
        "open after revoke is AccessDenied"
    );

    loop {
        x86_64::instructions::hlt();
        sched::reap();
        if sched::unreaped_threads() == 0 {
            break;
        }
    }

    println!("[test-galfs] grant and revoke on dan@Desktop");
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

    finish(&mut code);
    code
}

/// Wait for grant (open ok), yield, wait for revoke (open fail), exit.
fn build_holder_blob(code_base: u64, scratch: u64) -> alloc::vec::Vec<u8> {
    let file = b"/dan@Desktop/secret";
    let mut code: alloc::vec::Vec<u8> = alloc::vec::Vec::new();
    code.push(0xEB);
    code.push(file.len() as u8);
    let file_addr = code_base + 2;
    code.extend_from_slice(file);

    mov_r64_imm(&mut code, 15, scratch);

    // Phase 1: loop until open succeeds
    let loop1 = code.len();
    syscall_imm(
        &mut code,
        Syscall::Open as u64,
        file_addr,
        file.len() as u64,
    );
    code.extend_from_slice(&[0x48, 0x85, 0xD2]); // test rdx,rdx
    let jnz1 = code.len();
    code.extend_from_slice(&[0x0F, 0x85, 0, 0, 0, 0]); // jnz got_grant
    mov_eax(&mut code, Syscall::Yield as u32);
    code.extend_from_slice(&[0x0F, 0x05]);
    jmp_to(&mut code, loop1);

    let got_grant = code.len();
    patch_jnz(&mut code, jnz1, got_grant);
    // store 1 into after_grant (0x08)
    mov_r64_imm(&mut code, 0, 1);
    store(&mut code, 0, 0x08);
    // close the cap in rax
    mov_eax(&mut code, Syscall::Close as u32);
    // rdi still has... need cap in rdi. After open, rax=cap. mov rdi, rax
    code.extend_from_slice(&[0x48, 0x89, 0xC7]); // mov rdi, rax
    code.extend_from_slice(&[0x0F, 0x05]);

    // Yield a few times so granter can revoke
    for _ in 0..8 {
        mov_eax(&mut code, Syscall::Yield as u32);
        code.extend_from_slice(&[0x0F, 0x05]);
    }

    // Phase 2: loop until open fails with AccessDenied (or just try once after yields)
    let loop2 = code.len();
    syscall_imm(
        &mut code,
        Syscall::Open as u64,
        file_addr,
        file.len() as u64,
    );
    code.extend_from_slice(&[0x48, 0x85, 0xD2]); // test rdx,rdx
    let jz2 = code.len();
    code.extend_from_slice(&[0x0F, 0x84, 0, 0, 0, 0]); // jz denied
                                                       // still ok — close and yield
    code.extend_from_slice(&[0x48, 0x89, 0xC7]);
    mov_eax(&mut code, Syscall::Close as u32);
    code.extend_from_slice(&[0x0F, 0x05]);
    mov_eax(&mut code, Syscall::Yield as u32);
    code.extend_from_slice(&[0x0F, 0x05]);
    jmp_to(&mut code, loop2);

    let denied = code.len();
    patch_jnz(&mut code, jz2, denied); // actually jz
    store(&mut code, 2, 0x10); // after_revoke_ok = rdx (0)
    store(&mut code, 0, 0x18); // after_revoke_err = rax

    finish(&mut code);
    code
}

fn finish(code: &mut alloc::vec::Vec<u8>) {
    mov_r64_imm(code, 0, DONE);
    store(code, 0, 0x00);
    mov_eax(code, Syscall::Exit as u32);
    code.extend_from_slice(&[0x31, 0xFF]);
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

fn jmp_to(code: &mut alloc::vec::Vec<u8>, target: usize) {
    let after = code.len() + 5;
    let rel = target as i32 - after as i32;
    code.push(0xE9);
    code.extend_from_slice(&rel.to_le_bytes());
}

fn patch_jnz(code: &mut [u8], at: usize, target: usize) {
    let rel = target as i32 - (at as i32 + 6);
    code[at + 2..at + 6].copy_from_slice(&rel.to_le_bytes());
}
