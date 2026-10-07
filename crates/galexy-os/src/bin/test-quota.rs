//! Integration test: per-actor object and byte quotas (GALF v9).

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

fn test_main_entry(boot_info: &'static mut BootInfo) -> ! {
    galexy_os::init();
    screen::init(boot_info);
    println!("[test-quota] running");
    serial_println!("[test-quota] running");

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
    let (ao, am, ab, ax) = galfs::root_quota(admin).expect("admin quota");
    assert!(am >= ao, "admin object headroom");
    assert!(ax >= ab, "admin byte headroom");

    galfs::add_user("dan", b"dan-pass").expect("add dan");
    let (uo, um, ub, ux) = galfs::actor_quota("dan").expect("dan quota");
    assert_eq!(uo, 2, "root + Desktop");
    assert_eq!(um, galfs::DEFAULT_MAX_OBJECTS as u32);
    assert_eq!(ub, 0);
    assert_eq!(ux, galfs::DEFAULT_MAX_BYTES);

    galfs::set_actor_quota("dan", 3, 100).expect("tighten dan");
    let dan = galfs::root_named("dan").expect("dan root");
    let desk = galfs::find_under(dan, "Desktop").expect("dan Desktop");
    let f = galfs::create_file_under(desk, "a").expect("one file under object quota");
    assert!(matches!(
        galfs::create_file_under(desk, "b"),
        Err(SysError::NoResource)
    ));

    let n = galfs::append_file(f, &[b'x'; 80]).expect("append under byte quota");
    assert_eq!(n, 80);
    let n2 = galfs::append_file(f, &[b'y'; 40]).expect("short append at byte cap");
    assert_eq!(n2, 20, "byte quota must stop at 100");
    assert!(matches!(
        galfs::truncate_file(f, 200),
        Err(SysError::NoResource)
    ));

    galfs::set_actor_quota("dan", 8, 4096).expect("raise dan");
    galfs::create_file_under(desk, "b").expect("after raise");
    let (uo2, um2, ub2, _) = galfs::actor_quota("dan").expect("dan after");
    assert_eq!(uo2, 4);
    assert_eq!(um2, 8);
    assert_eq!(ub2, 100);

    println!("[test-quota] object and byte limits enforced");
    serial_println!("[test-quota] passed");
    exit_qemu(QemuExitCode::Success);
}
