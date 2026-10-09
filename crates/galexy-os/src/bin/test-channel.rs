//! Capability channels: one queued message, a moved pipe Cap, and give
//! across tasks. Reap returns every channel and pipe slot.

#![no_std]
#![no_main]

extern crate alloc;

use bootloader_api::{entry_point, BootInfo};
use galexy_abi::{Cap, CapRights, Syscall, FILE_CAP_BASE};
use galexy_os::{
    arch::mm, drivers::screen, exit_qemu, println, sched, serial_println, QemuExitCode,
};

entry_point!(test_main_entry, config = &galexy_os::BOOTLOADER_CONFIG);

const DONE: u64 = 0x66C4_A001;
const TICK_TIMEOUT: u64 = 8000;

const CHAN_CAPS: u64 = 0x80;
const PIPE_CAPS: u64 = 0x90;
const MSG_AB: u64 = 0xA0;
const MSG_C: u64 = 0xA8;
const RECV_BUF: u64 = 0xB0;
const CAPS_OUT: u64 = 0xC0;
const Z_BYTE: u64 = 0xD0;
const Z_OUT: u64 = 0xD8;

#[repr(C)]
#[derive(Clone, Copy)]
struct SelfReport {
    done: u64,
    send1_rax: u64,
    send1_rdx: u64,
    send2_rax: u64,
    send2_rdx: u64,
    recv_rax: u64,
    recv_rdx: u64,
    b0: u64,
    b1: u64,
    cap_rax: u64,
    cap_rdx: u64,
    recv2_rax: u64,
    z_byte: u64,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct ProducerReport {
    done: u64,
    chan_rdx: u64,
    give_rdx: u64,
    send_rax: u64,
    send_rdx: u64,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct ConsumerReport {
    done: u64,
    recv_rdx: u64,
    recv_rax: u64,
    b0: u64,
}

fn test_main_entry(boot_info: &'static mut BootInfo) -> ! {
    galexy_os::init();
    screen::init(boot_info);
    println!("[test-channel] running");
    serial_println!("[test-channel] running");

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

    let (self_region, _) = sched::spawn_user_task("selfchan", |gr| {
        // The loader zeroes scratch after `build` returns. The blob
        // plants `ab`, `c`, and `Z` itself.
        build_self(gr.scratch.as_u64())
    });
    let self_report: *const SelfReport = mm::frame_virt(self_region.scratch_phys).as_ptr();
    x86_64::instructions::interrupts::enable();
    let got = wait_done(
        self_report,
        unsafe { core::ptr::addr_of!((*self_report).done) },
        "self",
    );
    serial_println!(
        "[test-channel] self send2={}/{} recv={}/{} bytes={}{} cap={}/{} z={}",
        got.send2_rdx,
        got.send2_rax,
        got.recv_rdx,
        got.recv_rax,
        got.b0,
        got.b1,
        got.cap_rdx,
        got.cap_rax,
        got.z_byte
    );
    assert_eq!(got.send1_rdx, 1, "first send must succeed");
    assert_eq!(got.send1_rax, 2, "first send queues 2 bytes");
    assert_eq!(got.send2_rdx, 0, "second send must fail");
    assert_eq!(got.send2_rax, 7, "second send is NoResource");
    assert_eq!(got.recv_rdx, 1, "recv must succeed");
    assert_eq!(got.recv_rax, 2, "recv returns 2 bytes");
    assert_eq!(got.b0, u64::from(b'a'), "first payload byte");
    assert_eq!(got.b1, u64::from(b'b'), "second payload byte");
    assert_eq!(got.cap_rdx, 1, "send of a pipe cap must succeed");
    assert_eq!(got.recv2_rax, 1, "second recv returns the tag byte");
    assert_eq!(got.z_byte, u64::from(b'Z'), "moved pipe cap must read Z");

    reap_all();
    assert_eq!(sched::channel::in_use(), 0, "self channel must be gone");
    assert_eq!(sched::pipe::in_use(), 0, "self pipe must be gone");

    let (consumer_region, _) = sched::spawn_user_task("receiver", |gr| {
        unsafe {
            core::ptr::write_bytes(mm::frame_virt(gr.scratch_phys).as_mut_ptr::<u8>(), 0, 4096);
        }
        build_consumer(gr.scratch.as_u64())
    });
    let (producer_region, _) = sched::spawn_user_task("sender", |gr| {
        unsafe {
            core::ptr::write_bytes(mm::frame_virt(gr.scratch_phys).as_mut_ptr::<u8>(), 0, 4096);
        }
        build_producer(gr.code.as_u64(), gr.scratch.as_u64())
    });
    let prod: *const ProducerReport = mm::frame_virt(producer_region.scratch_phys).as_ptr();
    let cons: *const ConsumerReport = mm::frame_virt(consumer_region.scratch_phys).as_ptr();
    let pref = wait_done(
        prod,
        unsafe { core::ptr::addr_of!((*prod).done) },
        "producer",
    );
    assert_eq!(pref.chan_rdx, 1, "channel create must succeed");
    assert_eq!(pref.give_rdx, 1, "give of an endpoint must succeed");
    assert_eq!(pref.send_rdx, 1, "cross-task send must succeed");
    assert_eq!(pref.send_rax, 4, "sent ping");
    let cref = wait_done(
        cons,
        unsafe { core::ptr::addr_of!((*cons).done) },
        "consumer",
    );
    assert_eq!(cref.recv_rdx, 1, "consumer recv must succeed");
    assert_eq!(cref.recv_rax, 4, "consumer got 4 bytes");
    assert_eq!(cref.b0, u64::from(b'p'), "payload starts with p");

    reap_all();
    assert_eq!(
        sched::channel::in_use(),
        0,
        "channel table empty after reap"
    );
    assert_eq!(sched::pipe::in_use(), 0, "pipe table empty after reap");

    println!("[test-channel] send/recv and give");
    serial_println!("[test-channel] passed");
    exit_qemu(QemuExitCode::Success);
}

fn wait_done<T: Copy>(ptr: *const T, done: *const u64, who: &str) -> T {
    let mut elapsed = 0u64;
    loop {
        let flag = unsafe { core::ptr::read_volatile(done) };
        if flag == DONE {
            return unsafe { core::ptr::read_volatile(ptr) };
        }
        core::hint::spin_loop();
        elapsed += 1;
        if elapsed > TICK_TIMEOUT.saturating_mul(20_000) {
            panic!("channel task never finished: {}", who);
        }
    }
}

fn reap_all() {
    loop {
        x86_64::instructions::hlt();
        sched::reap();
        if sched::unreaped_threads() == 0 {
            break;
        }
    }
}

fn build_self(scratch: u64) -> alloc::vec::Vec<u8> {
    let mut code = alloc::vec::Vec::new();
    code.push(0xEB);
    code.push(0);
    mov_r64_imm(&mut code, 15, scratch);
    // Scratch is zero when the task starts. Plant the payloads there.
    store_u8(&mut code, MSG_AB as i32, b'a');
    store_u8(&mut code, MSG_AB as i32 + 1, b'b');
    store_u8(&mut code, MSG_C as i32, b'c');
    store_u8(&mut code, Z_BYTE as i32, b'Z');

    // channel
    mov_eax(&mut code, Syscall::Channel as u32);
    mov_r64_imm(&mut code, 7, scratch + CHAN_CAPS);
    code.extend_from_slice(&[0x0F, 0x05]);

    // send "ab" on end 0
    load_cap(&mut code, scratch + CHAN_CAPS);
    mov_eax(&mut code, Syscall::Send as u32);
    mov_r64_imm(&mut code, 6, scratch + MSG_AB);
    code.extend_from_slice(&[0xBA, 0x02, 0x00, 0x00, 0x00]); // mov edx, 2
    code.extend_from_slice(&[0x4D, 0x31, 0xC0, 0x4D, 0x31, 0xC9]); // xor r8,r8 / xor r9,r9
    code.extend_from_slice(&[0x0F, 0x05]);
    store(&mut code, 0, 0x08);
    code.extend_from_slice(&[0x48, 0x89, 0xD0]);
    store(&mut code, 0, 0x10);

    // second send, expect NoResource
    load_cap(&mut code, scratch + CHAN_CAPS);
    mov_eax(&mut code, Syscall::Send as u32);
    mov_r64_imm(&mut code, 6, scratch + MSG_AB);
    code.extend_from_slice(&[0xBA, 0x02, 0x00, 0x00, 0x00]);
    code.extend_from_slice(&[0x4D, 0x31, 0xC0, 0x4D, 0x31, 0xC9, 0x0F, 0x05]);
    store(&mut code, 0, 0x18);
    code.extend_from_slice(&[0x48, 0x89, 0xD0]);
    store(&mut code, 0, 0x20);

    // recv on end 1
    load_cap(&mut code, scratch + CHAN_CAPS + 8);
    mov_eax(&mut code, Syscall::Recv as u32);
    mov_r64_imm(&mut code, 6, scratch + RECV_BUF);
    code.extend_from_slice(&[0xBA, 0x08, 0x00, 0x00, 0x00]);
    code.extend_from_slice(&[0x4D, 0x31, 0xC0, 0x0F, 0x05]);
    store(&mut code, 0, 0x28);
    code.extend_from_slice(&[0x48, 0x89, 0xD0]);
    store(&mut code, 0, 0x30);
    mov_r64_imm(&mut code, 8, scratch + RECV_BUF);
    code.extend_from_slice(&[0x41, 0x0F, 0xB6, 0x00]);
    store(&mut code, 0, 0x38);
    mov_r64_imm(&mut code, 8, scratch + RECV_BUF + 1);
    code.extend_from_slice(&[0x41, 0x0F, 0xB6, 0x00]);
    store(&mut code, 0, 0x40);

    // pipe, write 'Z' on the write end, send the read cap.
    mov_eax(&mut code, Syscall::Pipe as u32);
    mov_r64_imm(&mut code, 7, scratch + PIPE_CAPS);
    code.extend_from_slice(&[0x0F, 0x05]);
    load_cap(&mut code, scratch + PIPE_CAPS + 8);
    mov_eax(&mut code, Syscall::Write as u32);
    mov_r64_imm(&mut code, 6, scratch + Z_BYTE);
    code.extend_from_slice(&[0xBA, 0x01, 0x00, 0x00, 0x00, 0x0F, 0x05]);

    load_cap(&mut code, scratch + PIPE_CAPS);
    code.extend_from_slice(&[0x49, 0x89, 0xC4]); // mov r12, rax
    load_cap(&mut code, scratch + CHAN_CAPS);
    code.extend_from_slice(&[0x4D, 0x89, 0xE0]); // mov r8, r12
    code.extend_from_slice(&[0x4D, 0x31, 0xC9]); // xor r9, r9
    mov_eax(&mut code, Syscall::Send as u32);
    mov_r64_imm(&mut code, 6, scratch + MSG_C);
    code.extend_from_slice(&[0xBA, 0x01, 0x00, 0x00, 0x00, 0x0F, 0x05]);
    store(&mut code, 0, 0x48);
    code.extend_from_slice(&[0x48, 0x89, 0xD0]);
    store(&mut code, 0, 0x50);

    load_cap(&mut code, scratch + CHAN_CAPS + 8);
    mov_eax(&mut code, Syscall::Recv as u32);
    mov_r64_imm(&mut code, 6, scratch + RECV_BUF);
    code.extend_from_slice(&[0xBA, 0x08, 0x00, 0x00, 0x00]);
    mov_r64_imm(&mut code, 8, scratch + CAPS_OUT);
    code.extend_from_slice(&[0x0F, 0x05]);
    store(&mut code, 0, 0x58);

    load_cap(&mut code, scratch + CAPS_OUT);
    mov_eax(&mut code, Syscall::Read as u32);
    mov_r64_imm(&mut code, 6, scratch + Z_OUT);
    code.extend_from_slice(&[0xBA, 0x01, 0x00, 0x00, 0x00, 0x0F, 0x05]);
    mov_r64_imm(&mut code, 8, scratch + Z_OUT);
    code.extend_from_slice(&[0x41, 0x0F, 0xB6, 0x00]);
    store(&mut code, 0, 0x60);

    mov_r64_imm(&mut code, 0, DONE);
    store(&mut code, 0, 0x00);
    mov_eax(&mut code, Syscall::Exit as u32);
    code.extend_from_slice(&[0x31, 0xFF, 0x0F, 0x05]);
    code
}

fn build_producer(code_base: u64, scratch: u64) -> alloc::vec::Vec<u8> {
    let task = b"receiver";
    let msg = b"ping";
    let mut code = alloc::vec::Vec::new();
    code.push(0xEB);
    code.push((task.len() + msg.len()) as u8);
    let task_addr = code_base + 2;
    code.extend_from_slice(task);
    let msg_addr = task_addr + task.len() as u64;
    code.extend_from_slice(msg);
    mov_r64_imm(&mut code, 15, scratch);

    mov_eax(&mut code, Syscall::Channel as u32);
    mov_r64_imm(&mut code, 7, scratch + 0x40);
    code.extend_from_slice(&[0x0F, 0x05]);
    code.extend_from_slice(&[0x48, 0x89, 0xD0]);
    store(&mut code, 0, 0x08);

    mov_r64_imm(&mut code, 8, scratch + 0x48);
    code.extend_from_slice(&[0x49, 0x8B, 0x00, 0x48, 0x89, 0xC7]);
    mov_eax(&mut code, Syscall::Give as u32);
    mov_r64_imm(&mut code, 6, task_addr);
    mov_r64_imm(&mut code, 2, task.len() as u64);
    code.extend_from_slice(&[0x0F, 0x05]);
    code.extend_from_slice(&[0x48, 0x89, 0xD0]);
    store(&mut code, 0, 0x10);

    load_cap(&mut code, scratch + 0x40);
    mov_eax(&mut code, Syscall::Send as u32);
    mov_r64_imm(&mut code, 6, msg_addr);
    mov_r64_imm(&mut code, 2, msg.len() as u64);
    code.extend_from_slice(&[0x4D, 0x31, 0xC0, 0x4D, 0x31, 0xC9, 0x0F, 0x05]);
    store(&mut code, 0, 0x18);
    code.extend_from_slice(&[0x48, 0x89, 0xD0]);
    store(&mut code, 0, 0x20);

    mov_r64_imm(&mut code, 0, DONE);
    store(&mut code, 0, 0x00);
    mov_eax(&mut code, Syscall::Exit as u32);
    code.extend_from_slice(&[0x31, 0xFF, 0x0F, 0x05]);
    code
}

fn build_consumer(scratch: u64) -> alloc::vec::Vec<u8> {
    let cap = Cap::new(FILE_CAP_BASE, CapRights::READ.union(CapRights::WRITE));
    let mut code = alloc::vec::Vec::new();
    code.push(0xEB);
    code.push(0);
    mov_r64_imm(&mut code, 15, scratch);
    let loop_at = code.len();
    mov_eax(&mut code, Syscall::Recv as u32);
    mov_r64_imm(&mut code, 7, cap.bits());
    mov_r64_imm(&mut code, 6, scratch + 0x40);
    code.extend_from_slice(&[0xBA, 0x08, 0x00, 0x00, 0x00]);
    code.extend_from_slice(&[0x4D, 0x31, 0xC0, 0x0F, 0x05]);
    code.extend_from_slice(&[0x48, 0x85, 0xD2]);
    let jz_yield = code.len();
    code.extend_from_slice(&[0x0F, 0x84, 0, 0, 0, 0]);
    store(&mut code, 2, 0x08);
    store(&mut code, 0, 0x10);
    mov_r64_imm(&mut code, 8, scratch + 0x40);
    code.extend_from_slice(&[0x41, 0x0F, 0xB6, 0x00]);
    store(&mut code, 0, 0x18);
    mov_r64_imm(&mut code, 0, DONE);
    store(&mut code, 0, 0x00);
    mov_eax(&mut code, Syscall::Exit as u32);
    code.extend_from_slice(&[0x31, 0xFF, 0x0F, 0x05]);

    let yield_at = code.len();
    let yrel = yield_at as i32 - (jz_yield as i32 + 6);
    code[jz_yield + 2..jz_yield + 6].copy_from_slice(&yrel.to_le_bytes());
    mov_eax(&mut code, Syscall::Yield as u32);
    code.extend_from_slice(&[0x0F, 0x05]);
    let after = code.len() + 5;
    let rel = loop_at as i32 - after as i32;
    code.push(0xE9);
    code.extend_from_slice(&rel.to_le_bytes());
    code
}

/// `mov rax, [addr]; mov rdi, rax` so the next syscall's cap is loaded.
fn load_cap(code: &mut alloc::vec::Vec<u8>, addr: u64) {
    mov_r64_imm(code, 8, addr);
    code.extend_from_slice(&[0x49, 0x8B, 0x00, 0x48, 0x89, 0xC7]);
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

/// `mov byte [r15+disp], imm`.
fn store_u8(code: &mut alloc::vec::Vec<u8>, disp: i32, imm: u8) {
    code.extend_from_slice(&[0x41, 0xC6, 0x87]);
    code.extend_from_slice(&disp.to_le_bytes());
    code.push(imm);
}
