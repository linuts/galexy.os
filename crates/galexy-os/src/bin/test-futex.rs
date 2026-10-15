//! Integration test: `Futex` wait/wake (Milestone 70).
//!
//! Runs ramdisk program `futex-test`: a 200-turn ping-pong between two
//! threads over one word (a lost wakeup hangs it, which the poll budget
//! here turns into a failure), the mismatch / empty-wake / timeout /
//! `BadBuffer` / `BadValue` edges, and a wake-all of three parked workers.
//! The program prints its verdict; this kernel proves the process and its
//! threads exit cleanly and give their frames back.

#![no_std]
#![no_main]

use bootloader_api::{entry_point, BootInfo};
use galexy_core::TarCursor;
use galexy_os::{drivers::screen, exit_qemu, println, sched, serial_println, QemuExitCode};
use x86_64::VirtAddr;

entry_point!(test_main_entry, config = &galexy_os::BOOTLOADER_CONFIG);

const PROGRAM: &str = "testing/futex-test";

fn test_main_entry(boot_info: &'static mut BootInfo) -> ! {
    galexy_os::init();
    screen::init(boot_info);
    println!("[test-futex] running");
    serial_println!("[test-futex] running");

    let Some(ramdisk_addr) = boot_info.ramdisk_addr.into_option() else {
        panic!("no ramdisk handed to the kernel");
    };
    let ramdisk_len = boot_info.ramdisk_len;

    galexy_os::arch::mm::init(boot_info);
    galexy_os::arch::init(boot_info);
    sched::init();

    let base = VirtAddr::new(ramdisk_addr);
    // SAFETY: bootloader-mapped ramdisk.
    let archive = unsafe { core::slice::from_raw_parts(base.as_ptr::<u8>(), ramdisk_len as usize) };
    let mut cursor = TarCursor::new(archive);
    let mut elf: Option<&[u8]> = None;
    while let Some((name, body)) = cursor.next_file() {
        if name == PROGRAM {
            elf = Some(body);
            break;
        }
    }
    let Some(elf) = elf else {
        panic!("'{PROGRAM}' not found in the ramdisk tar");
    };

    let baseline = galexy_os::arch::mm::free_frames();
    let _region = sched::loader::spawn_program(PROGRAM, elf).expect("futex-test elf");

    let mut polls = 0u64;
    loop {
        x86_64::instructions::hlt();
        sched::drain_spawn();
        sched::reap();
        if !sched::is_name_live(PROGRAM) {
            break;
        }
        polls += 1;
        if polls > 20_000 {
            panic!("futex-test never exited (lost wakeup?)");
        }
    }

    let mut stable = 0u32;
    while stable < 16 {
        x86_64::instructions::hlt();
        let before = galexy_os::arch::mm::free_frames();
        sched::reap();
        stable = if galexy_os::arch::mm::free_frames() == before {
            stable + 1
        } else {
            0
        };
    }
    assert_eq!(
        sched::unreaped_threads(),
        0,
        "no thread may outlive the process"
    );

    let now = galexy_os::arch::mm::free_frames();
    assert!(
        now >= baseline,
        "program and thread-stack frames must return: baseline={baseline} now={now}"
    );

    println!("[test-futex] futex lifecycle complete");
    serial_println!("[test-futex] passed");
    exit_qemu(QemuExitCode::Success);
}
