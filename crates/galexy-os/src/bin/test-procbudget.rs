//! Integration test: `MAX_PROC_CAPS` ceiling.
//!
//! Spawns `hello` without Cap-wait until eight process Caps are held, then
//! proves the next spawn is `NoResource` once the name is free again.

#![no_std]
#![no_main]

extern crate alloc;

use bootloader_api::{entry_point, BootInfo};
use galexy_abi::{reserved, CapRights, SysError, Syscall, MAX_PROC_CAPS};
use galexy_os::{
    arch::mm, drivers::screen, exit_qemu, println, sched, serial_println, QemuExitCode,
};

entry_point!(test_main_entry, config = &galexy_os::BOOTLOADER_CONFIG);

const DONE: u64 = 0xB0D6_E701;
const TICK_TIMEOUT: u64 = 30_000;

#[repr(C)]
struct Report {
    done: u64,
    filled: u64,
    ninth_ok: u64,
    ninth_err: u64,
}

fn test_main_entry(boot_info: &'static mut BootInfo) -> ! {
    galexy_os::init();
    screen::init(boot_info);
    println!("[test-procbudget] running");
    serial_println!("[test-procbudget] running");

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
    assert!(sched::ramdisk::find("hello").is_some());

    let (region, _) = sched::spawn_user_launcher("budget", |gr| {
        unsafe {
            core::ptr::write_bytes(mm::frame_virt(gr.scratch_phys).as_mut_ptr::<u8>(), 0, 4096);
        }
        build_blob(gr.code.as_u64(), gr.scratch.as_u64())
    });

    let scratch: *const Report = mm::frame_virt(region.scratch_phys).as_ptr();
    let mut elapsed = 0u64;
    let report = loop {
        x86_64::instructions::hlt();
        sched::drain_spawn();
        sched::reap();
        let done = unsafe { core::ptr::read_volatile(core::ptr::addr_of!((*scratch).done)) };
        if done == DONE {
            break unsafe { core::ptr::read_volatile(scratch) };
        }
        elapsed += 1;
        if elapsed > TICK_TIMEOUT {
            panic!("budget blob never finished");
        }
    };

    assert_eq!(
        report.filled, MAX_PROC_CAPS,
        "must hold MAX_PROC_CAPS process Caps"
    );
    assert_eq!(report.ninth_ok, 0, "ninth spawn must fail");
    assert_eq!(
        report.ninth_err,
        SysError::NoResource as u64,
        "full process Cap table is NoResource"
    );

    loop {
        x86_64::instructions::hlt();
        sched::drain_spawn();
        sched::reap();
        if sched::unreaped_threads() == 0 {
            break;
        }
    }

    println!("[test-procbudget] MAX_PROC_CAPS ceiling hit");
    serial_println!("[test-procbudget] passed");
    exit_qemu(QemuExitCode::Success);
}

fn build_blob(code_base: u64, scratch: u64) -> alloc::vec::Vec<u8> {
    let hello = b"hello";
    let mut code = alloc::vec::Vec::new();
    code.push(0xEB);
    code.push(hello.len() as u8);
    let hello_addr = code_base + 2;
    code.extend_from_slice(hello);

    mov_r64_imm(&mut code, 15, scratch); // r15 = scratch
    mov_r64_imm(&mut code, 13, 0); // r13 = filled count
    let loader = reserved::loader(CapRights::EXEC).bits();
    let max = MAX_PROC_CAPS;

    // fill_loop: spawn until r13 == max
    let fill_loop = code.len();
    // if r13 >= max → ninth
    code.extend_from_slice(&[0x49, 0x83, 0xFD]); // cmp r13, imm8
    code.push(max as u8);
    let jge = code.len();
    code.extend_from_slice(&[0x0F, 0x8D, 0, 0, 0, 0]); // jge ninth

    // spawn hello
    mov_eax(&mut code, Syscall::Spawn as u32);
    mov_r64_imm(&mut code, 7, loader);
    mov_r64_imm(&mut code, 6, hello_addr);
    mov_r64_imm(&mut code, 2, hello.len() as u64);
    mov_r64_imm(&mut code, 8, 0);
    mov_r64_imm(&mut code, 9, 0);
    mov_r64_imm(&mut code, 10, 0);
    code.extend_from_slice(&[0x0F, 0x05]);
    // if ok: inc r13, loop
    code.extend_from_slice(&[0x48, 0x85, 0xD2]); // test rdx, rdx
    let jz_busy = code.len();
    code.extend_from_slice(&[0x0F, 0x84, 0, 0, 0, 0]); // jz yield_busy
    code.extend_from_slice(&[0x49, 0xFF, 0xC5]); // inc r13
    let jmp_fill = code.len();
    code.push(0xE9);
    code.extend_from_slice(&[0, 0, 0, 0]);

    // yield_busy: name still live — yield and retry
    let yield_busy = code.len();
    let rel_busy = yield_busy as i32 - (jz_busy as i32 + 6);
    code[jz_busy + 2..jz_busy + 6].copy_from_slice(&rel_busy.to_le_bytes());
    mov_eax(&mut code, Syscall::Yield as u32);
    code.extend_from_slice(&[0x0F, 0x05]);
    let after_yb = code.len() + 5;
    let rel_yb = fill_loop as i32 - after_yb as i32;
    code.push(0xE9);
    code.extend_from_slice(&rel_yb.to_le_bytes());

    let after_jmp_fill = code.len();
    // patch jmp_fill → fill_loop (we're at after_jmp_fill which is yield_busy end... 
    // Actually jmp_fill should go to fill_loop. Patch now with current len wrong.
    // Re-patch after we know fill_loop target: already have fill_loop.
    let rel_fill = fill_loop as i32 - (jmp_fill as i32 + 5);
    code[jmp_fill + 1..jmp_fill + 5].copy_from_slice(&rel_fill.to_le_bytes());
    let _ = after_jmp_fill;

    // ninth: store filled, drain yields, spawn once more
    let ninth = code.len();
    let rel_jge = ninth as i32 - (jge as i32 + 6);
    code[jge + 2..jge + 6].copy_from_slice(&rel_jge.to_le_bytes());

    // store filled (r13) at scratch+0x08
    code.extend_from_slice(&[0x4C, 0x89, 0xE8]); // mov rax, r13
    store(&mut code, 0, 0x08);

    // yield a bunch so the last hello can exit (name free)
    mov_r64_imm(&mut code, 12, 256); // r12 = yield count
    let drain = code.len();
    mov_eax(&mut code, Syscall::Yield as u32);
    code.extend_from_slice(&[0x0F, 0x05]);
    code.extend_from_slice(&[0x49, 0xFF, 0xCC]); // dec r12
    code.extend_from_slice(&[0x49, 0x83, 0xFC, 0x00]); // cmp r12, 0
    let jg = code.len();
    code.extend_from_slice(&[0x0F, 0x8F, 0, 0, 0, 0]); // jg drain
    let rel_drain = drain as i32 - (jg as i32 + 6);
    code[jg + 2..jg + 6].copy_from_slice(&rel_drain.to_le_bytes());

    // spawn again → expect NoResource
    mov_eax(&mut code, Syscall::Spawn as u32);
    mov_r64_imm(&mut code, 7, loader);
    mov_r64_imm(&mut code, 6, hello_addr);
    mov_r64_imm(&mut code, 2, hello.len() as u64);
    mov_r64_imm(&mut code, 8, 0);
    mov_r64_imm(&mut code, 9, 0);
    mov_r64_imm(&mut code, 10, 0);
    code.extend_from_slice(&[0x0F, 0x05]);
    store(&mut code, 2, 0x10); // ninth_ok
    store(&mut code, 0, 0x18); // ninth_err

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
