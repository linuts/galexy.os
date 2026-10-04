//! Integration test kernel: boots the full stack and asserts kernel-side
//! behavior that needs real hardware (QEMU), then exits QEMU with Success.

#![no_std]
#![no_main]

use bootloader_api::{entry_point, BootInfo};
use galexy_core::Ring;
use galexy_os::{drivers::screen, exit_qemu, println, serial_println, QemuExitCode};

entry_point!(test_main_entry, config = &galexy_os::BOOTLOADER_CONFIG);

fn test_main_entry(boot_info: &'static mut BootInfo) -> ! {
    galexy_os::init();
    screen::init(boot_info);
    println!("[test-basic] running");
    serial_println!("[test-basic] running");

    // Kernel-side sanity of galexy-core's Ring (host unit tests cover more).
    let mut ring: Ring<u8, 3> = Ring::new();
    assert_eq!(ring.pop(), None, "fresh ring pops empty");
    let _ = ring.push(1);
    let _ = ring.push(2);
    let _ = ring.push(3);
    assert_eq!(ring.push(4), None, "overflow drops newest");
    assert_eq!(ring.pop(), Some(1), "fifo order");
    assert_eq!(ring.pop(), Some(2), "fifo order");
    assert_eq!(ring.pop(), Some(3), "fifo order");
    assert_eq!(ring.pop(), None, "emptied ring pops empty");

    // Screen was initialized and prints without faulting (visible output).
    println!("[test-basic] all assertions passed");

    serial_println!("[test-basic] passed");
    exit_qemu(QemuExitCode::Success);
}
