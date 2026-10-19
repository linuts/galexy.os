//! Ring-3 `gxld` links two ramdisk objects given as Caps and the image runs.
//!
//! The program does not open a path. This kernel creates the galfs files,
//! installs the Caps before the child is released, then loads the output.

#![no_std]
#![no_main]

extern crate alloc;

use bootloader_api::{entry_point, BootInfo};
use galexy_abi::CapRights;
use galexy_os::{drivers::screen, exit_qemu, println, sched, serial_println, QemuExitCode};
use x86_64::VirtAddr;

entry_point!(test_main_entry, config = &galexy_os::BOOTLOADER_CONFIG);

fn test_main_entry(boot_info: &'static mut BootInfo) -> ! {
    galexy_os::init();
    screen::init(boot_info);
    println!("[test-gxld] running");
    serial_println!("[test-gxld] running");

    let Some(ramdisk_addr) = boot_info.ramdisk_addr.into_option() else {
        panic!("no ramdisk handed to the kernel");
    };
    let ramdisk_len = boot_info.ramdisk_len;

    galexy_os::arch::mm::init(boot_info);
    galexy_os::arch::init(boot_info);
    sched::init();

    let base = VirtAddr::new(ramdisk_addr);
    // SAFETY: bootloader-mapped ramdisk.
    let archive = unsafe { core::slice::from_raw_parts(base.as_ptr::<u8>(), ramdisk_len as usize) };
    sched::ramdisk::init(archive);

    let gxld = sched::ramdisk::find("gxldos").expect("gxldos missing");
    let obj_a = sched::ramdisk::find("testing/a.o").expect("testing/a.o missing");
    let obj_b = sched::ramdisk::find("testing/b.o").expect("testing/b.o missing");

    let root = sched::galfs::admin_root();
    let a = sched::galfs::create_file_under(root, "a.o").expect("create a.o");
    let b = sched::galfs::create_file_under(root, "b.o").expect("create b.o");
    let out = sched::galfs::create_file_under(root, "out").expect("create out");
    assert_eq!(
        sched::galfs::append_file(a, obj_a),
        Some(obj_a.len()),
        "write a.o"
    );
    assert_eq!(
        sched::galfs::append_file(b, obj_b),
        Some(obj_b.len()),
        "write b.o"
    );

    let baseline = galexy_os::arch::mm::free_frames();
    sched::spawn_with_given_files(
        "gxldos",
        gxld,
        &[
            (a, CapRights::READ),
            (b, CapRights::READ),
            (out, CapRights::WRITE),
        ],
    )
    .expect("spawn gxldos");

    wait_until_dead("gxldos");
    assert_eq!(sched::user_exit_code("gxldos"), Some(0), "gxldos failed");

    let n = sched::galfs::object_len(out).expect("output missing");
    assert!(n > 64, "linked image is empty");
    let mut image = alloc::vec![0u8; n];
    let got = sched::galfs::read_file_bytes(out, &mut image).expect("read output");
    assert_eq!(got, n);
    assert!(image.starts_with(b"\x7fELF"), "output is not an ELF");

    sched::loader::spawn_program("linked", &image).expect("spawn linked image");
    wait_until_dead("linked");
    assert_eq!(
        sched::user_exit_code("linked"),
        Some(0x71),
        "linked program exit"
    );

    let mut stable = 0u32;
    while stable < 8 {
        x86_64::instructions::hlt();
        sched::reap();
        stable += 1;
    }
    assert_eq!(sched::unreaped_threads(), 0);
    let leaked = baseline.saturating_sub(galexy_os::arch::mm::free_frames());
    assert!(leaked < 512, "user frames leaked: {leaked}");

    println!("[test-gxld] ring-3 link ran");
    serial_println!("[test-gxld] passed");
    exit_qemu(QemuExitCode::Success);
}

fn wait_until_dead(name: &str) {
    let mut polls = 0u64;
    loop {
        x86_64::instructions::hlt();
        sched::drain_spawn();
        sched::drain_thread_spawn();
        sched::reap();
        if !sched::is_name_live(name) {
            return;
        }
        polls += 1;
        if polls > 2_000_000 {
            panic!("{name} never exited");
        }
    }
}
