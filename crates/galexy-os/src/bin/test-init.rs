//! Integration test: Milestone 53 init orphan root.
//!
//! Loads ramdisk `init`, then a parent that spawns `linger` and exits.
//! After the parent is reaped, `linger` is reparented to init (not kernel
//! slot 0). Init is marked unkillable.

#![no_std]
#![no_main]

extern crate alloc;

use bootloader_api::{entry_point, BootInfo};
use galexy_abi::{reserved, CapRights, SysError, Syscall};
use galexy_os::{
    arch::mm, drivers::screen, exit_qemu, println, sched, serial_println, QemuExitCode,
};

entry_point!(test_main_entry, config = &galexy_os::BOOTLOADER_CONFIG);

const DONE: u64 = 0x0053_0117;
const TICK_TIMEOUT: u64 = 12_000;

#[repr(C)]
struct Report {
    done: u64,
    spawn_ok: u64,
    spawn_err: u64,
}

fn test_main_entry(boot_info: &'static mut BootInfo) -> ! {
    galexy_os::init();
    screen::init(boot_info);
    println!("[test-init] running");
    serial_println!("[test-init] running");

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

    assert!(
        sched::spawn_init(),
        "ramdisk must contain init for Milestone 53"
    );
    let init = sched::init_slot().expect("init slot after spawn");
    assert!(
        sched::init_unkillable(),
        "init must be marked immortal to Cap-kill"
    );

    // Milestone 54/67: init spawns seats and stamp through the single
    // PENDING_SPAWN slot. Wait until it logs ready, after those spawns
    // have returned, so linger is not rejected with NoResource.
    let mut boot_log = [0u8; 4096];
    let mut elapsed = 0u64;
    loop {
        x86_64::instructions::hlt();
        sched::drain_spawn();
        sched::reap();
        let n = galexy_os::drivers::dmesg::snapshot(&mut boot_log);
        let text = core::str::from_utf8(&boot_log[..n]).unwrap_or("");
        if text.contains("[init] ready") && sched::seats_are_live() && !sched::spawn_is_pending() {
            break;
        }
        elapsed += 1;
        if elapsed > TICK_TIMEOUT {
            panic!("init never finished autostart; dmesg:\n{text}");
        }
    }

    assert!(sched::ramdisk::find("linger").is_some());

    let (region, _) = sched::spawn_user_launcher("orphan-parent", |gr| {
        unsafe {
            core::ptr::write_bytes(mm::frame_virt(gr.scratch_phys).as_mut_ptr::<u8>(), 0, 4096);
        }
        build_blob(gr.code.as_u64(), gr.scratch.as_u64())
    });

    let scratch: *const Report = mm::frame_virt(region.scratch_phys).as_ptr();
    elapsed = 0;
    // Do not reap until the report is copied — parent exit frees scratch.
    let report = loop {
        x86_64::instructions::hlt();
        sched::drain_spawn();
        let done = unsafe { core::ptr::read_volatile(core::ptr::addr_of!((*scratch).done)) };
        if done == DONE {
            break unsafe { core::ptr::read_volatile(scratch) };
        }
        elapsed += 1;
        if elapsed > TICK_TIMEOUT {
            panic!("orphan-parent never finished spawn");
        }
    };
    assert_eq!(
        report.spawn_ok, 1,
        "spawn linger must succeed (err={})",
        report.spawn_err
    );

    elapsed = 0;
    loop {
        x86_64::instructions::hlt();
        sched::drain_spawn();
        sched::reap();
        if !sched::is_name_running("orphan-parent") && !sched::is_name_live("orphan-parent") {
            break;
        }
        elapsed += 1;
        if elapsed > TICK_TIMEOUT {
            panic!("orphan-parent never exited");
        }
    }

    elapsed = 0;
    let parent = loop {
        x86_64::instructions::hlt();
        sched::reap();
        assert!(
            sched::is_name_running("linger"),
            "linger must keep running after parent exit"
        );
        if let Some(p) = sched::parent_slot_of("linger") {
            if p == init {
                break p;
            }
        }
        elapsed += 1;
        if elapsed > TICK_TIMEOUT {
            panic!(
                "linger never reparented to init; parent={:?} init={init}",
                sched::parent_slot_of("linger")
            );
        }
    };
    assert_eq!(parent, init, "orphan parent_slot is init");
    println!("[test-init] orphan Cap transfer to init works");

    // A task with no session must not drive init.
    let denied = run_rpc(
        "rpc-denied",
        galexy_os::sched::galfs::unauth_cred(),
        &[galexy_abi::INIT_OP_STATUS, 5, b's', b'h', b'e', b'l', b'l'],
    );
    sched::reap();
    assert_eq!(denied.rdx, 0, "logged-out send must fail");
    assert_eq!(
        denied.rax,
        SysError::AccessDenied as u64,
        "logged-out send is AccessDenied, got {}",
        denied.rax
    );
    serial_println!("[test-init] logged-out init rpc denied");

    // A non-admin session may ask status but may not change service
    // state: `svc stop` on another user's seat is refused in the reply
    // and the seat keeps running.
    let eve_root =
        galexy_os::sched::galfs::add_actor_named("eve", b"evepass1").expect("add non-admin actor");
    let eve = galexy_os::sched::galfs::FsCred::launcher(eve_root);
    let (status, status_reply) = run_rpc_reply(
        "rpc-eve-status",
        eve,
        &[
            galexy_abi::INIT_OP_STATUS,
            6,
            b's',
            b'h',
            b'e',
            b'l',
            b'l',
            b'2',
        ],
    );
    sched::reap();
    assert_eq!(status.rdx, 1, "non-admin svc status must be accepted");
    assert!(
        status_reply.starts_with(b"shell2 "),
        "status reply names the service: {:?}",
        core::str::from_utf8(&status_reply).unwrap_or("?")
    );
    let (stop, stop_reply) = run_rpc_reply(
        "rpc-eve-stop",
        eve,
        &[
            galexy_abi::INIT_OP_STOP,
            6,
            b's',
            b'h',
            b'e',
            b'l',
            b'l',
            b'2',
        ],
    );
    sched::reap();
    assert_eq!(stop.rdx, 1, "non-admin svc stop reaches init");
    assert!(
        stop_reply.starts_with(b"access denied"),
        "non-admin svc stop must be refused, got {:?}",
        core::str::from_utf8(&stop_reply).unwrap_or("?")
    );
    // The seat is parked on its keyboard read (WAITING), so "live", not
    // "running".
    assert!(
        sched::is_name_live("shell2"),
        "shell2 must survive a non-admin stop"
    );
    serial_println!("[test-init] non-admin svc stop denied");

    // Admin session starts the restart-storm fixture. The second fast
    // exit is the 250 ms backoff the service table promises.
    let started = run_rpc(
        "rpc-admin",
        galexy_os::sched::galfs::admin_cred(),
        &[galexy_abi::INIT_OP_START, 5, b'p', b'r', b'o', b'b', b'e'],
    );
    assert_eq!(started.rdx, 1, "admin svc start must be accepted");
    sched::reap();

    let mut log = [0u8; 4096];
    elapsed = 0;
    loop {
        x86_64::instructions::hlt();
        sched::drain_spawn();
        sched::reap();
        let n = galexy_os::drivers::dmesg::snapshot(&mut log);
        let text = core::str::from_utf8(&log[..n]).unwrap_or("");
        if text.contains("[init] backoff name=probe ms=250") {
            break;
        }
        elapsed += 1;
        if elapsed > TICK_TIMEOUT {
            panic!("probe never backed off; dmesg:\n{text}");
        }
    }

    println!("[test-init] service backoff works");
    serial_println!("[test-init] passed");
    exit_qemu(QemuExitCode::Success);
}

#[repr(C)]
struct RpcReport {
    done: u64,
    rax: u64,
    rdx: u64,
}

fn run_rpc(name: &str, fs: galexy_os::sched::galfs::FsCred, payload: &[u8]) -> RpcReport {
    run_rpc_reply(name, fs, payload).0
}

/// Like [`run_rpc`], also returning init's reply bytes (scratch + 0x100).
fn run_rpc_reply(
    name: &str,
    fs: galexy_os::sched::galfs::FsCred,
    payload: &[u8],
) -> (RpcReport, [u8; 64]) {
    let (region, _) = sched::spawn_user_launcher_with(name, fs, |gr| {
        unsafe {
            core::ptr::write_bytes(mm::frame_virt(gr.scratch_phys).as_mut_ptr::<u8>(), 0, 4096);
        }
        build_rpc_blob(gr.code.as_u64(), gr.scratch.as_u64(), payload)
    });
    let base = mm::frame_virt(region.scratch_phys);
    let scratch: *const RpcReport = base.as_ptr();
    let reply_ptr: *const [u8; 64] = (base + 0x100u64).as_ptr();
    let mut elapsed = 0u64;
    loop {
        x86_64::instructions::hlt();
        sched::drain_spawn();
        let done = unsafe { core::ptr::read_volatile(core::ptr::addr_of!((*scratch).done)) };
        if done == DONE {
            let report = unsafe { core::ptr::read_volatile(scratch) };
            let reply = unsafe { core::ptr::read_volatile(reply_ptr) };
            return (report, reply);
        }
        elapsed += 1;
        if elapsed > TICK_TIMEOUT {
            panic!("{name} never finished the init rpc");
        }
    }
}

fn build_rpc_blob(code_base: u64, scratch: u64, payload: &[u8]) -> alloc::vec::Vec<u8> {
    let mut code = alloc::vec::Vec::new();
    code.push(0xEB);
    code.push(payload.len() as u8);
    let payload_addr = code_base + 2;
    code.extend_from_slice(payload);

    mov_r64_imm(&mut code, 15, scratch);
    let cap = reserved::init(CapRights::WRITE).bits();
    mov_eax(&mut code, Syscall::Send as u32);
    mov_r64_imm(&mut code, 7, cap);
    mov_r64_imm(&mut code, 6, payload_addr);
    mov_r64_imm(&mut code, 2, payload.len() as u64);
    mov_r64_imm(&mut code, 8, scratch + 0x100);
    mov_r64_imm(&mut code, 9, 64);
    code.extend_from_slice(&[0x0F, 0x05]);
    store(&mut code, 0, 0x08);
    store(&mut code, 2, 0x10);
    mov_r64_imm(&mut code, 0, DONE);
    store(&mut code, 0, 0x00);
    mov_eax(&mut code, Syscall::Exit as u32);
    code.extend_from_slice(&[0x31, 0xFF, 0x0F, 0x05]);
    code
}

fn build_blob(code_base: u64, scratch: u64) -> alloc::vec::Vec<u8> {
    let linger = b"linger";
    let mut code = alloc::vec::Vec::new();
    code.push(0xEB);
    code.push(linger.len() as u8);
    let linger_addr = code_base + 2;
    code.extend_from_slice(linger);

    mov_r64_imm(&mut code, 15, scratch);
    let loader = reserved::loader(CapRights::EXEC).bits();

    mov_eax(&mut code, Syscall::Spawn as u32);
    mov_r64_imm(&mut code, 7, loader);
    mov_r64_imm(&mut code, 6, linger_addr);
    mov_r64_imm(&mut code, 2, linger.len() as u64);
    mov_r64_imm(&mut code, 8, 0);
    mov_r64_imm(&mut code, 9, 0);
    mov_r64_imm(&mut code, 10, 0);
    code.extend_from_slice(&[0x0F, 0x05]);
    store(&mut code, 2, 0x08);
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
