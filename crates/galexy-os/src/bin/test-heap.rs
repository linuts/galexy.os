//! Integration test kernel: exercises the kernel heap (alloc) end to end —
//! Box, Vec, String, drop-and-reuse — after booting the full stack.

#![no_std]
#![no_main]

extern crate alloc;

use alloc::{boxed::Box, string::String, vec::Vec};
use bootloader_api::{entry_point, BootInfo};
use galexy_os::{drivers::screen, exit_qemu, println, serial_println, QemuExitCode};

entry_point!(test_main_entry, config = &galexy_os::BOOTLOADER_CONFIG);

fn test_main_entry(boot_info: &'static mut BootInfo) -> ! {
    galexy_os::init();
    screen::init(boot_info);
    println!("[test-heap] running");
    serial_println!("[test-heap] running");

    galexy_os::arch::init(); // interrupts + IDT before memory work
    galexy_os::arch::mm::init(boot_info); // frames + paging + heap

    // Box roundtrip.
    let boxed = Box::new(0x4142_4344_4546_4748u64);
    assert_eq!(*boxed, 0x4142_4344_4546_4748, "Box roundtrip");
    drop(boxed);

    // Vec growth well past initial capacity.
    let mut vec: Vec<u64> = Vec::new();
    for i in 0..1000u64 {
        vec.push(i * 3);
    }
    assert_eq!(vec.len(), 1000, "Vec length");
    assert_eq!(vec[999], 999 * 3, "Vec contents");
    drop(vec);

    // String with multibyte chars.
    let mut text = String::new();
    text.push_str("galexy.os — ");
    text.push('✓');
    assert_eq!(text.chars().count(), 13, "String content");
    drop(text);

    // Drop-and-reuse: after dropping everything, fresh allocations must
    // succeed (linked list reclaimed the blocks).
    let boxed2 = Box::new(0x1000u64);
    assert_eq!(*boxed2, 0x1000, "Box after drops");
    drop(boxed2);

    println!("[test-heap] all allocations passed");
    serial_println!("[test-heap] passed");
    exit_qemu(QemuExitCode::Success);
}
