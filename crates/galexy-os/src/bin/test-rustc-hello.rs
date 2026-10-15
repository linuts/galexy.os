//! Milestone 73: on-OS `rustc` links with `gxld` and the program runs.
//!
//! The ramdisk holds the compiler, `gxld`, the target JSON, the `std`
//! sysroot (`srlib/`), and two host-linked references (`hello-lld`,
//! `hello-gxld`). Ring 3 then runs `rustc hello.rs -o hello` and
//! `rustc files.rs -o files`.

#![no_std]
#![no_main]

extern crate alloc;

use alloc::string::String;
use alloc::vec::Vec;
use bootloader_api::{entry_point, BootInfo};
use galexy_os::{drivers::screen, exit_qemu, println, sched, serial_println, QemuExitCode};
use x86_64::VirtAddr;

entry_point!(test_main_entry, config = &galexy_os::BOOTLOADER_CONFIG);

const HELLO_RS: &[u8] = b"fn main() {\n    println!(\"hello from rustc on galexy\");\n}\n";
const FILES_RS: &[u8] = b"\
fn main() {\n    \
let handle = std::thread::spawn(|| {\n        \
std::fs::write(\"/scratch\", b\"from a thread\").unwrap();\n    \
});\n    \
handle.join().unwrap();\n    \
let got = std::fs::read(\"/scratch\").unwrap();\n    \
assert_eq!(got, b\"from a thread\");\n    \
println!(\"files-ok\");\n}\n";

fn test_main_entry(boot_info: &'static mut BootInfo) -> ! {
    galexy_os::init();
    screen::init(boot_info);
    println!("[test-rustc-hello] running");
    serial_println!("[test-rustc-hello] running");

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

    install_sysroot();

    run_ramdisk("ref-lld", "hello-lld");
    assert_eq!(
        sched::user_exit_code("ref-lld"),
        Some(0),
        "hello-lld failed"
    );
    serial_println!("ref-lld: exit 0");

    run_ramdisk("ref-gxld", "hello-gxld");
    assert_eq!(
        sched::user_exit_code("ref-gxld"),
        Some(0),
        "hello-gxld failed"
    );
    serial_println!("ref-gxld: exit 0");

    let rustc = sched::ramdisk::find("rustc").expect("rustc missing");
    run_rustc("rustc-hello", rustc, &["hello.rs", "-o", "hello"]);
    let hello_code = sched::user_exit_code("rustc-hello");
    assert_eq!(hello_code, Some(0), "rustc hello.rs failed");
    serial_println!("rustc-hello: exit 0");

    let hello = read_root_file("hello");
    run_elf("on-hello", &hello);
    assert_eq!(
        sched::user_exit_code("on-hello"),
        Some(0),
        "on-OS hello failed"
    );
    serial_println!("on-hello: exit 0");

    run_rustc("rustc-files", rustc, &["files.rs", "-o", "files"]);
    let files_code = sched::user_exit_code("rustc-files");
    assert_eq!(files_code, Some(0), "rustc files.rs failed");
    serial_println!("rustc-files: exit 0");

    let files = read_root_file("files");
    run_elf("on-files", &files);
    assert_eq!(
        sched::user_exit_code("on-files"),
        Some(0),
        "on-OS files failed"
    );
    serial_println!("on-files: exit 0");

    let scratch = read_root_file("scratch");
    assert_eq!(scratch, b"from a thread", "scratch file mismatch");
    serial_println!("scratch: from a thread");

    serial_println!("[test-rustc-hello] passed");
    exit_qemu(QemuExitCode::Success);
}

fn install_sysroot() {
    let root = sched::galfs::admin_root();
    let lib = mkdir(root, "lib");
    let rustlib = mkdir(lib, "rustlib");
    let triple = mkdir(rustlib, "x86_64-unknown-galexy");
    let libs = mkdir(triple, "lib");

    let mut names: Vec<String> = Vec::new();
    sched::ramdisk::for_each_name(|name| {
        if name.starts_with("srlib/") || name == "x86_64-unknown-galexy.json" {
            names.push(String::from(name));
        }
    });
    assert!(names.iter().any(|n| n.starts_with("srlib/")), "no srlib");
    assert!(
        sched::ramdisk::find("gxld").is_some(),
        "gxld missing from ramdisk"
    );
    sched::galfs::raise_admin_quota_for_sysroot();

    for name in &names {
        let bytes = sched::ramdisk::find(name).expect("ramdisk file vanished");
        if let Some(file_name) = name.strip_prefix("srlib/") {
            sched::galfs::import_shared(libs, file_name, bytes)
                .unwrap_or_else(|_| panic!("import {file_name}"));
        } else {
            copy_file(root, name, bytes);
        }
    }
    copy_file(root, "hello.rs", HELLO_RS);
    copy_file(root, "files.rs", FILES_RS);
    serial_println!("rustc-sysroot: {} entries", names.len());
}

fn mkdir(parent: u16, name: &str) -> u16 {
    if let Some(existing) = sched::galfs::find_under(parent, name) {
        return existing;
    }
    sched::galfs::mkdir_under_root(parent, name).unwrap_or_else(|_| panic!("mkdir {name}"))
}

fn copy_file(parent: u16, name: &str, bytes: &[u8]) {
    let id =
        sched::galfs::create_file_under(parent, name).unwrap_or_else(|_| panic!("create {name}"));
    let n = sched::galfs::append_file(id, bytes).unwrap_or(0);
    assert_eq!(n, bytes.len(), "short write {name}: {n} of {}", bytes.len());
}

fn read_root_file(name: &str) -> Vec<u8> {
    let root = sched::galfs::admin_root();
    let id = sched::galfs::find_under(root, name).unwrap_or_else(|| panic!("{name} missing"));
    let n = sched::galfs::object_len(id).unwrap_or_else(|| panic!("{name} length"));
    assert!(n > 0, "{name} is empty");
    let mut bytes = alloc::vec![0u8; n];
    let got =
        sched::galfs::read_file_bytes(id, &mut bytes).unwrap_or_else(|| panic!("read {name}"));
    assert_eq!(got, n, "short read {name}");
    bytes
}

fn run_ramdisk(task: &str, label: &str) {
    let bytes = sched::ramdisk::find(label).unwrap_or_else(|| panic!("{label} missing"));
    serial_println!("[test-rustc-hello] spawn {}", task);
    sched::loader::spawn_program_session(task, bytes, &[])
        .unwrap_or_else(|_| panic!("spawn {task}"));
    wait_until_dead(task);
}

fn run_elf(task: &str, bytes: &[u8]) {
    serial_println!("[test-rustc-hello] spawn {}", task);
    sched::loader::spawn_program_session(task, bytes, &[])
        .unwrap_or_else(|_| panic!("spawn {task}"));
    wait_until_dead(task);
}

fn run_rustc(task: &str, rustc: &[u8], parts: &[&str]) {
    let arg = join_args(parts);
    serial_println!("[test-rustc-hello] spawn {}", task);
    sched::loader::spawn_program_session(task, rustc, &arg)
        .unwrap_or_else(|_| panic!("spawn {task}"));
    wait_until_dead(task);
}

fn join_args(parts: &[&str]) -> Vec<u8> {
    let mut out = Vec::new();
    for (i, part) in parts.iter().enumerate() {
        if i > 0 {
            out.push(0);
        }
        out.extend_from_slice(part.as_bytes());
    }
    out
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
