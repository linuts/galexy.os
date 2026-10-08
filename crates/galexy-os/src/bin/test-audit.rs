//! Keyboard overflow drops, the dmesg ring, and the blink mark.

#![no_std]
#![no_main]

use bootloader_api::{entry_point, BootInfo};
use galexy_os::drivers::{dmesg, keyboard, screen};
use galexy_os::{exit_qemu, println, serial_println, QemuExitCode};

entry_point!(test_main_entry, config = &galexy_os::BOOTLOADER_CONFIG);

fn test_main_entry(boot_info: &'static mut BootInfo) -> ! {
    galexy_os::init();
    screen::init(boot_info);
    println!("[test-audit] running");
    serial_println!("[test-audit] running");

    let extra = 8usize;
    let dropped = keyboard::test_inject(0, keyboard::QUEUE_CAPACITY + extra);
    assert_eq!(
        dropped, extra as u64,
        "a full keyboard queue drops the newest characters"
    );
    assert!(
        keyboard::drops() >= extra as u64,
        "drop counter must record the overflow"
    );
    keyboard::test_drain(0);

    serial_println!("[test-audit] marker");
    serial_println!("[auth] login fail user=eve tty=1 fails=1");
    serial_println!("[auth] login fail user=eve tty=1 fails=2");
    serial_println!("[auth] login fail user=eve tty=1 fails=3");
    let mut buf = [0u8; 1024];
    let n = dmesg::snapshot(&mut buf);
    let text = core::str::from_utf8(&buf[..n]).expect("dmesg snapshot is utf-8");
    assert!(
        text.contains("[test-audit] marker"),
        "dmesg ring must keep the marker line"
    );
    assert_eq!(
        text.matches("login fail").count(),
        1,
        "consecutive login-fail lines collapse to one ring entry"
    );

    println!("X");
    assert!(
        screen::test_cursor_bar_lit(),
        "the shown cursor cell draws an underscore"
    );
    assert!(
        screen::test_cursor_bar_clear(),
        "clearing the blink restores the stored cell"
    );

    serial_println!("[test-audit] passed");
    exit_qemu(QemuExitCode::Success);
}
