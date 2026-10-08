//! Integration test: galfs block store (GALF v8+; pool fill uses current
//! [`galfs::FILE_BYTES`], including single-indirect on v11).
//!
//! Writes a multi-block file, fills the block pool to exhaustion, frees
//! on remove, and proves a block can be reused.

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
    println!("[test-blocks] running");
    serial_println!("[test-blocks] running");

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
    assert_eq!(galfs::blocks_used(), 0, "fresh table has no blocks");

    // Multi-block write: 3 × 512 + 100 = 1636 bytes.
    let file = galfs::create_file_under(desktop, "big").expect("create big");
    let mut pattern = [0u8; 1636];
    for (i, b) in pattern.iter_mut().enumerate() {
        *b = (i % 251) as u8;
    }
    let n = galfs::append_file(file, &pattern).expect("append big");
    assert_eq!(n, pattern.len(), "multi-block append must fit");
    assert_eq!(galfs::blocks_used(), 4, "1636 bytes use four blocks");

    let mut got = [0u8; 1636];
    let rn = galfs::read_file_bytes(file, &mut got).expect("read big");
    assert_eq!(rn, pattern.len());
    assert_eq!(&got[..], &pattern[..], "multi-block bytes round-trip");

    // Fill the pool: each file takes DIRECT_BLOCKS until blocks run out.
    let mut created = 0u32;
    let mut names = [[0u8; 8]; 64];
    loop {
        if created as usize >= names.len() {
            break;
        }
        let name = &mut names[created as usize];
        name[0] = b'f';
        name[1] = b'0' + ((created / 10) as u8);
        name[2] = b'0' + ((created % 10) as u8);
        let label = core::str::from_utf8(&name[..3]).unwrap();
        let Ok(fi) = galfs::create_file_under(desktop, label) else {
            break;
        };
        created += 1;
        let chunk = [0xA5u8; galfs::FILE_BYTES];
        let wrote = galfs::append_file(fi, &chunk).unwrap_or(0);
        if wrote < galfs::FILE_BYTES {
            // Block pool exhausted mid-file or at capacity.
            break;
        }
    }
    assert!(created >= 1, "must create at least one fill file");
    assert_eq!(
        galfs::blocks_used(),
        galfs::BLOCK_SLOTS,
        "block pool must be full"
    );

    // One more byte cannot allocate.
    let overflow = galfs::create_file_under(desktop, "overflow").expect("empty inode");
    assert_eq!(
        galfs::append_file(overflow, &[1]).unwrap_or(0),
        0,
        "full pool appends nothing"
    );

    // Remove the multi-block file and reuse its blocks.
    let before = galfs::blocks_used();
    galfs::remove_as_admin("Desktop/big").expect("remove big");
    assert_eq!(galfs::blocks_used(), before - 4, "remove frees four blocks");

    let reuse = galfs::create_file_under(desktop, "reuse").expect("reuse");
    let again = [0x5Au8; galfs::BLOCK_SIZE + 16];
    let wn = galfs::append_file(reuse, &again).expect("append reuse");
    assert_eq!(wn, again.len(), "freed blocks must be reusable");

    println!("[test-blocks] multi-block + pool fill/reuse ok");
    serial_println!("[test-blocks] passed");
    exit_qemu(QemuExitCode::Success);
}
