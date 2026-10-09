//! User heap: 32 `Map` pages, then `NoResource`, then reap returns the frames.
//! Two `Clock` reads are monotonic.

#![no_std]
#![no_main]

extern crate alloc;

use bootloader_api::{entry_point, BootInfo};
use galexy_abi::Syscall;
use galexy_os::{
    arch::mm, drivers::screen, exit_qemu, println, sched, serial_println, QemuExitCode,
};

entry_point!(test_main_entry, config = &galexy_os::BOOTLOADER_CONFIG);

const DONE: u64 = 0x4816_6601;
const TICK_TIMEOUT: u64 = 8000;

#[repr(C)]
#[derive(Clone, Copy)]
struct Report {
    done: u64,
    maps: u64,
    byte: u64,
    extra_rax: u64,
    extra_rdx: u64,
    clock0: u64,
    clock1: u64,
}

fn test_main_entry(boot_info: &'static mut BootInfo) -> ! {
    galexy_os::init();
    screen::init(boot_info);
    println!("[test-userheap] running");
    serial_println!("[test-userheap] running");

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

    let before = mm::free_frames();
    let (region, _) = sched::spawn_user_task("heap", |gr| {
        unsafe {
            core::ptr::write_bytes(mm::frame_virt(gr.scratch_phys).as_mut_ptr::<u8>(), 0, 4096);
        }
        build(gr.scratch.as_u64())
    });
    let report: *const Report = mm::frame_virt(region.scratch_phys).as_ptr();
    x86_64::instructions::interrupts::enable();
    let mut elapsed = 0u64;
    let got = loop {
        let done = unsafe { core::ptr::read_volatile(core::ptr::addr_of!((*report).done)) };
        if done == DONE {
            break unsafe { core::ptr::read_volatile(report) };
        }
        core::hint::spin_loop();
        elapsed += 1;
        if elapsed > TICK_TIMEOUT.saturating_mul(10_000) {
            panic!("heap task never finished");
        }
    };
    serial_println!(
        "[test-userheap] maps={} byte={} extra_rax={} extra_rdx={} c0={} c1={}",
        got.maps,
        got.byte,
        got.extra_rax,
        got.extra_rdx,
        got.clock0,
        got.clock1
    );
    assert_eq!(got.maps, 32, "32 one-page maps must succeed");
    assert_eq!(got.byte, 0xA5, "mapped page must store and load 0xA5");
    assert_eq!(got.extra_rdx, 0, "33rd map must fail");
    assert_eq!(got.extra_rax, 7, "33rd map is NoResource");
    assert!(got.clock1 >= got.clock0, "clock must be monotonic");

    loop {
        x86_64::instructions::hlt();
        sched::reap();
        if sched::unreaped_threads() == 0 {
            break;
        }
    }
    let after = mm::free_frames();
    serial_println!("[test-userheap] frames before={} after={}", before, after);
    assert_eq!(after, before, "reap must return every heap frame");

    println!("[test-userheap] map budget and clock");
    serial_println!("[test-userheap] passed");
    exit_qemu(QemuExitCode::Success);
}

fn build(scratch: u64) -> alloc::vec::Vec<u8> {
    let mut code = alloc::vec::Vec::new();
    code.push(0xEB);
    code.push(0);
    mov_r64_imm(&mut code, 15, scratch);
    // xor r12, r12 / xor r13, r13 — success count and first base.
    code.extend_from_slice(&[0x4D, 0x31, 0xE4, 0x4D, 0x31, 0xED]);

    let loop_at = code.len();
    mov_eax(&mut code, Syscall::Map as u32);
    code.extend_from_slice(&[0xBF, 0x01, 0x00, 0x00, 0x00]); // mov edi, 1
    code.extend_from_slice(&[0x0F, 0x05]);
    code.extend_from_slice(&[0x48, 0x85, 0xD2]); // test rdx, rdx
    let jz_fail = code.len();
    code.extend_from_slice(&[0x0F, 0x84, 0, 0, 0, 0]);
    code.extend_from_slice(&[0x4D, 0x85, 0xED]); // test r13, r13
    let jnz_inc = code.len();
    code.extend_from_slice(&[0x75, 0]);
    code.extend_from_slice(&[0x49, 0x89, 0xC5]); // mov r13, rax
    code.extend_from_slice(&[0x41, 0xC6, 0x45, 0x00, 0xA5]); // mov byte [r13], 0xA5
    let inc_at = code.len();
    code[jnz_inc + 1] = (inc_at - (jnz_inc + 2)) as u8;
    code.extend_from_slice(&[0x49, 0xFF, 0xC4]); // inc r12
    code.extend_from_slice(&[0x49, 0x83, 0xFC, 0x20]); // cmp r12, 32
    let jb_at = code.len();
    code.extend_from_slice(&[0x0F, 0x82, 0, 0, 0, 0]);
    let after_jb = code.len();
    let back = loop_at as i32 - after_jb as i32;
    code[jb_at + 2..jb_at + 6].copy_from_slice(&back.to_le_bytes());

    // Success: the stored byte, the count, then one map past the budget.
    code.extend_from_slice(&[0x41, 0x0F, 0xB6, 0x45, 0x00]); // movzx eax, byte [r13]
    store(&mut code, 0, 0x10);
    code.extend_from_slice(&[0x4C, 0x89, 0xE0]); // mov rax, r12
    store(&mut code, 0, 0x08);
    mov_eax(&mut code, Syscall::Map as u32);
    code.extend_from_slice(&[0xBF, 0x01, 0x00, 0x00, 0x00, 0x0F, 0x05]);
    store(&mut code, 0, 0x18);
    code.extend_from_slice(&[0x48, 0x89, 0xD0]); // mov rax, rdx
    store(&mut code, 0, 0x20);
    let jmp_clock = code.len();
    code.extend_from_slice(&[0xE9, 0, 0, 0, 0]);

    let fail_at = code.len();
    let fail_rel = fail_at as i32 - (jz_fail as i32 + 6);
    code[jz_fail + 2..jz_fail + 6].copy_from_slice(&fail_rel.to_le_bytes());
    code.extend_from_slice(&[0x4C, 0x89, 0xE0]); // mov rax, r12
    store(&mut code, 0, 0x08);
    code.extend_from_slice(&[0x31, 0xC0]); // xor eax, eax
    store(&mut code, 0, 0x10);

    let clock_at = code.len();
    let clock_rel = clock_at as i32 - (jmp_clock as i32 + 5);
    code[jmp_clock + 1..jmp_clock + 5].copy_from_slice(&clock_rel.to_le_bytes());
    mov_eax(&mut code, Syscall::Clock as u32);
    code.extend_from_slice(&[0x0F, 0x05]);
    store(&mut code, 0, 0x28);
    mov_eax(&mut code, Syscall::Clock as u32);
    code.extend_from_slice(&[0x0F, 0x05]);
    store(&mut code, 0, 0x30);

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
