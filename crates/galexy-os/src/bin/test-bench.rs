//! Milestone 64 measurements. Prints `[bench] name=<id> us=<n>` for
//! yield × 10 000, a user spawn + exit, 4 KiB through a pipe, a 32 KiB
//! galfs append + sync, and one full-screen repaint.

#![no_std]
#![no_main]

extern crate alloc;

use bootloader_api::{entry_point, BootInfo};
use galexy_abi::Syscall;
use galexy_os::sched::galfs;
use galexy_os::{drivers::screen, exit_qemu, println, sched, serial_println, QemuExitCode};

entry_point!(test_main_entry, config = &galexy_os::BOOTLOADER_CONFIG);

const YIELD_ITERS: u32 = 10_000;
const PIPE_BYTES: usize = 4096;

fn rdtsc() -> u64 {
    unsafe { core::arch::x86_64::_rdtsc() }
}

/// TSC cycles per microsecond, from a 20 ms halt against `timer_ticks`.
fn tsc_per_us() -> u64 {
    let t0 = galexy_os::arch::timer_ticks();
    let c0 = rdtsc();
    while galexy_os::arch::timer_ticks() < t0.saturating_add(20) {
        x86_64::instructions::interrupts::enable_and_hlt();
    }
    let dt = galexy_os::arch::timer_ticks().saturating_sub(t0).max(1);
    let dc = rdtsc().saturating_sub(c0).max(1);
    (dc / dt / 1000).max(1)
}

fn us_of(cycles: u64, per_us: u64) -> u64 {
    cycles / per_us.max(1)
}

fn report(name: &str, us: u64) {
    serial_println!("[bench] name={} us={}", name, us);
    println!("[bench] name={} us={}", name, us);
}

fn test_main_entry(boot_info: &'static mut BootInfo) -> ! {
    galexy_os::init();
    screen::init(boot_info);
    println!("[test-bench] running");
    serial_println!("[test-bench] running");

    galexy_os::arch::mm::init(boot_info);
    galexy_os::arch::init(boot_info);
    sched::init();

    let per_us = tsc_per_us();

    bench_yield(per_us);
    bench_spawn(per_us);
    bench_pipe(per_us);
    bench_galfs(per_us);
    bench_repaint(per_us);

    serial_println!("[test-bench] passed");
    exit_qemu(QemuExitCode::Success);
}

fn bench_yield(per_us: u64) {
    let (region, _) = sched::spawn_user_task("bench-yield", |gr| {
        let mut code: alloc::vec::Vec<u8> = alloc::vec::Vec::new();
        // rdtsc; shl rdx, 32; or rax, rdx; mov r12, rax
        // r12 survives SYSCALL (the entry saves rbx and r12..=r15). rcx does not:
        // the instruction itself overwrites it with the return RIP.
        code.extend_from_slice(&[
            0x0F, 0x31, 0x48, 0xC1, 0xE2, 0x20, 0x48, 0x09, 0xD0, 0x49, 0x89, 0xC4,
        ]);
        // mov r13d, 10000
        code.extend_from_slice(&[0x41, 0xBD]);
        code.extend_from_slice(&YIELD_ITERS.to_le_bytes());
        // mov eax, YIELD; syscall; dec r13d; jnz -12
        code.extend_from_slice(&[
            0xB8,
            Syscall::Yield as u8,
            0x00,
            0x00,
            0x00,
            0x0F,
            0x05,
            0x41,
            0xFF,
            0xCD,
            0x75,
            0xF4,
        ]);
        // rdtsc; pack; sub rax, r12; mov rdi, scratch; mov [rdi], rax; jmp $
        code.extend_from_slice(&[
            0x0F, 0x31, 0x48, 0xC1, 0xE2, 0x20, 0x48, 0x09, 0xD0, 0x4C, 0x29, 0xE0,
        ]);
        code.extend_from_slice(&[0x48, 0xBF]);
        code.extend_from_slice(&gr.scratch.as_u64().to_le_bytes());
        code.extend_from_slice(&[0x48, 0x89, 0x07, 0xEB, 0xFE]);
        code
    });
    let scratch = galexy_os::arch::mm::frame_virt(region.scratch_phys).as_ptr::<u64>();
    let mut spins = 0u64;
    let cycles = loop {
        x86_64::instructions::hlt();
        spins += 1;
        // SAFETY: the blob stores one u64 into its scratch page, then spins.
        let v = unsafe { core::ptr::read_volatile(scratch) };
        if v != 0 {
            break v;
        }
        if spins > 100_000 {
            panic!("yield bench never reported");
        }
    };
    report("yield", us_of(cycles, per_us));
}

fn bench_spawn(per_us: u64) {
    let start = rdtsc();
    let _ = sched::spawn_user_task("bench-exit", |_gr| {
        alloc::vec![
            0xB8,
            Syscall::Exit as u8,
            0x00,
            0x00,
            0x00,
            0x31,
            0xFF,
            0x0F,
            0x05,
            0xEB,
            0xFE,
        ]
    });
    let mut spins = 0u64;
    while sched::is_name_live("bench-exit") {
        x86_64::instructions::hlt();
        spins += 1;
        if spins > 100_000 {
            panic!("spawn bench never exited");
        }
    }
    sched::reap();
    report("spawn", us_of(rdtsc().saturating_sub(start), per_us));
}

fn bench_pipe(per_us: u64) {
    let id = sched::pipe::alloc().expect("pipe");
    let chunk = [0xABu8; 256];
    let mut out = [0u8; 256];
    let start = rdtsc();
    let mut moved = 0usize;
    while moved < PIPE_BYTES {
        let want = (PIPE_BYTES - moved).min(chunk.len());
        let n = sched::pipe::write(id, &chunk[..want]).expect("pipe write");
        if n == 0 {
            panic!("pipe write blocked in the bench");
        }
        let got = sched::pipe::read(id, &mut out[..n]).expect("pipe read");
        if got != n {
            panic!("pipe short read {got} != {n}");
        }
        moved += got;
    }
    let us = us_of(rdtsc().saturating_sub(start), per_us);
    sched::pipe::close_end(id, sched::pipe::PipeEnd::Read);
    sched::pipe::close_end(id, sched::pipe::PipeEnd::Write);
    report("pipe", us);
}

fn bench_galfs(per_us: u64) {
    let admin = galfs::admin_root();
    let desktop = galfs::find_under(admin, "Desktop").expect("Desktop");
    let file = galfs::create_file_under(desktop, "bench").expect("create");
    static PATTERN: [u8; 32 * 1024] = [0x5A; 32 * 1024];
    let start = rdtsc();
    let n = galfs::append_file(file, &PATTERN).expect("append");
    if n != PATTERN.len() {
        panic!("append wrote {n}");
    }
    galfs::sync_explicit().expect("sync");
    report("galfs", us_of(rdtsc().saturating_sub(start), per_us));
}

fn bench_repaint(per_us: u64) {
    screen::fill_shown_for_bench();
    let start = rdtsc();
    screen::repaint_shown();
    report("repaint", us_of(rdtsc().saturating_sub(start), per_us));
}
