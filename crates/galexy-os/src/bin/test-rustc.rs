//! On-OS Cranelift `rustc` (Milestone 72).
//!
//! The ramdisk holds one static `rustc` ELF, the target JSON, and the
//! sysroot rlibs as `srlib/<file>`. This kernel copies that tree into
//! galfs and runs three compilers: `--print cfg`, a type error, and
//! `--emit=obj`. Linking the object is Milestone 73.

#![no_std]
#![no_main]

extern crate alloc;

use alloc::string::String;
use alloc::vec::Vec;
use bootloader_api::{entry_point, BootInfo};
use galexy_os::{drivers::screen, exit_qemu, println, sched, serial_println, QemuExitCode};
use x86_64::VirtAddr;
use xmas_elf::symbol_table::Entry;
use xmas_elf::ElfFile;

entry_point!(test_main_entry, config = &galexy_os::BOOTLOADER_CONFIG);

const HELLO_RS: &[u8] = b"#![no_std]\npub fn hello() -> u32 {\n    72\n}\n";
const BAD_RS: &[u8] = b"#![no_std]\npub fn bad() {\n    let _x: u32 = \"no\";\n}\n";

fn test_main_entry(boot_info: &'static mut BootInfo) -> ! {
    galexy_os::init();
    screen::init(boot_info);
    println!("[test-rustc] running");
    serial_println!("[test-rustc] running");

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

    let rustc = sched::ramdisk::find("rustc").expect("rustc missing");
    run_rustc(
        "rustc-cfg",
        rustc,
        &[
            "--sysroot",
            "/",
            "--print",
            "cfg",
            "--target",
            "x86_64-unknown-galexy",
        ],
    );
    assert_eq!(sched::user_exit_code("rustc-cfg"), Some(0), "cfg failed");
    serial_println!("rustc-cfg: exit 0");

    run_rustc(
        "rustc-err",
        rustc,
        &[
            "--sysroot",
            "/",
            "-Zunstable-options",
            "--edition",
            "2021",
            "--crate-type",
            "lib",
            "--crate-name",
            "bad",
            "--emit",
            "metadata",
            "--target",
            "/x86_64-unknown-galexy.json",
            "/bad.rs",
        ],
    );
    let err = sched::user_exit_code("rustc-err");
    assert_ne!(err, Some(0), "type error should fail");
    assert_ne!(err, Some(134), "type error aborted");
    serial_println!("rustc-err: exit {}", err.unwrap_or(0));

    run_rustc(
        "rustc-obj",
        rustc,
        &[
            "--sysroot",
            "/",
            "-Zunstable-options",
            "--edition",
            "2021",
            "--crate-type",
            "lib",
            "--crate-name",
            "hello",
            "--emit",
            "obj",
            "--target",
            "/x86_64-unknown-galexy.json",
            "-o",
            "/hello.o",
            "/hello.rs",
        ],
    );
    assert_eq!(
        sched::user_exit_code("rustc-obj"),
        Some(0),
        "emit obj failed"
    );
    summarize_object();

    serial_println!("[test-rustc] passed");
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
        if name.starts_with("srlib/") || name == "x86_64-unknown-galexy.json" || name == "hello.rs"
        {
            names.push(String::from(name));
        }
    });
    assert!(names.iter().any(|n| n.starts_with("srlib/")), "no srlib");

    for name in &names {
        let bytes = sched::ramdisk::find(name).expect("ramdisk file vanished");
        if let Some(file_name) = name.strip_prefix("srlib/") {
            sched::galfs::import_promoted(libs, file_name, bytes)
                .unwrap_or_else(|_| panic!("import {file_name}"));
        } else {
            copy_file(root, name, bytes);
        }
    }
    copy_file(root, "hello.rs", HELLO_RS);
    copy_file(root, "bad.rs", BAD_RS);
    serial_println!("rustc-sysroot: {} rlib entries listed", names.len());
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

fn run_rustc(task: &str, rustc: &[u8], parts: &[&str]) {
    let arg = join_args(parts);
    serial_println!("[test-rustc] spawn {}", task);
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

/// Section sizes and function/object symbols, sorted. Debug, comment,
/// and note sections are omitted: those carry paths and producer strings.
fn summarize_object() {
    let root = sched::galfs::admin_root();
    let id = sched::galfs::find_under(root, "hello.o").expect("hello.o missing");
    let n = sched::galfs::object_len(id).expect("hello.o length");
    assert!(n > 64, "hello.o is empty");
    let mut bytes = alloc::vec![0u8; n];
    let got = sched::galfs::read_file_bytes(id, &mut bytes).expect("read hello.o");
    assert_eq!(got, n);
    let elf = ElfFile::new(&bytes).expect("hello.o is not an ELF");
    let mut lines: Vec<String> = Vec::new();
    for section in elf.section_iter() {
        let Ok(name) = section.get_name(&elf) else {
            continue;
        };
        if skip_section(name) {
            continue;
        }
        lines.push(alloc::format!("rustc-obj: sec {name} {}", section.size()));
    }
    if let Some(symtab) = elf.find_section_by_name(".symtab") {
        if let Ok(xmas_elf::sections::SectionData::SymbolTable64(symbols)) = symtab.get_data(&elf) {
            for symbol in symbols {
                let Ok(name) = symbol.get_name(&elf) else {
                    continue;
                };
                if name.is_empty() || name.contains('/') {
                    continue;
                }
                let Ok(kind) = symbol.get_type() else {
                    continue;
                };
                if !matches!(
                    kind,
                    xmas_elf::symbol_table::Type::Func | xmas_elf::symbol_table::Type::Object
                ) {
                    continue;
                }
                lines.push(alloc::format!("rustc-obj: sym {name} {}", symbol.size()));
            }
        }
    }
    lines.sort();
    assert!(!lines.is_empty(), "hello.o has no sections");
    for line in &lines {
        serial_println!("{}", line);
    }
}

fn skip_section(name: &str) -> bool {
    name.is_empty()
        || name.starts_with(".debug")
        || name.starts_with(".comment")
        || name.starts_with(".note")
        || name.starts_with(".llvm")
}
