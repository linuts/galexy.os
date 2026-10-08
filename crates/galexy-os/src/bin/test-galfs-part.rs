//! Integration test: galfs dual slots live past a partition LBA offset.
//!
//! Same persist flow as `test-galfs-disk`, but slot 0 starts at
//! [`galfs::DISK_PART_LBA`] (2048) so absolute LBA 0 stays unused.

#![no_std]
#![no_main]

extern crate alloc;

use bootloader_api::{entry_point, BootInfo};
use galexy_os::sched::galfs;
use galexy_os::{
    arch::mm, drivers::screen, exit_qemu, println, sched, serial_println, QemuExitCode,
};

entry_point!(test_main_entry, config = &galexy_os::BOOTLOADER_CONFIG);

const PLAIN_TAG: &[u8] = b"persist-ok-part-offset";

fn multi_block_payload() -> [u8; 600] {
    let mut buf = [0u8; 600];
    for (i, b) in buf.iter_mut().enumerate() {
        *b = PLAIN_TAG.get(i % PLAIN_TAG.len()).copied().unwrap_or(b'x');
    }
    buf
}

fn test_main_entry(boot_info: &'static mut BootInfo) -> ! {
    galexy_os::init();
    screen::init(boot_info);
    println!("[test-galfs-part] running");
    serial_println!("[test-galfs-part] running");

    let Some(ramdisk_addr) = boot_info.ramdisk_addr.into_option() else {
        panic!("no ramdisk");
    };
    mm::init(boot_info);
    galexy_os::arch::init(boot_info);
    galfs::set_disk_lba_base(galfs::DISK_PART_LBA);
    sched::init();
    let archive = unsafe {
        core::slice::from_raw_parts(
            x86_64::VirtAddr::new(ramdisk_addr).as_ptr::<u8>(),
            boot_info.ramdisk_len as usize,
        )
    };
    sched::ramdisk::init(archive);

    assert_eq!(galfs::disk_lba_base(), galfs::DISK_PART_LBA);
    assert!(
        galfs::disk_backed(),
        "block device must back galfs for this test"
    );
    let need =
        u64::from(galfs::DISK_PART_LBA) + (galfs::DISK_SECTORS * galfs::DISK_SLOT_COUNT) as u64;
    assert!(
        galfs::disk_capacity_sectors() >= need,
        "capacity must cover partition offset + dual slots"
    );

    let admin = galfs::admin_root();
    assert_ne!(admin, galfs::NO_OBJECT, "admin must exist");

    let desktop = galfs::find_under(admin, "Desktop").expect("admin Desktop");
    let want = multi_block_payload();
    if let Some(file) = galfs::find_under(desktop, "persist") {
        let mut buf = [0u8; 600];
        let n = galfs::read_file_bytes(file, &mut buf).expect("read persist");
        assert_eq!(&buf[..n], &want[..], "persist file must hold marker");
        println!("[test-galfs-part] loaded persist across reboot");
        serial_println!("[test-galfs-part] passed");
        exit_qemu(QemuExitCode::Success);
    }

    let file = galfs::create_file_under(desktop, "persist").expect("persist");
    let n = galfs::append_file(file, &want).expect("append");
    assert_eq!(n, want.len());
    galfs::sync();

    println!("[test-galfs-part] wrote persist past partition offset");
    serial_println!("[test-galfs-part] wrote");
    exit_qemu(QemuExitCode::Success);
}
