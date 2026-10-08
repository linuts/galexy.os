//! Crash injection: commit `keep`, start a second mutate, get killed.
//!
//! The runner kills QEMU after `[test-crash] mutating` and before that
//! commit returns. The next boot must load a consistent slot that still
//! holds `keep` and does not hold `drop`.

#![no_std]
#![no_main]

extern crate alloc;

use bootloader_api::{entry_point, BootInfo};
use galexy_os::sched::galfs;
use galexy_os::{
    arch::mm, drivers::screen, exit_qemu, println, sched, serial_println, QemuExitCode,
};

entry_point!(test_main_entry, config = &galexy_os::BOOTLOADER_CONFIG);

const KEEP: &[u8] = b"crash-keep-ok";
const DROP: &[u8] = b"crash-drop-should-not-land";

fn test_main_entry(boot_info: &'static mut BootInfo) -> ! {
    galexy_os::init();
    screen::init(boot_info);
    println!("[test-crash] running");
    serial_println!("[test-crash] running");

    let Some(ramdisk_addr) = boot_info.ramdisk_addr.into_option() else {
        panic!("no ramdisk");
    };
    mm::init(boot_info);
    galexy_os::arch::init(boot_info);
    sched::init();
    let archive = unsafe {
        core::slice::from_raw_parts(
            x86_64::VirtAddr::new(ramdisk_addr).as_ptr::<u8>(),
            boot_info.ramdisk_len as usize,
        )
    };
    sched::ramdisk::init(archive);

    assert!(galfs::disk_backed(), "block device must back galfs");
    let admin = galfs::admin_root();
    let desktop = galfs::find_under(admin, "Desktop").expect("admin Desktop");

    if let Some(file) = galfs::find_under(desktop, "keep") {
        let mut buf = [0u8; 32];
        let n = galfs::read_file_bytes(file, &mut buf).expect("read keep");
        assert_eq!(&buf[..n], KEEP, "committed file must survive the crash");
        assert!(
            galfs::find_under(desktop, "drop").is_none(),
            "in-flight mutate must not land in the consistent slot"
        );
        assert!(galfs::fsck_ok(), "loaded slot must validate");
        println!("[test-crash] recovered consistent slot");
        serial_println!("[test-crash] passed");
        exit_qemu(QemuExitCode::Success);
    }

    let file = galfs::create_file_under(desktop, "keep").expect("keep");
    let n = galfs::append_file(file, KEEP).expect("append keep");
    assert_eq!(n, KEEP.len());
    // Both slots carry `keep` so a torn newer slot still has a sibling.
    galfs::sync();
    galfs::sync();
    serial_println!("[test-crash] armed");

    let drop_file = galfs::create_file_under(desktop, "drop").expect("drop");
    let _ = galfs::append_file(drop_file, DROP).expect("append drop");
    serial_println!("[test-crash] mutating");
    galfs::sync();
    // The runner should have killed us inside this commit.
    serial_println!("[test-crash] committed-drop");
    exit_qemu(QemuExitCode::Failed);
}
