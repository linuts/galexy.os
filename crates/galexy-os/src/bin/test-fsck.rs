//! Integration test: live-table fsck after create/remove/truncate.

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
    println!("[test-fsck] running");
    serial_println!("[test-fsck] running");

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

    assert!(galfs::fsck_ok(), "fresh table must pass fsck");
    let admin = galfs::admin_root();
    let desktop = galfs::find_under(admin, "Desktop").expect("Desktop");
    let f = galfs::create_file_under(desktop, "a").expect("a");
    assert_eq!(galfs::append_file(f, &[1; 600]).unwrap(), 600);
    assert!(galfs::fsck_ok(), "after multi-block write");
    galfs::truncate_file(f, 10).expect("truncate");
    assert!(galfs::fsck_ok(), "after truncate");
    galfs::remove_as_admin("Desktop/a").expect("rm");
    assert_eq!(galfs::blocks_used(), 0);
    assert!(galfs::fsck_ok(), "after remove");

    // Path policy smoke.
    assert!(matches!(
        galfs::create_file_under(desktop, ".."),
        Err(galexy_abi::SysError::BadValue)
    ));
    assert!(matches!(
        galfs::create_file_under(desktop, "bad/name"),
        Err(galexy_abi::SysError::BadValue)
    ));

    println!("[test-fsck] live table consistent");
    serial_println!("[test-fsck] passed");
    exit_qemu(QemuExitCode::Success);
}
