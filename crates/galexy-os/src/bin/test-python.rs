//! Milestone 93: ring 3 runs the pinned RustPython ELF.
//!
//! The ramdisk holds `python` only when `target/galexy-python/python`
//! existed at image build. This kernel writes `hello.py` and `bad.py`
//! onto the galfs root (that is what `std::fs` opens) and spawns the
//! interpreter with each path. A refused import must exit non-zero
//! without panicking the kernel.

#![no_std]
#![no_main]

use bootloader_api::{entry_point, BootInfo};
use galexy_os::{drivers::screen, exit_qemu, println, sched, serial_println, QemuExitCode};
use x86_64::VirtAddr;

entry_point!(test_main_entry, config = &galexy_os::BOOTLOADER_CONFIG);

const HELLO_PY: &[u8] = b"print(\"hello from python on galexy\")\n";
const BAD_PY: &[u8] = b"import socket\nprint(\"should-not-run\")\n";

fn test_main_entry(boot_info: &'static mut BootInfo) -> ! {
    galexy_os::init();
    screen::init(boot_info);
    println!("[test-python] running");
    serial_println!("[test-python] running");

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

    let python = sched::ramdisk::find("python").expect("python missing from ramdisk");
    let root = sched::galfs::admin_root();
    copy_file(root, "hello.py", HELLO_PY);
    copy_file(root, "bad.py", BAD_PY);

    run_python("py-hello", python, "hello.py");
    let hello_code = sched::user_exit_code("py-hello");
    serial_println!("py-hello: exit {:?}", hello_code);
    assert_eq!(hello_code, Some(0), "python hello.py failed");

    run_python("py-import", python, "bad.py");
    let import_code = sched::user_exit_code("py-import");
    serial_println!("py-import: exit {:?}", import_code);
    assert_ne!(import_code, Some(0), "import socket must not succeed");
    assert!(import_code.is_some(), "import socket left no exit code");

    serial_println!("[test-python] passed");
    exit_qemu(QemuExitCode::Success);
}

fn copy_file(parent: u16, name: &str, bytes: &[u8]) {
    let id =
        sched::galfs::create_file_under(parent, name).unwrap_or_else(|_| panic!("create {name}"));
    let n = sched::galfs::append_file(id, bytes).unwrap_or(0);
    assert_eq!(n, bytes.len(), "short write {name}: {n} of {}", bytes.len());
}

fn run_python(task: &str, python: &[u8], path: &str) {
    serial_println!("[test-python] spawn {} {}", task, path);
    sched::loader::spawn_program_session(task, python, path.as_bytes())
        .unwrap_or_else(|_| panic!("spawn {task}"));
    wait_until_dead(task);
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
        if polls > 20_000_000 {
            panic!("{name} never exited");
        }
    }
}
