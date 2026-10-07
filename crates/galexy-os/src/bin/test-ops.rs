//! Integration test: galfs rename / truncate / stat (Milestone 45 ops).

#![no_std]
#![no_main]

extern crate alloc;

use bootloader_api::{entry_point, BootInfo};
use galexy_abi::{STAT_DIR, STAT_FILE, STAT_LEN, TOKEN_ALL};
use galexy_os::sched::galfs;
use galexy_os::{
    arch::mm, drivers::screen, exit_qemu, println, sched, serial_println, QemuExitCode,
};

entry_point!(test_main_entry, config = &galexy_os::BOOTLOADER_CONFIG);

fn test_main_entry(boot_info: &'static mut BootInfo) -> ! {
    galexy_os::init();
    screen::init(boot_info);
    println!("[test-ops] running");
    serial_println!("[test-ops] running");

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
    let file = galfs::create_file_under(desktop, "note").expect("note");
    let n = galfs::append_file(file, b"hello-ops").expect("append");
    assert_eq!(n, 9);

    galfs::truncate_file(file, 5).expect("shrink");
    let mut buf = [0u8; 16];
    let rn = galfs::read_file_bytes(file, &mut buf).expect("read");
    assert_eq!(&buf[..rn], b"hello");
    galfs::truncate_file(file, 12).expect("grow");
    let rn = galfs::read_file_bytes(file, &mut buf).expect("read grown");
    assert_eq!(rn, 12);
    assert_eq!(&buf[..5], b"hello");
    assert_eq!(&buf[5..12], &[0; 7]);

    galfs::rename_as_admin("Desktop/note", "Desktop/moved").expect("rename");
    assert!(galfs::find_under(desktop, "note").is_none());
    let moved = galfs::find_under(desktop, "moved").expect("moved");
    assert_eq!(moved, file);

    galfs::mkdir_under_root(admin, "box").expect("box");
    galfs::rename_as_admin("Desktop/moved", "box/inside").expect("cross rename");
    let box_dir = galfs::find_under(admin, "box").expect("box dir");
    assert!(galfs::find_under(desktop, "moved").is_none());
    assert_eq!(
        galfs::find_under(box_dir, "inside").expect("inside"),
        file
    );

    let mut st = [0u8; STAT_LEN];
    let sn = galfs::stat_as_admin("box/inside", &mut st).expect("stat file");
    assert_eq!(sn, STAT_LEN);
    assert_eq!(st[0], STAT_FILE);
    assert_eq!(st[1], TOKEN_ALL as u8);
    let size = u32::from_le_bytes([st[4], st[5], st[6], st[7]]);
    assert_eq!(size, 12);
    assert_eq!(st[8], 5);
    assert_eq!(&st[9..14], b"admin");

    galfs::stat_as_admin("box", &mut st).expect("stat dir");
    assert_eq!(st[0], STAT_DIR);
    assert_eq!(u32::from_le_bytes([st[4], st[5], st[6], st[7]]), 0);

    assert!(matches!(
        galfs::rename_as_admin("box", "box/nested"),
        Err(galexy_abi::SysError::BadValue)
    ));

    println!("[test-ops] rename/truncate/stat ok");
    serial_println!("[test-ops] passed");
    exit_qemu(QemuExitCode::Success);
}
