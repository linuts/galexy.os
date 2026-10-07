//! Integration test: both GALF slots corrupt → refuse silent format.

#![no_std]
#![no_main]

extern crate alloc;

use bootloader_api::{entry_point, BootInfo};
use galexy_os::sched::galfs;
use galexy_os::{
    arch::mm, drivers::screen, exit_qemu, println, sched, serial_println, QemuExitCode,
};

entry_point!(test_main_entry, config = &galexy_os::BOOTLOADER_CONFIG);

fn test_main_entry(boot_info: &'static mut BootInfo) -> ! {
    galexy_os::init();
    screen::init(boot_info);
    println!("[test-galfs-corrupt] running");
    serial_println!("[test-galfs-corrupt] running");

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
        galfs::disk_corrupt(),
        "both-bad image must mark the volume corrupt"
    );
    assert!(
        !galfs::disk_backed(),
        "corrupt volume must not mount as disk-backed"
    );
    assert_eq!(
        galfs::admin_root(),
        galfs::NO_OBJECT,
        "corrupt volume must not invent a RAM admin"
    );
    assert!(
        galfs::sync_explicit().is_err(),
        "sync must refuse a corrupt volume"
    );
    assert!(galfs::fsck_ok(), "empty table still structurally ok");

    println!("[test-galfs-corrupt] refused silent format");
    serial_println!("[test-galfs-corrupt] passed");
    exit_qemu(QemuExitCode::Success);
}
