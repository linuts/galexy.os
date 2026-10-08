//! Integration test: soak (Milestone 51). Rounds of real work with idle
//! gaps in between; every fixed table must return to its baseline after
//! every round.
//!
//! One round spawns a seat-grade ring-3 worker that: opens a pipe, writes
//! and reads through it, closes both ends; creates a galfs file, writes
//! it, closes it, removes it; spawns the ramdisk `hello` with
//! `SPAWN_WAIT` (the real ELF loader, a real exit, a real reap); sleeps a
//! few ticks; exits. The kernel then idles (tickless) before the next
//! round and asserts free frames, pipe slots, thread slots, galfs blocks,
//! and the galfs tree are exactly where they started.
//!
//! Short by suite design (one QEMU boot is bounded at 60 s under TCG);
//! the shape is what matters — each round is one spawn/exit/pipe/galfs
//! lifecycle, and the asserts are exact, not "roughly stable".

#![no_std]
#![no_main]

extern crate alloc;

use bootloader_api::{entry_point, BootInfo};
use galexy_abi::{reserved, CapRights, Syscall, SPAWN_WAIT};
use galexy_os::sched::{galfs, pipe};
use galexy_os::{
    arch, arch::mm, drivers::screen, exit_qemu, println, sched, serial_println, QemuExitCode,
};

entry_point!(test_main_entry, config = &galexy_os::BOOTLOADER_CONFIG);

const DONE: u64 = 0x0000_4B41_4F53_5456;
/// Written by the kernel once the report is copied; the worker exits
/// only after seeing it, so a reap can never wipe an unread report.
const ACK: u64 = 0x0000_4B43_414B_4F53;
const ACK_OFF: i32 = 0x300;
const TICK_TIMEOUT: u64 = 8000;
/// Rounds of work. Round 0 warms the kernel (first ELF parse, first heap
/// growth); the baseline is taken after it.
const ROUNDS: usize = 10;
/// Idle ticks between rounds — long enough for the tickless idle path to
/// stretch the LAPIC deadline.
const IDLE_TICKS: u64 = 25;
const MSG: &[u8] = b"soak!";

/// Scratch layout: `done`, then (ok, value) pairs, then the pipe buffer
/// at +0x100 and the read-back buffer at +0x200.
#[repr(C)]
struct Report {
    done: u64,
    r: [[u64; 2]; 9],
}

const R_PIPE: usize = 0;
const R_PWRITE: usize = 1;
const R_PREAD: usize = 2;
const R_CREATE: usize = 3;
const R_FWRITE: usize = 4;
const R_REMOVE: usize = 5;
const R_SPAWN: usize = 6;
const R_SLEEP: usize = 7;
const R_CLOSE: usize = 8;

fn test_main_entry(boot_info: &'static mut BootInfo) -> ! {
    galexy_os::init();
    screen::init(boot_info);
    println!("[test-soak] running");
    serial_println!("[test-soak] running");

    let Some(ramdisk_addr) = boot_info.ramdisk_addr.into_option() else {
        panic!("no ramdisk");
    };
    mm::init(boot_info);
    arch::init(boot_info);
    sched::init();
    let archive = unsafe {
        core::slice::from_raw_parts(
            x86_64::VirtAddr::new(ramdisk_addr).as_ptr::<u8>(),
            boot_info.ramdisk_len as usize,
        )
    };
    sched::ramdisk::init(archive);
    assert!(sched::ramdisk::find("hello").is_some(), "hello on ramdisk");

    let admin = galfs::admin_root();
    let mut baseline: Option<(usize, usize)> = None;
    let start_ticks = arch::timer_ticks();

    for round in 0..ROUNDS {
        let report = run_round(round);
        check_report(round, &report);

        idle_for(IDLE_TICKS);

        let frames = mm::free_frames();
        let blocks = galfs::blocks_used();
        assert_eq!(pipe::in_use(), 0, "round {round}: pipe slot leaked");
        assert_eq!(
            sched::unreaped_threads(),
            0,
            "round {round}: thread slot leaked"
        );
        assert!(
            galfs::find_under(admin, "soak.txt").is_none(),
            "round {round}: soak.txt still present"
        );
        match baseline {
            None => {
                baseline = Some((frames, blocks));
                serial_println!(
                    "[test-soak] round 0 warm: frames={} blocks={}",
                    frames,
                    blocks
                );
            }
            Some((f0, b0)) => {
                assert_eq!(
                    frames, f0,
                    "round {round}: free frames drifted (got {frames}, want {f0})"
                );
                assert_eq!(
                    blocks, b0,
                    "round {round}: galfs blocks drifted (got {blocks}, want {b0})"
                );
                println!("[test-soak] round {}: tables closed", round);
            }
        }
    }

    let elapsed = arch::timer_ticks() - start_ticks;
    serial_println!(
        "[test-soak] {} rounds, {} tick(s), no leak in frames / pipes / threads / blocks",
        ROUNDS,
        elapsed
    );
    println!("[test-soak] soak ok");
    serial_println!("[test-soak] passed");
    exit_qemu(QemuExitCode::Success);
}

fn check_report(round: usize, report: &Report) {
    let ok = |i: usize| report.r[i][0] == 1;
    assert!(
        ok(R_PIPE),
        "round {round}: pipe failed ({})",
        report.r[R_PIPE][1]
    );
    assert!(ok(R_PWRITE), "round {round}: pipe write failed");
    assert_eq!(
        report.r[R_PWRITE][1],
        MSG.len() as u64,
        "round {round}: pipe write len"
    );
    assert!(ok(R_PREAD), "round {round}: pipe read failed");
    assert_eq!(
        report.r[R_PREAD][1],
        MSG.len() as u64,
        "round {round}: pipe read len"
    );
    assert!(ok(R_CLOSE), "round {round}: pipe close failed");
    assert!(
        ok(R_CREATE),
        "round {round}: create failed ({})",
        report.r[R_CREATE][1]
    );
    assert!(ok(R_FWRITE), "round {round}: file write failed");
    assert_eq!(
        report.r[R_FWRITE][1],
        MSG.len() as u64,
        "round {round}: file write len"
    );
    assert!(
        ok(R_REMOVE),
        "round {round}: remove failed ({})",
        report.r[R_REMOVE][1]
    );
    assert!(
        ok(R_SPAWN),
        "round {round}: spawn hello failed ({})",
        report.r[R_SPAWN][1]
    );
    assert_eq!(report.r[R_SPAWN][1], 0, "round {round}: hello exit code");
    assert!(ok(R_SLEEP), "round {round}: sleep failed");
}

/// Idles on `hlt` (reaping as it goes) until `ticks` have passed.
fn idle_for(ticks: u64) {
    let until = arch::timer_ticks() + ticks;
    while arch::timer_ticks() < until {
        x86_64::instructions::hlt();
        sched::drain_spawn();
        sched::reap();
    }
}

/// Spawns the worker, drives spawns, waits for DONE, drains the rotation.
fn run_round(round: usize) -> Report {
    let (region, _) = sched::spawn_user_launcher("soak", |gr| {
        unsafe {
            core::ptr::write_bytes(mm::frame_virt(gr.scratch_phys).as_mut_ptr::<u8>(), 0, 4096);
        }
        build_worker(gr.code.as_u64(), gr.scratch.as_u64())
    });
    let base = mm::frame_virt(region.scratch_phys);
    let scratch: *const Report = base.as_ptr();
    let mut elapsed = 0u64;
    x86_64::instructions::interrupts::enable();
    // Reap inside the poll: `hello` may be owned by this CPU, and the
    // worker's SPAWN_WAIT only returns once the child is reaped. The
    // worker cannot exit (and lose its scratch page) before ACK.
    let report = loop {
        sched::drain_spawn();
        sched::reap();
        let done = unsafe { core::ptr::read_volatile(core::ptr::addr_of!((*scratch).done)) };
        if done == DONE {
            let report = unsafe { core::ptr::read_volatile(scratch) };
            unsafe {
                core::ptr::write_volatile(
                    base.as_mut_ptr::<u8>().add(ACK_OFF as usize).cast(),
                    ACK,
                );
            }
            break report;
        }
        core::hint::spin_loop();
        elapsed += 1;
        if elapsed > TICK_TIMEOUT.saturating_mul(10_000) {
            panic!("round {round}: worker never finished");
        }
    };
    loop {
        x86_64::instructions::hlt();
        sched::drain_spawn();
        sched::reap();
        if sched::unreaped_threads() == 0 {
            break;
        }
    }
    report
}

fn build_worker(code_base: u64, scratch: u64) -> alloc::vec::Vec<u8> {
    let mut code = alloc::vec::Vec::new();
    let [msg, path, hello] = embed(&mut code, code_base, [MSG, b"soak.txt", b"hello"]);
    mov_r64_imm(&mut code, 15, scratch);
    let pair = |i: usize| (0x08 + (i as i32) * 0x10, 0x10 + (i as i32) * 0x10);

    // pipe(scratch + 0x100) → caps at +0x100 (read) and +0x108 (write).
    mov_eax(&mut code, Syscall::Pipe as u32);
    mov_r64_imm(&mut code, 7, scratch + 0x100);
    syscall(&mut code);
    store(&mut code, 2, pair(R_PIPE).0);
    store(&mut code, 0, pair(R_PIPE).1);

    // write(pipe_w, msg, len)
    mov_eax(&mut code, Syscall::Write as u32);
    load(&mut code, 7, 0x108);
    mov_r64_imm(&mut code, 6, msg);
    mov_r64_imm(&mut code, 2, MSG.len() as u64);
    syscall(&mut code);
    store(&mut code, 2, pair(R_PWRITE).0);
    store(&mut code, 0, pair(R_PWRITE).1);

    // read(pipe_r, scratch + 0x200, len)
    mov_eax(&mut code, Syscall::Read as u32);
    load(&mut code, 7, 0x100);
    mov_r64_imm(&mut code, 6, scratch + 0x200);
    mov_r64_imm(&mut code, 2, MSG.len() as u64);
    syscall(&mut code);
    store(&mut code, 2, pair(R_PREAD).0);
    store(&mut code, 0, pair(R_PREAD).1);

    // close both ends; the second result is the one recorded.
    mov_eax(&mut code, Syscall::Close as u32);
    load(&mut code, 7, 0x108);
    syscall(&mut code);
    mov_eax(&mut code, Syscall::Close as u32);
    load(&mut code, 7, 0x100);
    syscall(&mut code);
    store(&mut code, 2, pair(R_CLOSE).0);
    store(&mut code, 0, pair(R_CLOSE).1);

    // create soak.txt (replace) → cap in rax, kept in r12.
    mov_eax(&mut code, Syscall::Create as u32);
    mov_r64_imm(&mut code, 7, path);
    mov_r64_imm(&mut code, 6, 8);
    mov_r64_imm(&mut code, 2, 1);
    syscall(&mut code);
    store(&mut code, 2, pair(R_CREATE).0);
    store(&mut code, 0, pair(R_CREATE).1);
    code.extend_from_slice(&[0x49, 0x89, 0xC4]); // mov r12, rax

    // write(file, msg, len); close(file)
    mov_eax(&mut code, Syscall::Write as u32);
    code.extend_from_slice(&[0x4C, 0x89, 0xE7]); // mov rdi, r12
    mov_r64_imm(&mut code, 6, msg);
    mov_r64_imm(&mut code, 2, MSG.len() as u64);
    syscall(&mut code);
    store(&mut code, 2, pair(R_FWRITE).0);
    store(&mut code, 0, pair(R_FWRITE).1);
    mov_eax(&mut code, Syscall::Close as u32);
    code.extend_from_slice(&[0x4C, 0x89, 0xE7]);
    syscall(&mut code);

    // remove soak.txt
    mov_eax(&mut code, Syscall::Remove as u32);
    mov_r64_imm(&mut code, 7, path);
    mov_r64_imm(&mut code, 6, 8);
    syscall(&mut code);
    store(&mut code, 2, pair(R_REMOVE).0);
    store(&mut code, 0, pair(R_REMOVE).1);

    // spawn hello with SPAWN_WAIT → rax = exit code
    let loader = reserved::loader(CapRights::EXEC).bits();
    mov_eax(&mut code, Syscall::Spawn as u32);
    mov_r64_imm(&mut code, 7, loader);
    mov_r64_imm(&mut code, 6, hello);
    mov_r64_imm(&mut code, 2, 5);
    mov_r64_imm(&mut code, 8, 0);
    mov_r64_imm(&mut code, 9, 0);
    mov_r64_imm(&mut code, 10, SPAWN_WAIT);
    syscall(&mut code);
    store(&mut code, 2, pair(R_SPAWN).0);
    store(&mut code, 0, pair(R_SPAWN).1);

    // sleep(3)
    mov_eax(&mut code, Syscall::Sleep as u32);
    mov_r64_imm(&mut code, 7, 3);
    syscall(&mut code);
    store(&mut code, 2, pair(R_SLEEP).0);
    store(&mut code, 0, pair(R_SLEEP).1);

    mov_r64_imm(&mut code, 0, DONE);
    store(&mut code, 0, 0x00);

    // Wait for the kernel's ACK before exiting: yield until it lands.
    let top = code.len();
    load(&mut code, 0, ACK_OFF); // rax = [r15 + ACK_OFF]
    mov_r64_imm(&mut code, 3, ACK); // rbx = ACK
    code.extend_from_slice(&[0x48, 0x39, 0xD8]); // cmp rax, rbx
    code.extend_from_slice(&[0x74, 0x09]); // je exit
    mov_eax(&mut code, Syscall::Yield as u32);
    syscall(&mut code);
    let back = top as i64 - (code.len() as i64 + 2);
    code.extend_from_slice(&[0xEB, back as i8 as u8]); // jmp top

    mov_eax(&mut code, Syscall::Exit as u32);
    code.extend_from_slice(&[0x31, 0xFF, 0x0F, 0x05]);
    code
}

/// Embeds `strings` after a short jump; returns their user addresses.
fn embed<const N: usize>(
    code: &mut alloc::vec::Vec<u8>,
    code_base: u64,
    strings: [&[u8]; N],
) -> [u64; N] {
    let total: usize = strings.iter().map(|s| s.len()).sum();
    code.push(0xEB);
    code.push(total as u8);
    let mut addrs = [0u64; N];
    let mut at = code_base + 2;
    for (i, s) in strings.iter().enumerate() {
        addrs[i] = at;
        code.extend_from_slice(s);
        at += s.len() as u64;
    }
    addrs
}

fn syscall(code: &mut alloc::vec::Vec<u8>) {
    code.extend_from_slice(&[0x0F, 0x05]);
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

/// `mov reg, [r15 + disp]`.
fn load(code: &mut alloc::vec::Vec<u8>, reg: u8, disp: i32) {
    let rex = 0x49 | (u8::from(reg >= 8) << 2);
    let modrm = 0x80 | ((reg & 7) << 3) | 7;
    code.extend_from_slice(&[rex, 0x8B, modrm]);
    code.extend_from_slice(&disp.to_le_bytes());
}
