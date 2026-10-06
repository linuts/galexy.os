//! Integration test: `seek` on a galfs file.

#![no_std]
#![no_main]

extern crate alloc;

use bootloader_api::{entry_point, BootInfo};
use galexy_abi::{Syscall, SEEK_SET};
use galexy_os::{
    arch::mm, drivers::screen, exit_qemu, println, sched, serial_println, QemuExitCode,
};

entry_point!(test_main_entry, config = &galexy_os::BOOTLOADER_CONFIG);

const DONE: u64 = 0x5C4A_7C22;
const TICK_TIMEOUT: u64 = 4000;
const PAYLOAD: &[u8] = b"abcdefgh";

#[repr(C)]
struct Report {
    done: u64,
    create_ok: u64,
    write_n: u64,
    seek_ok: u64,
    seek_pos: u64,
    read_n: u64,
    byte0: u64,
}

fn test_main_entry(boot_info: &'static mut BootInfo) -> ! {
    galexy_os::init();
    screen::init(boot_info);
    println!("[test-seek] running");
    serial_println!("[test-seek] running");

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

    let (region, _) = sched::spawn_user_task("seeker", |gr| {
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
            panic!("seeker never finished");
        }
    };

    assert_eq!(report.create_ok, 1);
    assert_eq!(report.write_n, PAYLOAD.len() as u64);
    assert_eq!(report.seek_ok, 1);
    assert_eq!(report.seek_pos, 3);
    assert_eq!(report.read_n, 1);
    assert_eq!(report.byte0, u64::from(b'd'));

    loop {
        x86_64::instructions::hlt();
        sched::reap();
        if sched::unreaped_threads() == 0 {
            break;
        }
    }

    println!("[test-seek] seek lands on byte");
    serial_println!("[test-seek] passed");
    exit_qemu(QemuExitCode::Success);
}

fn build_blob(code_base: u64, scratch: u64) -> alloc::vec::Vec<u8> {
    let name = b"seekme";
    let mut code = alloc::vec::Vec::new();
    let data_len = name.len() + PAYLOAD.len();
    code.push(0xEB);
    code.push(data_len as u8);
    let name_addr = code_base + 2;
    code.extend_from_slice(name);
    let pay_addr = name_addr + name.len() as u64;
    code.extend_from_slice(PAYLOAD);
    let buf_addr = scratch + 0x40;

    mov_r64_imm(&mut code, 15, scratch);

    // create
    mov_eax(&mut code, Syscall::Create as u32);
    mov_r64_imm(&mut code, 7, name_addr);
    mov_r64_imm(&mut code, 6, name.len() as u64);
    mov_r64_imm(&mut code, 2, 0);
    code.extend_from_slice(&[0x0F, 0x05]);
    store(&mut code, 2, 0x08); // create_ok
                               // save cap in r14
    code.extend_from_slice(&[0x49, 0x89, 0xC6]); // mov r14, rax

    // write
    code.extend_from_slice(&[0x4C, 0x89, 0xF7]); // mov rdi, r14
    mov_eax(&mut code, Syscall::Write as u32);
    mov_r64_imm(&mut code, 6, pay_addr);
    mov_r64_imm(&mut code, 2, PAYLOAD.len() as u64);
    code.extend_from_slice(&[0x0F, 0x05]);
    store(&mut code, 0, 0x10); // write_n

    // seek(cap, 3, SEEK_SET)
    code.extend_from_slice(&[0x4C, 0x89, 0xF7]);
    mov_eax(&mut code, Syscall::Seek as u32);
    mov_r64_imm(&mut code, 6, 3);
    mov_r64_imm(&mut code, 2, SEEK_SET);
    code.extend_from_slice(&[0x0F, 0x05]);
    store(&mut code, 2, 0x18); // seek_ok
    store(&mut code, 0, 0x20); // seek_pos

    // read 1 byte into scratch
    code.extend_from_slice(&[0x4C, 0x89, 0xF7]);
    mov_eax(&mut code, Syscall::Read as u32);
    mov_r64_imm(&mut code, 6, buf_addr);
    mov_r64_imm(&mut code, 2, 1);
    code.extend_from_slice(&[0x0F, 0x05]);
    store(&mut code, 0, 0x28); // read_n
    mov_r64_imm(&mut code, 8, buf_addr);
    code.extend_from_slice(&[0x41, 0x0F, 0xB6, 0x00]);
    store(&mut code, 0, 0x30); // byte0

    mov_r64_imm(&mut code, 0, DONE);
    store(&mut code, 0, 0x00);
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
