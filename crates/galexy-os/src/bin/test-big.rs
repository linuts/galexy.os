//! Milestone 71: a `std` program allocates 300 MiB and round-trips a 20 MiB file.
//!
//! The boot suite runs this only under KVM. Mapping hundreds of megabytes
//! a page at a time is not a TCG test.

#![no_std]
#![no_main]

use bootloader_api::{entry_point, BootInfo};
use galexy_os::{drivers::screen, exit_qemu, println, sched, serial_println, QemuExitCode};
use x86_64::VirtAddr;

entry_point!(test_main_entry, config = &galexy_os::BOOTLOADER_CONFIG);

const PROGRAM: &str = "bigtest";

fn test_main_entry(boot_info: &'static mut BootInfo) -> ! {
    galexy_os::init();
    screen::init(boot_info);
    println!("[test-big] running");
    serial_println!("[test-big] running");

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
    sched::ramdisk::init(archive);
    let elf = sched::ramdisk::find(PROGRAM)
        .unwrap_or_else(|| panic!("'{PROGRAM}' not found in the ramdisk tar"));

    let baseline = galexy_os::arch::mm::free_frames();
    let _region = sched::loader::spawn_program_session(PROGRAM, elf, b"").expect("bigtest elf");

    let mut polls = 0u64;
    loop {
        x86_64::instructions::hlt();
        sched::drain_spawn();
        sched::drain_thread_spawn();
        sched::reap();
        if !sched::is_name_live(PROGRAM) {
            break;
        }
        polls += 1;
        if polls > 2_000_000 {
            panic!("bigtest never exited");
        }
    }

    let mut stable = 0u32;
    while stable < 16 {
        x86_64::instructions::hlt();
        sched::drain_spawn();
        sched::drain_thread_spawn();
        let before = galexy_os::arch::mm::free_frames();
        sched::reap();
        stable = if galexy_os::arch::mm::free_frames() == before {
            stable + 1
        } else {
            0
        };
    }
    assert_eq!(sched::user_exit_code(PROGRAM), Some(0), "bigtest failed");
    assert_eq!(
        sched::unreaped_threads(),
        0,
        "no thread may outlive the process"
    );

    // The 20 MiB file stays in the kernel large-file pool (the heap does
    // not shrink). The 300 MiB user heap must have come back.
    let now = galexy_os::arch::mm::free_frames();
    let leaked = baseline.saturating_sub(now);
    assert!(
        leaked < 8_000,
        "user heap frames must return (leaked {leaked}, baseline {baseline}, now {now})"
    );

    println!("[test-big] 300 MiB map and 20 MiB file reclaimed");
    serial_println!("[test-big] passed");
    exit_qemu(QemuExitCode::Success);
}
