//! Integration test: extent-mapped file blocks (more than one 4 KiB block).

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
    println!("[test-indirect] running");
    serial_println!("[test-indirect] running");

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

    let admin = galfs::admin_root();
    let desktop = galfs::find_under(admin, "Desktop").expect("Desktop");
    assert!(galfs::fsck_ok(), "fresh table");

    const WIDE: usize = galfs::BLOCK_SIZE + 200;
    let file = galfs::create_file_under(desktop, "wide").expect("create wide");
    let mut pattern = [0u8; WIDE];
    for (i, b) in pattern.iter_mut().enumerate() {
        *b = (i % 251) as u8;
    }
    let n = galfs::append_file(file, &pattern).expect("append two blocks");
    assert_eq!(n, WIDE);
    assert_eq!(galfs::blocks_used(), 2, "two extents, no pointer block");
    assert!(galfs::fsck_ok(), "after multi-block append");

    let mut got = [0u8; WIDE];
    let rn = galfs::read_file_bytes(file, &mut got).expect("read wide");
    assert_eq!(rn, WIDE);
    assert_eq!(&got[..], &pattern[..], "extent bytes round-trip");

    galfs::truncate_file(file, 100).expect("truncate into first block");
    assert_eq!(galfs::blocks_used(), 1, "second block freed");
    assert!(galfs::fsck_ok(), "after truncate shrink");

    galfs::truncate_file(file, WIDE).expect("truncate grow");
    assert_eq!(galfs::blocks_used(), 2, "grow reallocates the second block");
    let mut buf = [0u8; WIDE];
    let zn = galfs::read_file_bytes(file, &mut buf).expect("read grown");
    assert_eq!(zn, WIDE);
    assert_eq!(&buf[..100], &pattern[..100], "prefix survives shrink+grow");
    assert!(
        buf[100..].iter().all(|&b| b == 0),
        "truncate grow zero-fills"
    );

    let full = galfs::create_file_under(desktop, "full").expect("full");
    let chunk = [0xA5u8; galfs::BLOCK_SIZE];
    for _ in 0..8 {
        assert_eq!(
            galfs::append_file(full, &chunk).expect("append"),
            chunk.len()
        );
    }
    assert_eq!(galfs::blocks_used(), 2 + 8);
    assert!(galfs::fsck_ok(), "after 32 KiB file");

    galfs::remove_as_admin("Desktop/full").expect("remove full");
    assert_eq!(galfs::blocks_used(), 2, "remove frees the file");
    assert!(galfs::fsck_ok(), "after remove");

    println!("[test-indirect] extent file ok");
    serial_println!("[test-indirect] passed");
    exit_qemu(QemuExitCode::Success);
}
