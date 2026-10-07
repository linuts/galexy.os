//! Integration test: create/remove after dual-slot recovery stays consistent.
//!
//! Boot 1: write `/Desktop/persist`, sync (dual slots hold the file).
//! Host corrupts the newest slot (runner `boot_with_galfs_recover`).
//! Boot 2: recover, verify bytes, repeat create/remove, sync; fsck stays ok.

#![no_std]
#![no_main]

extern crate alloc;

use bootloader_api::{entry_point, BootInfo};
use galexy_abi::SysError;
use galexy_os::sched::galfs;
use galexy_os::{
    arch::mm, drivers::screen, exit_qemu, println, sched, serial_println, QemuExitCode,
};

entry_point!(test_main_entry, config = &galexy_os::BOOTLOADER_CONFIG);

const PLAIN_TAG: &[u8] = b"idempotent-persist-ok";

fn payload() -> [u8; 64] {
    let mut buf = [0u8; 64];
    for (i, b) in buf.iter_mut().enumerate() {
        *b = PLAIN_TAG.get(i % PLAIN_TAG.len()).copied().unwrap_or(b'x');
    }
    buf
}

fn test_main_entry(boot_info: &'static mut BootInfo) -> ! {
    galexy_os::init();
    screen::init(boot_info);
    println!("[test-galfs-idempotent] running");
    serial_println!("[test-galfs-idempotent] running");

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

    assert!(galfs::disk_backed(), "ATA slave required");

    let admin = galfs::admin_root();
    let desktop = galfs::find_under(admin, "Desktop").expect("Desktop");
    let want = payload();

    if let Some(file) = galfs::find_under(desktop, "persist") {
        assert!(
            galfs::recoveries() >= 1,
            "verify boot must have recovered from a bad sibling"
        );
        let mut buf = [0u8; 64];
        let n = galfs::read_file_bytes(file, &mut buf).expect("read");
        assert_eq!(&buf[..n], &want[..], "recovered bytes");

        let (_slot, gen0) = galfs::disk_slot_info();

        // Same-name create after recover is a no-op error, not corruption.
        assert!(matches!(
            galfs::create_file_under(desktop, "persist"),
            Err(SysError::Unsupported)
        ));
        assert!(galfs::fsck_ok(), "after duplicate create");

        galfs::remove_as_admin("Desktop/persist").expect("remove");
        assert!(galfs::find_under(desktop, "persist").is_none());
        assert!(galfs::fsck_ok(), "after remove");

        let again = galfs::create_file_under(desktop, "persist").expect("recreate");
        let n = galfs::append_file(again, &want).expect("rewrite");
        assert_eq!(n, want.len());
        galfs::sync();
        galfs::sync(); // both slots carry the post-recover table
        assert!(galfs::fsck_ok(), "after recreate+sync");

        let (_slot2, gen1) = galfs::disk_slot_info();
        assert!(gen1 > gen0, "sync after recover must advance generation");

        println!("[test-galfs-idempotent] recover + recreate ok");
        serial_println!("[test-galfs-idempotent] passed");
        exit_qemu(QemuExitCode::Success);
    }

    let file = galfs::create_file_under(desktop, "persist").expect("persist");
    let n = galfs::append_file(file, &want).expect("append");
    assert_eq!(n, want.len());
    galfs::sync();
    galfs::sync();

    println!("[test-galfs-idempotent] wrote persist");
    serial_println!("[test-galfs-idempotent] wrote");
    exit_qemu(QemuExitCode::Success);
}
