//! Integration test kernel: removing galfs files.
//!
//! A ring-3 blob creates a file, removes it, and checks the old cap and a
//! fresh open both fail. A tar name cannot be removed. A directory with a
//! child cannot be removed; once the child is gone, the directory can.
//! The freed table then fills again, and one more create does not.

#![no_std]
#![no_main]

extern crate alloc;

use bootloader_api::{entry_point, BootInfo};
use galexy_abi::{SysError, Syscall};
use galexy_os::{
    arch::mm, drivers::screen, exit_qemu, println, sched, serial_println, QemuExitCode,
};

entry_point!(test_main_entry, config = &galexy_os::BOOTLOADER_CONFIG);

const DONE: u64 = 0x5C4A_7C10;
const TICK_TIMEOUT: u64 = 4000;

#[repr(C)]
struct Report {
    done: u64,
    create_ok: u64,
    rm_note_ok: u64,
    read_ok: u64,
    read_err: u64,
    tar_ok: u64,
    tar_err: u64,
    miss_ok: u64,
    miss_err: u64,
    busy_ok: u64,
    busy_err: u64,
    empty_ok: u64,
    fill_ok: u64,
    extra_ok: u64,
    extra_err: u64,
}

fn test_main_entry(boot_info: &'static mut BootInfo) -> ! {
    galexy_os::init();
    screen::init(boot_info);
    println!("[test-rm] running");
    serial_println!("[test-rm] running");

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

    let (region, _) = sched::spawn_user_task("remover", |gr| {
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
            panic!("remover never finished the remove report");
        }
    };

    assert_eq!(report.create_ok, 1, "create must succeed");
    assert_eq!(
        report.rm_note_ok, 1,
        "remove of a scratch file must succeed"
    );
    assert_eq!(report.read_ok, 0, "a removed file's cap must fail");
    assert_eq!(
        report.read_err,
        SysError::BadCap as u64,
        "a removed file's cap is BadCap"
    );
    assert_eq!(report.tar_ok, 0, "remove of banner.txt must fail");
    assert_eq!(
        report.tar_err,
        SysError::Unsupported as u64,
        "a tar name is Unsupported"
    );
    assert_eq!(report.miss_ok, 0, "remove of a missing name must fail");
    assert_eq!(
        report.miss_err,
        SysError::NotFound as u64,
        "a missing path is NotFound"
    );
    assert_eq!(report.busy_ok, 0, "a directory with a child must stay");
    assert_eq!(
        report.busy_err,
        SysError::Unsupported as u64,
        "a non-empty directory is Unsupported"
    );
    assert_eq!(report.empty_ok, 1, "an empty directory must be removed");
    // Boot keeps admin root + Desktop — 126 free of 128 (GALF v7).
    assert_eq!(
        report.fill_ok, 126,
        "one hundred twenty-six files must fit after the frees"
    );
    assert_eq!(report.extra_ok, 0, "a full galfs table must fail");
    assert_eq!(
        report.extra_err,
        SysError::NoResource as u64,
        "a full galfs table is NoResource"
    );

    loop {
        x86_64::instructions::hlt();
        sched::reap();
        if sched::unreaped_threads() == 0 {
            break;
        }
    }

    println!("[test-rm] removed a file and an empty directory");
    serial_println!("[test-rm] passed");
    exit_qemu(QemuExitCode::Success);
}

fn build_blob(code_base: u64, scratch: u64) -> alloc::vec::Vec<u8> {
    let note = b"note";
    let banner = b"banner.txt";
    let missing = b"missing";
    let dir = b"box/";
    let leaf = b"box/leaf";
    let dir_name = b"box";
    let extra = b"extra";
    let mut code: alloc::vec::Vec<u8> = alloc::vec::Vec::new();
    let data_len = note.len()
        + banner.len()
        + missing.len()
        + dir.len()
        + leaf.len()
        + dir_name.len()
        + extra.len();
    code.push(0xEB);
    code.push(data_len as u8);
    let note_addr = code_base + 2;
    let banner_addr = note_addr + note.len() as u64;
    let missing_addr = banner_addr + banner.len() as u64;
    let dir_addr = missing_addr + missing.len() as u64;
    let leaf_addr = dir_addr + dir.len() as u64;
    let dir_name_addr = leaf_addr + leaf.len() as u64;
    let extra_addr = dir_name_addr + dir_name.len() as u64;
    code.extend_from_slice(note);
    code.extend_from_slice(banner);
    code.extend_from_slice(missing);
    code.extend_from_slice(dir);
    code.extend_from_slice(leaf);
    code.extend_from_slice(dir_name);
    code.extend_from_slice(extra);

    mov_r64_imm(&mut code, 15, scratch);

    syscall_imm(
        &mut code,
        Syscall::Create as u64,
        note_addr,
        note.len() as u64,
    );
    store(&mut code, 2, 0x08);
    code.extend_from_slice(&[0x49, 0x89, 0xC4]); // mov r12, rax

    syscall_imm(
        &mut code,
        Syscall::Remove as u64,
        note_addr,
        note.len() as u64,
    );
    store(&mut code, 2, 0x10);

    // read the cap remove just dropped
    code.extend_from_slice(&[0x4C, 0x89, 0xE7]); // mov rdi, r12
    code.extend_from_slice(&[0x4C, 0x89, 0xFE]); // mov rsi, r15
    code.extend_from_slice(&[0x48, 0x81, 0xC6, 0x00, 0x01, 0x00, 0x00]); // add rsi, 0x100
    mov_edx(&mut code, 8);
    mov_eax(&mut code, Syscall::Read as u32);
    code.extend_from_slice(&[0x0F, 0x05]);
    store(&mut code, 2, 0x18);
    store(&mut code, 0, 0x20);

    code.extend_from_slice(&[0x4C, 0x89, 0xE7]); // mov rdi, r12
    mov_eax(&mut code, Syscall::Close as u32);
    code.extend_from_slice(&[0x0F, 0x05]);

    syscall_imm(
        &mut code,
        Syscall::Remove as u64,
        banner_addr,
        banner.len() as u64,
    );
    store(&mut code, 2, 0x28);
    store(&mut code, 0, 0x30);

    syscall_imm(
        &mut code,
        Syscall::Remove as u64,
        missing_addr,
        missing.len() as u64,
    );
    store(&mut code, 2, 0x38);
    store(&mut code, 0, 0x40);

    syscall_imm(
        &mut code,
        Syscall::Create as u64,
        dir_addr,
        dir.len() as u64,
    );
    syscall_imm(
        &mut code,
        Syscall::Create as u64,
        leaf_addr,
        leaf.len() as u64,
    );
    code.extend_from_slice(&[0x48, 0x89, 0xC7]); // mov rdi, rax
    mov_eax(&mut code, Syscall::Close as u32);
    code.extend_from_slice(&[0x0F, 0x05]);

    syscall_imm(
        &mut code,
        Syscall::Remove as u64,
        dir_name_addr,
        dir_name.len() as u64,
    );
    store(&mut code, 2, 0x48);
    store(&mut code, 0, 0x50);

    syscall_imm(
        &mut code,
        Syscall::Remove as u64,
        leaf_addr,
        leaf.len() as u64,
    );
    syscall_imm(
        &mut code,
        Syscall::Remove as u64,
        dir_name_addr,
        dir_name.len() as u64,
    );
    store(&mut code, 2, 0x58);

    // 126 two-char names (Aa..) — fills 128 slots with admin root + Desktop.
    // Uppercase first letter avoids ramdisk names (`cp` / `ls` / `mv` / `rm`).
    emit_fill_loop(&mut code, 126, 0x60, 0x200);

    syscall_imm(
        &mut code,
        Syscall::Create as u64,
        extra_addr,
        extra.len() as u64,
    );
    store(&mut code, 2, 0x68);
    store(&mut code, 0, 0x70);

    mov_r64_imm(&mut code, 0, DONE);
    store(&mut code, 0, 0x00);
    mov_eax(&mut code, Syscall::Exit as u32);
    code.extend_from_slice(&[0x31, 0xFF]);
    code.extend_from_slice(&[0x0F, 0x05]);
    code
}

/// Create `count` unique two-char names (`Aa`…), counting successes into
/// `[r15+fill_ok_off]`. Names are staged at `[r15+name_disp]`.
fn emit_fill_loop(code: &mut alloc::vec::Vec<u8>, count: u64, fill_ok_off: i32, name_disp: i32) {
    code.extend_from_slice(&[0x4D, 0x31, 0xED]); // xor r13, r13
    let loop_at = code.len();

    code.extend_from_slice(&[0x4C, 0x89, 0xE8]); // mov rax, r13
    code.extend_from_slice(&[0x48, 0x31, 0xD2]); // xor rdx, rdx
    code.extend_from_slice(&[0xB9, 26, 0, 0, 0]); // mov ecx, 26
    code.extend_from_slice(&[0x48, 0xF7, 0xF1]); // div rcx
    code.extend_from_slice(&[0x04, b'A']); // add al, 'A' (avoid ramdisk names)
    code.extend_from_slice(&[0x80, 0xC2, b'a']); // add dl, 'a'
    code.extend_from_slice(&[0x41, 0x88, 0x87]); // mov [r15+disp], al
    code.extend_from_slice(&name_disp.to_le_bytes());
    code.extend_from_slice(&[0x41, 0x88, 0x97]); // mov [r15+disp], dl
    code.extend_from_slice(&(name_disp + 1).to_le_bytes());

    code.extend_from_slice(&[0x49, 0x8D, 0xBF]); // lea rdi, [r15+disp]
    code.extend_from_slice(&name_disp.to_le_bytes());
    mov_r64_imm(code, 6, 2); // rsi = 2
    mov_eax(code, Syscall::Create as u32);
    code.extend_from_slice(&[0x0F, 0x05]);
    code.extend_from_slice(&[0x49, 0x01, 0x97]);
    code.extend_from_slice(&fill_ok_off.to_le_bytes());
    code.extend_from_slice(&[0x48, 0x89, 0xC7]); // mov rdi, rax
    mov_eax(code, Syscall::Close as u32);
    code.extend_from_slice(&[0x0F, 0x05]);

    code.extend_from_slice(&[0x49, 0xFF, 0xC5]); // inc r13
    mov_r64_imm(code, 0, count);
    code.extend_from_slice(&[0x49, 0x39, 0xC5]); // cmp r13, rax
    let after_jb = code.len() + 2;
    let rel = loop_at as i32 - after_jb as i32;
    debug_assert!((-128..128).contains(&rel), "fill loop too large for short jb");
    code.push(0x72); // jb
    code.push(rel as u8);
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
    mov_r64_imm(code, 7, a0);
    mov_r64_imm(code, 6, a1);
    code.extend_from_slice(&[0x0F, 0x05]);
}

/// `mov [r15+disp], reg` where reg 0 is rax and reg 2 is rdx.
fn store(code: &mut alloc::vec::Vec<u8>, reg: u8, disp: i32) {
    let modrm = 0x80 | (reg << 3) | 7;
    code.extend_from_slice(&[0x49, 0x89, modrm]);
    code.extend_from_slice(&disp.to_le_bytes());
}
