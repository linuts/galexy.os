//! Integration test: self Cap inspect snapshot.

#![no_std]
#![no_main]

extern crate alloc;

use bootloader_api::{entry_point, BootInfo};
use galexy_abi::{reserved, CapRights, Syscall};
use galexy_os::{
    arch::mm, drivers::screen, exit_qemu, println, sched, serial_println, QemuExitCode,
};

entry_point!(test_main_entry, config = &galexy_os::BOOTLOADER_CONFIG);

const DONE: u64 = 0x5E1F_CA91;
const TICK_TIMEOUT: u64 = 4000;
/// Offset of [`Report::text`] in the scratch page.
const TEXT_OFF: u64 = 0x28;

#[repr(C)]
struct Report {
    done: u64,
    read_ok: u64,
    read_n: u64,
    denied_ok: u64,
    denied_err: u64,
    /// Inspect text starts here (after the u64 fields).
    text: [u8; 128],
}

fn test_main_entry(boot_info: &'static mut BootInfo) -> ! {
    galexy_os::init();
    screen::init(boot_info);
    println!("[test-selfcap] running");
    serial_println!("[test-selfcap] running");

    mm::init(boot_info);
    galexy_os::arch::init(boot_info);
    sched::init();

    let (region, _) = sched::spawn_user_task("selfcap", |gr| {
        unsafe {
            core::ptr::write_bytes(mm::frame_virt(gr.scratch_phys).as_mut_ptr::<u8>(), 0, 4096);
        }
        build_blob(gr.scratch.as_u64())
    });

    let scratch: *const Report = mm::frame_virt(region.scratch_phys).as_ptr();
    let mut elapsed = 0u64;
    let report = loop {
        x86_64::instructions::hlt();
        sched::reap();
        let done = unsafe { core::ptr::read_volatile(core::ptr::addr_of!((*scratch).done)) };
        if done == DONE {
            break unsafe { core::ptr::read_volatile(scratch) };
        }
        elapsed += 1;
        if elapsed > TICK_TIMEOUT {
            panic!("selfcap blob never finished");
        }
    };

    assert_eq!(report.read_ok, 1, "self Cap read must succeed");
    assert!(report.read_n > 0, "inspect line non-empty");

    let n = (report.read_n as usize).min(report.text.len());
    let text = core::str::from_utf8(&report.text[..n]).expect("utf8 inspect");
    assert!(text.starts_with("id="), "inspect starts with id=; got {text:?}");
    assert!(
        text.contains("name=selfcap"),
        "inspect names the task; got {text:?}"
    );
    assert!(
        text.contains("state=running"),
        "inspect shows running; got {text:?}"
    );

    assert_eq!(report.denied_ok, 0, "READ-only forgery must fail");
    assert_eq!(
        report.denied_err,
        galexy_abi::SysError::AccessDenied as u64,
        "missing PROC_INSPECT is AccessDenied"
    );

    loop {
        x86_64::instructions::hlt();
        sched::reap();
        if sched::unreaped_threads() == 0 {
            break;
        }
    }

    println!("[test-selfcap] self Cap inspect works");
    serial_println!("[test-selfcap] passed");
    exit_qemu(QemuExitCode::Success);
}

fn build_blob(scratch: u64) -> alloc::vec::Vec<u8> {
    let mut code = alloc::vec::Vec::new();
    code.push(0xEB);
    code.push(0);
    mov_r64_imm(&mut code, 15, scratch);

    let self_bits = reserved::self_cap().bits();
    let buf = scratch + TEXT_OFF;

    // read(self, buf, 128)
    mov_eax(&mut code, Syscall::Read as u32);
    mov_r64_imm(&mut code, 7, self_bits);
    mov_r64_imm(&mut code, 6, buf);
    mov_r64_imm(&mut code, 2, 128);
    code.extend_from_slice(&[0x0F, 0x05]);
    store(&mut code, 2, 0x08); // read_ok
    store(&mut code, 0, 0x10); // read_n

    // Forgery: SELF_INDEX with READ only (no PROC_INSPECT).
    let forged = galexy_abi::Cap::new(reserved::SELF_INDEX, CapRights::READ).bits();
    mov_eax(&mut code, Syscall::Read as u32);
    mov_r64_imm(&mut code, 7, forged);
    mov_r64_imm(&mut code, 6, buf);
    mov_r64_imm(&mut code, 2, 16);
    code.extend_from_slice(&[0x0F, 0x05]);
    store(&mut code, 2, 0x18);
    store(&mut code, 0, 0x20);

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
