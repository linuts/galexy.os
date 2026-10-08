//! Integration test: single-indirect file blocks (GALF v11).

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

    assert_eq!(galfs::FILE_BYTES, 32 * 1024);
    const _: () = assert!(galfs::FILE_BYTES > galfs::DIRECT_BLOCKS * galfs::BLOCK_SIZE);

    let admin = galfs::admin_root();
    let desktop = galfs::find_under(admin, "Desktop").expect("Desktop");
    assert!(galfs::fsck_ok(), "fresh table");

    // Past the 8 direct blocks: 8×512 + 200 = 4296 bytes.
    const PAST_DIRECT: usize = galfs::DIRECT_BLOCKS * galfs::BLOCK_SIZE + 200;
    let file = galfs::create_file_under(desktop, "wide").expect("create wide");
    let mut pattern = [0u8; PAST_DIRECT];
    for (i, b) in pattern.iter_mut().enumerate() {
        *b = (i % 251) as u8;
    }
    let n = galfs::append_file(file, &pattern).expect("append past direct");
    assert_eq!(n, PAST_DIRECT, "must cross into indirect");
    // 9 data blocks + 1 indirect block.
    assert_eq!(galfs::blocks_used(), 10, "indirect adds one pointer block");
    assert!(galfs::fsck_ok(), "after indirect append");

    let mut got = [0u8; PAST_DIRECT];
    let rn = galfs::read_file_bytes(file, &mut got).expect("read wide");
    assert_eq!(rn, PAST_DIRECT);
    assert_eq!(&got[..], &pattern[..], "indirect bytes round-trip");

    // Truncate back into the direct region frees the indirect block.
    galfs::truncate_file(file, 100).expect("truncate into directs");
    assert_eq!(galfs::blocks_used(), 1, "indirect + unused data freed");
    assert!(galfs::fsck_ok(), "after truncate shrink");

    // Grow through truncate into indirect again (new bytes are zero-filled).
    galfs::truncate_file(file, PAST_DIRECT).expect("truncate grow past direct");
    assert_eq!(galfs::blocks_used(), 10, "grow reallocates indirect");
    let mut buf = [0u8; PAST_DIRECT];
    let zn = galfs::read_file_bytes(file, &mut buf).expect("read grown");
    assert_eq!(zn, PAST_DIRECT);
    assert_eq!(&buf[..100], &pattern[..100], "prefix survives shrink+grow");
    assert!(
        buf[100..].iter().all(|&b| b == 0),
        "truncate grow zero-fills the new region"
    );

    // Full 32 KiB file.
    let full = galfs::create_file_under(desktop, "full").expect("full");
    let chunk = [0xA5u8; galfs::FILE_BYTES];
    let wn = galfs::append_file(full, &chunk).expect("append full");
    assert_eq!(wn, galfs::FILE_BYTES);
    // 64 data + 1 indirect.
    assert_eq!(galfs::blocks_used(), 10 + 65);
    assert!(galfs::fsck_ok(), "after 32 KiB file");

    galfs::remove_as_admin("Desktop/full").expect("remove full");
    assert_eq!(galfs::blocks_used(), 10, "remove frees indirect file");
    assert!(galfs::fsck_ok(), "after remove");

    println!("[test-indirect] single-indirect 32 KiB ok");
    serial_println!("[test-indirect] passed");
    exit_qemu(QemuExitCode::Success);
}
