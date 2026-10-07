//! Integration test: galfs path policy (charset, depth, `.`/`..`, junk).

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

fn bad_stat(path: &str) {
    let mut st = [0u8; galexy_abi::STAT_LEN];
    assert!(
        matches!(galfs::stat_as_admin(path, &mut st), Err(SysError::BadValue)),
        "expected BadValue for {path:?}"
    );
}

fn bad_create(parent: u16, name: &str) {
    assert!(
        matches!(
            galfs::create_file_under(parent, name),
            Err(SysError::BadValue)
        ),
        "expected BadValue create {name:?}"
    );
}

fn test_main_entry(boot_info: &'static mut BootInfo) -> ! {
    galexy_os::init();
    screen::init(boot_info);
    println!("[test-paths] running");
    serial_println!("[test-paths] running");

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

    assert_eq!(galfs::MAX_DEPTH, 8);

    let admin = galfs::admin_root();
    let desktop = galfs::find_under(admin, "Desktop").expect("Desktop");

    // Dot components — byte names only; no path walking of `.` / `..`.
    bad_create(desktop, ".");
    bad_create(desktop, "..");
    bad_stat(".");
    bad_stat("..");
    bad_stat("Desktop/.");
    bad_stat("Desktop/..");
    bad_stat("Desktop/../x");
    bad_stat("./Desktop");

    // Charset: alphanumeric + `.` `_` `-` only (documented byte policy).
    bad_create(desktop, "has space");
    bad_create(desktop, "bang!");
    bad_stat("Desktop/has space");
    bad_stat("Desktop/caf\u{00e9}");
    // Embedded NUL is valid UTF-8 but not a legal component byte.
    let with_nul = core::str::from_utf8(b"Desktop/\0x").unwrap();
    bad_stat(with_nul);
    bad_create(desktop, core::str::from_utf8(b"a\0b").unwrap());

    // Overlong component (> 64).
    let ok64 = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    assert_eq!(ok64.len(), 64);
    galfs::create_file_under(desktop, ok64).expect("64-char name ok");
    let long65 = [b'b'; 65];
    let long65 = core::str::from_utf8(&long65).unwrap();
    bad_create(desktop, long65);

    // Empty / doubled separators and junk owner@ shapes.
    bad_stat("Desktop//x");
    bad_stat("Desktop//");
    bad_stat("/");
    bad_stat("");
    bad_stat("@Desktop");
    bad_stat("admin@@Desktop");
    bad_stat("Desktop/foo@bar");
    bad_stat("admin@");
    // `admin@/` is the actor-root form (directory); bare `admin@` is not.
    // Trailing slash on a real dir (`Desktop/`) is allowed.
    let mut st = [0u8; galexy_abi::STAT_LEN];
    galfs::stat_as_admin("Desktop/", &mut st).expect("dir trailing slash");
    galfs::stat_as_admin("admin@/", &mut st).expect("actor root path");

    // Max depth: 8 components ok; 9th rejected at parse.
    let mut parent = desktop;
    let names = ["d0", "d1", "d2", "d3", "d4", "d5"];
    for name in names {
        parent = galfs::mkdir_under_root(parent, name).expect("nest");
    }
    // Desktop + 6 dirs + leaf = 8 components.
    galfs::create_file_under(parent, "leaf").expect("depth-8 leaf");
    let deep_path = "Desktop/d0/d1/d2/d3/d4/d5/leaf";
    galfs::stat_as_admin(deep_path, &mut st).expect("depth 8 ok");
    bad_stat("Desktop/d0/d1/d2/d3/d4/d5/leaf/too");

    // rename refuses nesting a name into a path (existing ops smoke).
    assert!(matches!(
        galfs::rename_as_admin("Desktop", "Desktop/nested"),
        Err(SysError::BadValue)
    ));

    println!("[test-paths] charset depth and dots rejected");
    serial_println!("[test-paths] passed");
    exit_qemu(QemuExitCode::Success);
}
