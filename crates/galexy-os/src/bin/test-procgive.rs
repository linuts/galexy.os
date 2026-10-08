//! Integration test: `give` moves a process Cap (`PROC_TRANSFER`).
//!
//! `holder` spawns `hello`, proves give without TRANSFER is denied, then
//! gives the Cap to `peer`. `peer` Cap-waits for exit 0. Holder's wait
//! after give is BadCap.

#![no_std]
#![no_main]

extern crate alloc;

use bootloader_api::{entry_point, BootInfo};
use galexy_abi::{
    reserved, Cap, CapRights, SysError, Syscall, PROC_CAP_BASE,
};
use galexy_os::{
    arch::mm, drivers::screen, exit_qemu, println, sched, serial_println, QemuExitCode,
};

entry_point!(test_main_entry, config = &galexy_os::BOOTLOADER_CONFIG);

const DONE: u64 = 0x61_FE_CA91;
const TICK_TIMEOUT: u64 = 12_000;

#[repr(C)]
struct HolderReport {
    done: u64,
    spawn_ok: u64,
    deny_ok: u64,
    deny_err: u64,
    give_ok: u64,
    stale_ok: u64,
    stale_err: u64,
}

#[repr(C)]
struct PeerReport {
    done: u64,
    wait_ok: u64,
    wait_code: u64,
}

fn test_main_entry(boot_info: &'static mut BootInfo) -> ! {
    galexy_os::init();
    screen::init(boot_info);
    println!("[test-procgive] running");
    serial_println!("[test-procgive] running");

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

    // Peer first so it is live when holder gives.
    let (peer_region, _) = sched::spawn_user_launcher("peer", |gr| {
        unsafe {
            core::ptr::write_bytes(mm::frame_virt(gr.scratch_phys).as_mut_ptr::<u8>(), 0, 4096);
        }
        build_peer(gr.scratch.as_u64())
    });

    let (holder_region, _) = sched::spawn_user_launcher("holder", |gr| {
        unsafe {
            core::ptr::write_bytes(mm::frame_virt(gr.scratch_phys).as_mut_ptr::<u8>(), 0, 4096);
        }
        build_holder(gr.code.as_u64(), gr.scratch.as_u64())
    });

    let hold: *const HolderReport = mm::frame_virt(holder_region.scratch_phys).as_ptr();
    let mut elapsed = 0u64;
    // Spin-poll DONE before any reap (wipe + AP steal/reap race). IRQs on.
    x86_64::instructions::interrupts::enable();
    let href = loop {
        sched::drain_spawn();
        let done = unsafe { core::ptr::read_volatile(core::ptr::addr_of!((*hold).done)) };
        if done == DONE {
            break unsafe { core::ptr::read_volatile(hold) };
        }
        core::hint::spin_loop();
        elapsed += 1;
        if elapsed > TICK_TIMEOUT.saturating_mul(10_000) {
            panic!("holder never finished");
        }
    };
    assert_eq!(href.spawn_ok, 1, "spawn hello");
    assert_eq!(href.deny_ok, 0, "give without TRANSFER must fail");
    assert_eq!(
        href.deny_err,
        SysError::AccessDenied as u64,
        "missing PROC_TRANSFER is AccessDenied"
    );
    assert_eq!(href.give_ok, 1, "give must succeed");
    assert_eq!(href.stale_ok, 0, "wait after give must fail");
    assert_eq!(
        href.stale_err,
        SysError::BadCap as u64,
        "given-away Cap is BadCap"
    );

    let peer: *const PeerReport = mm::frame_virt(peer_region.scratch_phys).as_ptr();
    elapsed = 0;
    let pref = loop {
        sched::drain_spawn();
        let done = unsafe { core::ptr::read_volatile(core::ptr::addr_of!((*peer).done)) };
        if done == DONE {
            break unsafe { core::ptr::read_volatile(peer) };
        }
        core::hint::spin_loop();
        elapsed += 1;
        if elapsed > TICK_TIMEOUT.saturating_mul(10_000) {
            panic!("peer never finished");
        }
    };
    assert_eq!(pref.wait_ok, 1, "peer Cap-wait must succeed");
    assert_eq!(pref.wait_code, 0, "hello exits 0");

    loop {
        x86_64::instructions::hlt();
        sched::drain_spawn();
        sched::reap();
        if sched::unreaped_threads() == 0 {
            break;
        }
    }

    println!("[test-procgive] process Cap give works");
    serial_println!("[test-procgive] passed");
    exit_qemu(QemuExitCode::Success);
}

fn build_holder(code_base: u64, scratch: u64) -> alloc::vec::Vec<u8> {
    let hello = b"hello";
    let peer = b"peer";
    let mut code = alloc::vec::Vec::new();
    code.push(0xEB);
    code.push((hello.len() + peer.len()) as u8);
    let hello_addr = code_base + 2;
    code.extend_from_slice(hello);
    let peer_addr = hello_addr + hello.len() as u64;
    code.extend_from_slice(peer);

    mov_r64_imm(&mut code, 15, scratch);
    let loader = reserved::loader(CapRights::EXEC).bits();

    // spawn("hello")
    mov_eax(&mut code, Syscall::Spawn as u32);
    mov_r64_imm(&mut code, 7, loader);
    mov_r64_imm(&mut code, 6, hello_addr);
    mov_r64_imm(&mut code, 2, hello.len() as u64);
    mov_r64_imm(&mut code, 8, 0);
    mov_r64_imm(&mut code, 9, 0);
    mov_r64_imm(&mut code, 10, 0);
    code.extend_from_slice(&[0x0F, 0x05]);
    store(&mut code, 2, 0x08);
    code.extend_from_slice(&[0x49, 0x89, 0xC4]); // r12 = Cap

    // give without PROC_TRANSFER (WAIT|KILL|INSPECT only) → AccessDenied
    let thin = Cap::new(
        PROC_CAP_BASE,
        CapRights::PROC_WAIT
            .union(CapRights::PROC_KILL)
            .union(CapRights::PROC_INSPECT),
    )
    .bits();
    mov_eax(&mut code, Syscall::Give as u32);
    mov_r64_imm(&mut code, 7, thin);
    mov_r64_imm(&mut code, 6, peer_addr);
    mov_r64_imm(&mut code, 2, peer.len() as u64);
    code.extend_from_slice(&[0x0F, 0x05]);
    store(&mut code, 2, 0x10);
    store(&mut code, 0, 0x18);

    // give(real Cap, "peer")
    mov_eax(&mut code, Syscall::Give as u32);
    code.extend_from_slice(&[0x4C, 0x89, 0xE7]); // rdi = r12
    mov_r64_imm(&mut code, 6, peer_addr);
    mov_r64_imm(&mut code, 2, peer.len() as u64);
    code.extend_from_slice(&[0x0F, 0x05]);
    store(&mut code, 2, 0x20);

    // wait on old Cap bits → BadCap
    mov_eax(&mut code, Syscall::Wait as u32);
    code.extend_from_slice(&[0x4C, 0x89, 0xE7]);
    code.extend_from_slice(&[0x0F, 0x05]);
    store(&mut code, 2, 0x28);
    store(&mut code, 0, 0x30);

    mov_r64_imm(&mut code, 0, DONE);
    store(&mut code, 0, 0x00);
    mov_eax(&mut code, Syscall::Exit as u32);
    code.extend_from_slice(&[0x31, 0xFF, 0x0F, 0x05]);
    code
}

fn build_peer(scratch: u64) -> alloc::vec::Vec<u8> {
    let mut code = alloc::vec::Vec::new();
    code.push(0xEB);
    code.push(0);
    mov_r64_imm(&mut code, 15, scratch);

    // Poll wait(PROC_PARENT on PROC_CAP_BASE) until the Cap arrives.
    let wait_cap = Cap::new(PROC_CAP_BASE, CapRights::PROC_PARENT).bits();
    let loop_at = code.len();
    mov_eax(&mut code, Syscall::Wait as u32);
    mov_r64_imm(&mut code, 7, wait_cap);
    code.extend_from_slice(&[0x0F, 0x05]);
    // if rdx != 0 → success
    code.extend_from_slice(&[0x48, 0x85, 0xD2]); // test rdx, rdx
    let jnz = code.len();
    code.extend_from_slice(&[0x0F, 0x85, 0, 0, 0, 0]); // jnz got
    // yield and retry
    mov_eax(&mut code, Syscall::Yield as u32);
    code.extend_from_slice(&[0x0F, 0x05]);
    let after = code.len() + 5;
    let rel = loop_at as i32 - after as i32;
    code.push(0xE9);
    code.extend_from_slice(&rel.to_le_bytes());

    let got = code.len();
    let jrel = got as i32 - (jnz as i32 + 6);
    code[jnz + 2..jnz + 6].copy_from_slice(&jrel.to_le_bytes());

    store(&mut code, 2, 0x08); // wait_ok
    store(&mut code, 0, 0x10); // wait_code

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
