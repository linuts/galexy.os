//! Integration test: galfs persists across two QEMU boots on the ATA slave.
//!
//! Boot 1 (empty disk): format, write `/Desktop/persist`, exit with `wrote`.
//! Boot 2 (same image): load, verify the file bytes, exit with `passed`.

#![no_std]
#![no_main]

extern crate alloc;

use bootloader_api::{entry_point, BootInfo};
use galexy_os::sched::galfs;
use galexy_os::{
    arch::mm, drivers::screen, exit_qemu, println, sched, serial_println, QemuExitCode,
};

entry_point!(test_main_entry, config = &galexy_os::BOOTLOADER_CONFIG);

const MARKER: &[u8] = b"persist-ok";

fn test_main_entry(boot_info: &'static mut BootInfo) -> ! {
    galexy_os::init();
    screen::init(boot_info);
    println!("[test-galfs-disk] running");
    serial_println!("[test-galfs-disk] running");

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

    assert!(
        galfs::disk_backed(),
        "ATA slave must back galfs for this test"
    );

    let alex = galfs::alex_root();
    assert_ne!(alex, galfs::NO_OBJECT, "alex must exist");

    if let Some(desktop) = galfs::find_under(alex, "Desktop") {
        if let Some(file) = galfs::find_under(desktop, "persist") {
            let mut buf = [0u8; 32];
            let n = galfs::read_file_bytes(file, &mut buf).expect("read persist");
            assert_eq!(&buf[..n], MARKER, "persist file must hold {MARKER:?}");
            println!("[test-galfs-disk] loaded persist across reboot");
            serial_println!("[test-galfs-disk] passed");
            exit_qemu(QemuExitCode::Success);
        }
    }

    let desktop = galfs::mkdir_under_root(alex, "Desktop").expect("Desktop");
    let file = galfs::create_file_under(desktop, "persist").expect("persist");
    let n = galfs::append_file(file, MARKER).expect("append");
    assert_eq!(n, MARKER.len());
    galfs::sync();

    println!("[test-galfs-disk] wrote persist to disk");
    serial_println!("[test-galfs-disk] wrote");
    exit_qemu(QemuExitCode::Success);
}
