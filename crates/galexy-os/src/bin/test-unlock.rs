//! Volume unlock: wrong passphrase stays RAM-only; the real one remounts.
//!
//! Auto-unlock (test default) seals an empty image with [`VOLUME_PASSPHRASE`].
//! The test then wipes the key, rejects a guess, and unlocks again.

#![no_std]
#![no_main]

extern crate alloc;

use bootloader_api::{entry_point, BootInfo};
use galexy_abi::SysError;
use galexy_os::sched::galfs::{self, VOLUME_PASSPHRASE};
use galexy_os::{
    arch::mm, drivers::screen, exit_qemu, println, sched, serial_println, QemuExitCode,
};

entry_point!(test_main_entry, config = &galexy_os::BOOTLOADER_CONFIG);

fn test_main_entry(boot_info: &'static mut BootInfo) -> ! {
    galexy_os::init();
    screen::init(boot_info);
    println!("[test-unlock] running");
    serial_println!("[test-unlock] running");

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
    assert!(!galfs::volume_locked(), "auto-unlock should mount");
    assert!(!galfs::disk_corrupt());

    galfs::wipe_volume_key();
    assert!(galfs::volume_locked(), "wiped key locks a usable disk");
    assert!(!galfs::disk_corrupt());

    assert_eq!(
        galfs::unlock_volume(b"nope"),
        Err(SysError::AccessDenied),
        "wrong passphrase must not mount"
    );
    assert!(galfs::volume_locked());
    assert!(
        !galfs::disk_corrupt(),
        "a bad passphrase must not stick as corruption"
    );
    assert_eq!(galfs::sync_explicit(), Err(SysError::Unsupported));
    assert_eq!(
        galfs::verify_password("admin", b"admin"),
        Ok(true),
        "RAM admin still answers while the volume is locked"
    );

    assert_eq!(galfs::unlock_volume(VOLUME_PASSPHRASE), Ok(()));
    assert!(!galfs::volume_locked());
    assert!(galfs::sync_explicit().is_ok());

    println!("[test-unlock] remounted");
    serial_println!("[test-unlock] passed");
    exit_qemu(QemuExitCode::Success);
}
