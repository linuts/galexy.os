//! Integration test: pathological input (Milestone 51) — a tight ring-3
//! console write loop. The per-task console budget (512 bytes per timer
//! tick) must hold exactly: the bytes the kernel admits over the run
//! never exceed `512 × (ticks + 2)`, the task is never blocked or killed
//! by flooding (every `write` returns success, possibly `0`), and the
//! kernel keeps ticking underneath it.
//!
//! The other two pathological inputs named in Milestone 51 live
//! elsewhere: deep / overlong path components are `bin/test-paths` plus
//! the exhaustive `galexy_core::path` host sweep, and a huge paste at
//! the password prompt is the `shell_password_paste_typing_e2e` boot.

#![no_std]
#![no_main]

extern crate alloc;

use bootloader_api::{entry_point, BootInfo};
use galexy_abi::{reserved, CapRights, Syscall};
use galexy_os::{
    arch, arch::mm, drivers::screen, exit_qemu, println, sched, serial_println, QemuExitCode,
};

entry_point!(test_main_entry, config = &galexy_os::BOOTLOADER_CONFIG);

const DONE: u64 = 0x0000_4854_4150_5456;
const TICK_TIMEOUT: u64 = 8000;
/// Console budget per task per tick (`sched::CONSOLE_BUDGET_PER_TICK`).
const BUDGET: u64 = 512;
/// `write` calls in the tight loop.
const WRITES: u64 = 3000;
const MSG: &[u8] = b"flood-flood-flood-flood-flood!!\n";

#[repr(C)]
struct Report {
    done: u64,
    admitted: u64,
    failures: u64,
}

fn test_main_entry(boot_info: &'static mut BootInfo) -> ! {
    galexy_os::init();
    screen::init(boot_info);
    println!("[test-pathological] running");
    serial_println!("[test-pathological] running");

    mm::init(boot_info);
    arch::init(boot_info);
    sched::init();

    let baseline = mm::free_frames();
    let t0 = arch::timer_ticks();
    let (region, _) = sched::spawn_user_task("flood", |gr| {
        unsafe {
            core::ptr::write_bytes(mm::frame_virt(gr.scratch_phys).as_mut_ptr::<u8>(), 0, 4096);
        }
        build_flood(gr.code.as_u64(), gr.scratch.as_u64())
    });
    let scratch: *const Report = mm::frame_virt(region.scratch_phys).as_ptr();
    let mut elapsed = 0u64;
    x86_64::instructions::interrupts::enable();
    let report = loop {
        let done = unsafe { core::ptr::read_volatile(core::ptr::addr_of!((*scratch).done)) };
        if done == DONE {
            break unsafe { core::ptr::read_volatile(scratch) };
        }
        core::hint::spin_loop();
        elapsed += 1;
        if elapsed > TICK_TIMEOUT.saturating_mul(10_000) {
            panic!("flood blob never finished");
        }
    };
    let t1 = arch::timer_ticks();
    loop {
        x86_64::instructions::hlt();
        sched::reap();
        if sched::unreaped_threads() == 0 {
            break;
        }
    }

    let ticks = t1 - t0;
    let asked = WRITES * MSG.len() as u64;
    serial_println!(
        "\n[test-pathological] {} writes asked {} bytes; admitted {} over {} tick(s); failures {}",
        WRITES,
        asked,
        report.admitted,
        ticks,
        report.failures
    );
    assert_eq!(
        report.failures, 0,
        "flooding must never turn write into an error"
    );
    assert!(report.admitted > 0, "the console must admit something");
    assert!(
        report.admitted <= BUDGET * (ticks + 2),
        "console budget violated: admitted {} > {} x ({} + 2)",
        report.admitted,
        BUDGET,
        ticks
    );
    assert!(ticks > 0, "the timer must keep ticking under a flood");
    assert_eq!(
        mm::free_frames(),
        baseline,
        "flood task must return every frame"
    );

    println!("[test-pathological] console budget held");
    serial_println!("[test-pathological] passed");
    exit_qemu(QemuExitCode::Success);
}

/// r12 = loop counter, r14 = admitted bytes, r13 = failed calls.
fn build_flood(code_base: u64, scratch: u64) -> alloc::vec::Vec<u8> {
    let mut code = alloc::vec::Vec::new();
    code.push(0xEB);
    code.push(MSG.len() as u8);
    let msg = code_base + 2;
    code.extend_from_slice(MSG);

    mov_r64_imm(&mut code, 15, scratch);
    mov_r64_imm(&mut code, 12, WRITES);
    mov_r64_imm(&mut code, 14, 0);
    mov_r64_imm(&mut code, 13, 0);
    let console = reserved::console(CapRights::WRITE).bits();

    let top = code.len();
    mov_eax(&mut code, Syscall::Write as u32);
    mov_r64_imm(&mut code, 7, console);
    mov_r64_imm(&mut code, 6, msg);
    mov_r64_imm(&mut code, 2, MSG.len() as u64);
    code.extend_from_slice(&[0x0F, 0x05]); // syscall
                                           // if rdx == 0 { r13 += 1 } else { r14 += rax }
    code.extend_from_slice(&[0x48, 0x85, 0xD2]); // test rdx, rdx
    code.extend_from_slice(&[0x75, 0x05]); // jnz +5 (skip the inc)
    code.extend_from_slice(&[0x49, 0xFF, 0xC5]); // inc r13
    code.extend_from_slice(&[0xEB, 0x03]); // jmp +3 (skip the add)
    code.extend_from_slice(&[0x49, 0x01, 0xC6]); // add r14, rax
    code.extend_from_slice(&[0x49, 0xFF, 0xCC]); // dec r12
    let back = top as i64 - (code.len() as i64 + 2);
    code.extend_from_slice(&[0x75, back as i8 as u8]); // jnz top

    store(&mut code, 14, 0x08);
    store(&mut code, 13, 0x10);
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

/// `mov [r15 + disp], reg`.
fn store(code: &mut alloc::vec::Vec<u8>, reg: u8, disp: i32) {
    let rex = 0x49 | (u8::from(reg >= 8) << 2);
    let modrm = 0x80 | ((reg & 7) << 3) | 7;
    code.extend_from_slice(&[rex, 0x89, modrm]);
    code.extend_from_slice(&disp.to_le_bytes());
}
