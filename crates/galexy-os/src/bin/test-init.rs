//! Integration test: Milestone 53 init orphan root.
//!
//! Loads ramdisk `init`, then a parent that spawns `linger` and exits.
//! After the parent is reaped, `linger` is reparented to init (not kernel
//! slot 0). Init is marked unkillable.

#![no_std]
#![no_main]

extern crate alloc;

use bootloader_api::{entry_point, BootInfo};
use galexy_abi::{reserved, CapRights, Syscall};
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

    assert!(sched::ramdisk::find("linger").is_some());

    let (region, _) = sched::spawn_user_launcher("orphan-parent", |gr| {
        unsafe {
            core::ptr::write_bytes(mm::frame_virt(gr.scratch_phys).as_mut_ptr::<u8>(), 0, 4096);
        }
        build_blob(gr.code.as_u64(), gr.scratch.as_u64())
    });

    let scratch: *const Report = mm::frame_virt(region.scratch_phys).as_ptr();
    let mut elapsed = 0u64;
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
    assert_eq!(report.spawn_ok, 1, "spawn linger must succeed");

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
    serial_println!("[test-init] passed");
    exit_qemu(QemuExitCode::Success);
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
