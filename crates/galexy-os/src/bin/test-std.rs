//! Integration test: a real `std` program on the Galexy PAL (Milestone 70).
//!
//! Runs ramdisk program `stdtest` with session grants (console, keyboard,
//! loader, queries) so it can `Spawn` a child and read the `files`
//! snapshot. `stdtest` exercises `std::thread` + `Mutex` + `Condvar` +
//! `thread_local!`, `std::fs` on galfs, `std::process::Command`, `Instant`
//! / `SystemTime`, `env::args`, `HashMap`, and the unpatched `toml` crate,
//! and prints `stdtest: passed` with a check count. This kernel proves the
//! lifecycle around it: all threads gone, every frame back.

#![no_std]
#![no_main]

use bootloader_api::{entry_point, BootInfo};
use galexy_os::{drivers::screen, exit_qemu, println, sched, serial_println, QemuExitCode};
use x86_64::VirtAddr;

entry_point!(test_main_entry, config = &galexy_os::BOOTLOADER_CONFIG);

const PROGRAM: &str = "stdtest";

fn test_main_entry(boot_info: &'static mut BootInfo) -> ! {
    galexy_os::init();
    screen::init(boot_info);
    println!("[test-std] running");
    serial_println!("[test-std] running");

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
    // `Command::new("echo")` resolves through the kernel's ramdisk view.
    sched::ramdisk::init(archive);
    let elf = sched::ramdisk::find(PROGRAM)
        .unwrap_or_else(|| panic!("'{PROGRAM}' not found in the ramdisk tar"));
    assert!(
        sched::ramdisk::find("echo").is_some(),
        "echo missing from ramdisk"
    );

    let baseline = galexy_os::arch::mm::free_frames();
    let _region = sched::loader::spawn_program_session(PROGRAM, elf, b"").expect("stdtest elf");

    let mut polls = 0u64;
    loop {
        x86_64::instructions::hlt();
        sched::drain_spawn();
        sched::reap();
        if !sched::is_name_live(PROGRAM) {
            break;
        }
        polls += 1;
        if polls > 40_000 {
            panic!("stdtest never exited");
        }
    }

    let mut stable = 0u32;
    while stable < 16 {
        x86_64::instructions::hlt();
        sched::drain_spawn();
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
        "program, thread-stack, and child frames must return: baseline={baseline} now={now}"
    );

    println!("[test-std] std program lifecycle complete");
    serial_println!("[test-std] passed");
    exit_qemu(QemuExitCode::Success);
}
