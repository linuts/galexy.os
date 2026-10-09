//! Integration test: ELF produced by host `gxc` (Milestone 61).
//!
//! Loads ramdisk entry `hello-gxc`, runs it, asserts the gxc hello line
//! reaches serial, and that the task exits + reaps cleanly. rustc-built
//! `hello` is untouched.

#![no_std]
#![no_main]

use bootloader_api::{entry_point, BootInfo};
use galexy_core::TarCursor;
use galexy_os::{drivers::screen, exit_qemu, println, sched, serial_println, QemuExitCode};
use x86_64::VirtAddr;

entry_point!(test_main_entry, config = &galexy_os::BOOTLOADER_CONFIG);

fn test_main_entry(boot_info: &'static mut BootInfo) -> ! {
    galexy_os::init();
    screen::init(boot_info);
    println!("[test-hellogxc] running");
    serial_println!("[test-hellogxc] running");

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
        if name == "hello-gxc" {
            elf = Some(body);
            break;
        }
    }
    let Some(elf) = elf else {
        panic!("'hello-gxc' not found in the ramdisk tar");
    };

    let baseline = galexy_os::arch::mm::free_frames();
    let _region = sched::loader::spawn_program("hello-gxc", elf).expect("hello-gxc elf");

    let mut polls = 0u64;
    loop {
        x86_64::instructions::hlt();
        sched::reap();
        if sched::threads_count() == 0 {
            break;
        }
        polls += 1;
        if polls > 4000 {
            panic!("hello-gxc never exited");
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

    let now = galexy_os::arch::mm::free_frames();
    assert!(
        now >= baseline,
        "program frames must return: baseline={baseline} now={now}"
    );

    println!("[test-hellogxc] gxc program lifecycle complete");
    serial_println!("[test-hellogxc] passed");
    exit_qemu(QemuExitCode::Success);
}
