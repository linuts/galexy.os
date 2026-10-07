//! Integration test: anonymous pipes and `give` across tasks.
//! Milestone 57: empty-pipe `read` parks until the producer writes
//! (no yield spin once the read Cap is installed).

#![no_std]
#![no_main]

extern crate alloc;

use bootloader_api::{entry_point, BootInfo};
use galexy_abi::{Cap, CapRights, Syscall, FILE_CAP_BASE};
use galexy_os::{
    arch::mm, drivers::screen, exit_qemu, println, sched, serial_println, QemuExitCode,
};

entry_point!(test_main_entry, config = &galexy_os::BOOTLOADER_CONFIG);

const DONE: u64 = 0x5C4A_7C21;
const TICK_TIMEOUT: u64 = 8000;
/// Writable scratch layout: report, then pipe caps out, then read buf.
const CAPS_OFF: u64 = 0x40;
const BUF_OFF: u64 = 0x50;

#[repr(C)]
struct ProducerReport {
    done: u64,
    pipe_ok: u64,
    give_ok: u64,
    write_n: u64,
}

#[repr(C)]
struct ConsumerReport {
    done: u64,
    read_n: u64,
    byte0: u64,
}

fn test_main_entry(boot_info: &'static mut BootInfo) -> ! {
    galexy_os::init();
    screen::init(boot_info);
    println!("[test-pipe] running");
    serial_println!("[test-pipe] running");

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

    let (consumer_region, _) = sched::spawn_user_task("consumer", |gr| {
        unsafe {
            core::ptr::write_bytes(mm::frame_virt(gr.scratch_phys).as_mut_ptr::<u8>(), 0, 4096);
        }
        build_consumer(gr.code.as_u64(), gr.scratch.as_u64())
    });

    let (producer_region, _) = sched::spawn_user_task("producer", |gr| {
        unsafe {
            core::ptr::write_bytes(mm::frame_virt(gr.scratch_phys).as_mut_ptr::<u8>(), 0, 4096);
        }
        build_producer(gr.code.as_u64(), gr.scratch.as_u64())
    });

    let prod: *const ProducerReport = mm::frame_virt(producer_region.scratch_phys).as_ptr();
    let mut elapsed = 0u64;
    let pref = loop {
        x86_64::instructions::hlt();
        let done = unsafe { core::ptr::read_volatile(core::ptr::addr_of!((*prod).done)) };
        if done == DONE {
            break unsafe { core::ptr::read_volatile(prod) };
        }
        sched::reap();
        elapsed += 1;
        if elapsed > TICK_TIMEOUT {
            panic!("producer never finished");
        }
    };
    assert_eq!(pref.pipe_ok, 1, "pipe must succeed");
    assert_eq!(pref.give_ok, 1, "give must succeed");
    assert_eq!(pref.write_n, 4, "wrote 4 bytes");

    let cons: *const ConsumerReport = mm::frame_virt(consumer_region.scratch_phys).as_ptr();
    elapsed = 0;
    let cref = loop {
        x86_64::instructions::hlt();
        let done = unsafe { core::ptr::read_volatile(core::ptr::addr_of!((*cons).done)) };
        if done == DONE {
            break unsafe { core::ptr::read_volatile(cons) };
        }
        sched::reap();
        elapsed += 1;
        if elapsed > TICK_TIMEOUT {
            panic!("consumer never finished");
        }
    };
    assert_eq!(cref.read_n, 4, "consumer read 4 bytes");
    assert_eq!(cref.byte0, u64::from(b'p'), "first byte is 'p'");

    loop {
        x86_64::instructions::hlt();
        sched::reap();
        if sched::unreaped_threads() == 0 {
            break;
        }
    }

    println!("[test-pipe] pipe+give works");
    serial_println!("[test-pipe] passed");
    exit_qemu(QemuExitCode::Success);
}

fn build_producer(code_base: u64, scratch: u64) -> alloc::vec::Vec<u8> {
    let task = b"consumer";
    let msg = b"ping";
    let mut code = alloc::vec::Vec::new();
    let data_len = task.len() + msg.len();
    code.push(0xEB);
    code.push(data_len as u8);
    let task_addr = code_base + 2;
    code.extend_from_slice(task);
    let msg_addr = task_addr + task.len() as u64;
    code.extend_from_slice(msg);

    let caps_addr = scratch + CAPS_OFF;
    mov_r64_imm(&mut code, 15, scratch);

    mov_eax(&mut code, Syscall::Pipe as u32);
    mov_r64_imm(&mut code, 7, caps_addr);
    code.extend_from_slice(&[0x0F, 0x05]);
    store(&mut code, 2, 0x08);

    mov_r64_imm(&mut code, 8, caps_addr);
    code.extend_from_slice(&[0x49, 0x8B, 0x00]);
    code.extend_from_slice(&[0x48, 0x89, 0xC7]);
    mov_eax(&mut code, Syscall::Give as u32);
    mov_r64_imm(&mut code, 6, task_addr);
    mov_r64_imm(&mut code, 2, task.len() as u64);
    code.extend_from_slice(&[0x0F, 0x05]);
    store(&mut code, 2, 0x10);

    mov_r64_imm(&mut code, 8, caps_addr + 8);
    code.extend_from_slice(&[0x49, 0x8B, 0x00]);
    code.extend_from_slice(&[0x48, 0x89, 0xC7]);
    mov_eax(&mut code, Syscall::Write as u32);
    mov_r64_imm(&mut code, 6, msg_addr);
    mov_r64_imm(&mut code, 2, msg.len() as u64);
    code.extend_from_slice(&[0x0F, 0x05]);
    store(&mut code, 0, 0x18);

    mov_r64_imm(&mut code, 8, caps_addr + 8);
    code.extend_from_slice(&[0x49, 0x8B, 0x00]);
    code.extend_from_slice(&[0x48, 0x89, 0xC7]);
    mov_eax(&mut code, Syscall::Close as u32);
    code.extend_from_slice(&[0x0F, 0x05]);

    mov_r64_imm(&mut code, 0, DONE);
    store(&mut code, 0, 0x00);
    mov_eax(&mut code, Syscall::Exit as u32);
    code.extend_from_slice(&[0x31, 0xFF, 0x0F, 0x05]);
    code
}

fn build_consumer(_code_base: u64, scratch: u64) -> alloc::vec::Vec<u8> {
    let mut code = alloc::vec::Vec::new();
    // No embedded data; jump over 0 bytes still needs a target.
    code.push(0xEB);
    code.push(0);

    let buf_addr = scratch + BUF_OFF;
    mov_r64_imm(&mut code, 15, scratch);

    let read_cap = Cap::new(FILE_CAP_BASE, CapRights::READ);

    let loop_at = code.len();
    mov_eax(&mut code, Syscall::Read as u32);
    mov_r64_imm(&mut code, 7, read_cap.bits());
    mov_r64_imm(&mut code, 6, buf_addr);
    mov_r64_imm(&mut code, 2, 16);
    code.extend_from_slice(&[0x0F, 0x05]);
    code.extend_from_slice(&[0x48, 0x85, 0xD2]);
    let jz_bad = code.len();
    code.extend_from_slice(&[0x0F, 0x84, 0, 0, 0, 0]);
    code.extend_from_slice(&[0x48, 0x85, 0xC0]);
    let jnz = code.len();
    code.extend_from_slice(&[0x0F, 0x85, 0, 0, 0, 0]);
    let yield_at = code.len();
    let yrel = yield_at as i32 - (jz_bad as i32 + 6);
    code[jz_bad + 2..jz_bad + 6].copy_from_slice(&yrel.to_le_bytes());
    mov_eax(&mut code, Syscall::Yield as u32);
    code.extend_from_slice(&[0x0F, 0x05]);
    let after = code.len() + 5;
    let rel = loop_at as i32 - after as i32;
    code.push(0xE9);
    code.extend_from_slice(&rel.to_le_bytes());

    let got = code.len();
    let jrel = got as i32 - (jnz as i32 + 6);
    code[jnz + 2..jnz + 6].copy_from_slice(&jrel.to_le_bytes());

    store(&mut code, 0, 0x08);
    mov_r64_imm(&mut code, 8, buf_addr);
    code.extend_from_slice(&[0x41, 0x0F, 0xB6, 0x00]);
    store(&mut code, 0, 0x10);

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
