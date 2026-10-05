//! Integration test kernel: ramdisk files as capabilities.
//!
//! A ring-3 blob `open`s a missing name, `open`s `banner.txt`, `read`s it,
//! proves a WRITE-only forgery of that cap is denied, reads to EOF, `close`s,
//! and shows the closed cap is gone. The kernel checks the scratch report.

#![no_std]
#![no_main]

extern crate alloc;

use bootloader_api::{entry_point, BootInfo};
use galexy_abi::{Cap, CapRights, SysError, Syscall, FILE_CAP_BASE};
use galexy_os::{
    arch::mm, drivers::screen, exit_qemu, println, sched, serial_println, QemuExitCode,
};

entry_point!(test_main_entry, config = &galexy_os::BOOTLOADER_CONFIG);

/// Packed by the runner (`build.rs`). The blob reads this through `open`.
const BANNER: &[u8] = b"galexy ramdisk plumbing works";
const DONE: u64 = 0x0F11_E000;
const TICK_TIMEOUT: u64 = 4000;

#[repr(C)]
struct Report {
    done: u64,
    missing_ok: u64,
    missing_err: u64,
    open_ok: u64,
    cap_bits: u64,
    read_n: u64,
    data: [u8; 32],
    denied_ok: u64,
    denied_err: u64,
    eof_n: u64,
    eof_ok: u64,
    close_ok: u64,
    closed_ok: u64,
    closed_err: u64,
}

fn test_main_entry(boot_info: &'static mut BootInfo) -> ! {
    galexy_os::init();
    screen::init(boot_info);
    println!("[test-open] running");
    serial_println!("[test-open] running");

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

    let (region, _) = sched::spawn_user_task("opener", |gr| {
        // Zero before the task is queued (this closure runs first). A dirty
        // frame must not look like the done marker.
        // SAFETY: the scratch frame was just allocated and is not yet mapped
        // into a running task. The phys map reaches it.
        unsafe {
            core::ptr::write_bytes(mm::frame_virt(gr.scratch_phys).as_mut_ptr::<u8>(), 0, 4096);
        }
        build_blob(gr.code.as_u64(), gr.scratch.as_u64())
    });

    let scratch: *const Report = mm::frame_virt(region.scratch_phys).as_ptr();
    let mut elapsed = 0u64;
    let report = loop {
        x86_64::instructions::hlt();
        // SAFETY: the scratch frame stays mapped until reap; we read it
        // through the phys map, and the blob writes the marker last.
        let done = unsafe { core::ptr::read_volatile(core::ptr::addr_of!((*scratch).done)) };
        if done == DONE {
            break unsafe { core::ptr::read_volatile(scratch) };
        }
        sched::reap();
        elapsed += 1;
        if elapsed > TICK_TIMEOUT {
            panic!("opener never finished the open/read/close report");
        }
    };

    assert_eq!(report.missing_ok, 0, "missing open must fail");
    assert_eq!(
        report.missing_err,
        SysError::NotFound as u64,
        "missing open must be NotFound"
    );

    assert_eq!(report.open_ok, 1, "banner.txt must open");
    let cap = Cap::from_bits(report.cap_bits);
    assert_eq!(cap.index(), FILE_CAP_BASE, "first file cap is index 3");
    assert!(cap.rights().contains(CapRights::READ));
    assert!(!cap.rights().contains(CapRights::WRITE));

    assert_eq!(
        report.read_n,
        BANNER.len() as u64,
        "read returns the file length"
    );
    assert_eq!(
        &report.data[..BANNER.len()],
        BANNER,
        "read bytes must match banner.txt"
    );

    assert_eq!(report.denied_ok, 0, "WRITE-only forgery must fail");
    assert_eq!(
        report.denied_err,
        SysError::AccessDenied as u64,
        "forgery is AccessDenied, not a successful read"
    );

    assert_eq!(report.eof_ok, 1, "read at EOF succeeds");
    assert_eq!(report.eof_n, 0, "read at EOF returns 0");

    assert_eq!(report.close_ok, 1, "close succeeds");
    assert_eq!(report.closed_ok, 0, "read after close fails");
    assert_eq!(
        report.closed_err,
        SysError::BadCap as u64,
        "closed cap is BadCap"
    );

    loop {
        x86_64::instructions::hlt();
        sched::reap();
        if sched::unreaped_threads() == 0 {
            break;
        }
    }

    println!("[test-open] open/read/close round-tripped banner.txt");
    println!("[test-open] all assertions passed");
    serial_println!("[test-open] passed");
    exit_qemu(QemuExitCode::Success);
}

fn build_blob(code_base: u64, scratch: u64) -> alloc::vec::Vec<u8> {
    let missing = b"no-such";
    let banner = b"banner.txt";
    let mut code: alloc::vec::Vec<u8> = alloc::vec::Vec::new();
    // jmp over the two names.
    code.push(0xEB);
    code.push((missing.len() + banner.len()) as u8);
    let missing_addr = code_base + 2;
    let banner_addr = missing_addr + missing.len() as u64;
    code.extend_from_slice(missing);
    code.extend_from_slice(banner);

    // r15 = scratch. r12 = good cap, once open returns it.
    mov_r64_imm(&mut code, 15, scratch);

    // open("no-such")
    syscall_imm(
        &mut code,
        Syscall::Open as u64,
        missing_addr,
        missing.len() as u64,
    );
    store(&mut code, 2, 0x08); // rdx -> missing_ok
    store(&mut code, 0, 0x10); // rax -> missing_err

    // open("banner.txt"); keep the cap in r12.
    syscall_imm(
        &mut code,
        Syscall::Open as u64,
        banner_addr,
        banner.len() as u64,
    );
    store(&mut code, 2, 0x18);
    store(&mut code, 0, 0x20);
    code.extend_from_slice(&[0x49, 0x89, 0xC4]); // mov r12, rax

    // read(cap, scratch+0x30, 32)
    mov_eax(&mut code, Syscall::Read as u32);
    code.extend_from_slice(&[0x4C, 0x89, 0xE7]); // mov rdi, r12
    code.extend_from_slice(&[0x4C, 0x89, 0xFE]); // mov rsi, r15
    code.extend_from_slice(&[0x48, 0x83, 0xC6, 0x30]); // add rsi, 0x30
    mov_edx(&mut code, 32);
    code.extend_from_slice(&[0x0F, 0x05]);
    store(&mut code, 0, 0x28); // rax -> read_n

    // WRITE-only forgery of the same index: strip rights, set WRITE.
    code.extend_from_slice(&[0x4C, 0x89, 0xE7]); // mov rdi, r12
    mov_r64_imm(&mut code, 13, 0x0000_FFFF_FFFF_FFFF);
    code.extend_from_slice(&[0x4C, 0x21, 0xEF]); // and rdi, r13
    mov_r64_imm(&mut code, 13, (CapRights::WRITE.bits() as u64) << 48);
    code.extend_from_slice(&[0x4C, 0x09, 0xEF]); // or rdi, r13
    mov_eax(&mut code, Syscall::Read as u32);
    code.extend_from_slice(&[0x4C, 0x89, 0xFE]); // mov rsi, r15
    code.extend_from_slice(&[0x48, 0x83, 0xC6, 0x30]);
    mov_edx(&mut code, 4);
    code.extend_from_slice(&[0x0F, 0x05]);
    store(&mut code, 2, 0x50); // denied_ok
    store(&mut code, 0, 0x58); // denied_err

    // read again: EOF
    mov_eax(&mut code, Syscall::Read as u32);
    code.extend_from_slice(&[0x4C, 0x89, 0xE7]); // mov rdi, r12
    code.extend_from_slice(&[0x4C, 0x89, 0xFE]);
    code.extend_from_slice(&[0x48, 0x83, 0xC6, 0x30]);
    mov_edx(&mut code, 32);
    code.extend_from_slice(&[0x0F, 0x05]);
    store(&mut code, 0, 0x60); // eof_n
    store(&mut code, 2, 0x68); // eof_ok

    // close
    mov_eax(&mut code, Syscall::Close as u32);
    code.extend_from_slice(&[0x4C, 0x89, 0xE7]);
    code.extend_from_slice(&[0x0F, 0x05]);
    store(&mut code, 2, 0x70);

    // read after close
    mov_eax(&mut code, Syscall::Read as u32);
    code.extend_from_slice(&[0x4C, 0x89, 0xE7]);
    code.extend_from_slice(&[0x4C, 0x89, 0xFE]);
    code.extend_from_slice(&[0x48, 0x83, 0xC6, 0x30]);
    mov_edx(&mut code, 4);
    code.extend_from_slice(&[0x0F, 0x05]);
    store(&mut code, 2, 0x78); // closed_ok
    store(&mut code, 0, 0x80); // closed_err

    // done marker last, then exit.
    mov_r64_imm(&mut code, 0, DONE); // mov rax, DONE
    store(&mut code, 0, 0x00);
    mov_eax(&mut code, Syscall::Exit as u32);
    code.extend_from_slice(&[0x31, 0xFF]); // xor edi, edi
    code.extend_from_slice(&[0x0F, 0x05]);
    code
}

/// `mov r{reg}, imm64`. `reg` is the full register number (0 = rax … 15 = r15).
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

fn mov_edx(code: &mut alloc::vec::Vec<u8>, imm: u32) {
    code.push(0xBA);
    code.extend_from_slice(&imm.to_le_bytes());
}

/// `syscall` with `rdi = a0`, `rsi = a1`. Number is `eax`.
fn syscall_imm(code: &mut alloc::vec::Vec<u8>, number: u64, a0: u64, a1: u64) {
    mov_eax(code, number as u32);
    mov_r64_imm(code, 7, a0); // rdi
    mov_r64_imm(code, 6, a1); // rsi
    code.extend_from_slice(&[0x0F, 0x05]);
}

/// `mov [r15+disp], reg` where reg 0 is rax and reg 2 is rdx.
fn store(code: &mut alloc::vec::Vec<u8>, reg: u8, disp: i32) {
    let modrm = 0x80 | (reg << 3) | 7;
    code.extend_from_slice(&[0x49, 0x89, modrm]);
    code.extend_from_slice(&disp.to_le_bytes());
}
