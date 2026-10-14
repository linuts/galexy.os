//! Milestone 93: ring 3 runs the pinned RustPython ELF.
//! Milestone 94: that same process reads and writes galfs, and `time`
//! follows `Date` / `Clock`.
//!
//! The ramdisk holds `python` only when `target/galexy-python/python`
//! existed at image build. Scripts live on the galfs root because
//! `std::fs` opens them. A refused import, and a path with no token,
//! must exit non-zero without panicking the kernel.

#![no_std]
#![no_main]

extern crate alloc;

use alloc::vec::Vec;
use bootloader_api::{entry_point, BootInfo};
use galexy_os::{drivers::screen, exit_qemu, println, sched, serial_println, QemuExitCode};
use x86_64::VirtAddr;

entry_point!(test_main_entry, config = &galexy_os::BOOTLOADER_CONFIG);

const HELLO_PY: &[u8] = b"print(\"hello from python on galexy\")\n";
const BAD_PY: &[u8] = b"import socket\nprint(\"should-not-run\")\n";
const NOTE: &[u8] = b"alpha\n";
/// 2026-10-12 15:04:00 UTC. `Clock` must not move when this is set.
const WHEN: u64 = 1_791_817_440;
const FILES_PY: &[u8] = br#"
import time
try:
    time.time()
    print("time-unexpected")
    raise SystemExit(1)
except OSError:
    print("time-unset")
import io
if open is not io.open:
    print("open-split")
    raise SystemExit(1)
text = io.open("note.txt")
if type(text).__name__ != "TextIOWrapper":
    print("bad-text-type")
    raise SystemExit(1)
got = text.read()
text.close()
if got != "alpha\n":
    print("bad-read")
    raise SystemExit(1)
with open("out.txt", "w") as handle:
    handle.write(got)
raw = io.open("out.txt", "rb")
if type(raw).__name__ != "BufferedReader":
    print("bad-binary-type")
    raise SystemExit(1)
if raw.read() != b"alpha\n":
    print("bad-binary")
    raise SystemExit(1)
raw.close()
open("out2.txt","w").write(got)
if open("out2.txt").read() != got:
    print("bad-temp")
    raise SystemExit(1)
print("files-ok")
"#;
const DENY_PY: &[u8] = b"open(\"note.txt\").read()\nprint(\"token-unexpected\")\n";
const TIME_PY: &[u8] = b"\
import time\n\
print(\"wall\", int(time.time()))\n\
print(\"mono-ms\", int(time.monotonic() * 1000))\n\
";

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

    galexy_os::arch::clear_secs();
    copy_file(root, "note.txt", NOTE);
    copy_file(root, "files.py", FILES_PY);
    copy_file(root, "deny.py", DENY_PY);
    copy_file(root, "time.py", TIME_PY);

    run_python("py-files", python, "files.py");
    let files_code = sched::user_exit_code("py-files");
    serial_println!("py-files: exit {:?}", files_code);
    assert_eq!(files_code, Some(0), "python files.py failed");
    let out = read_root_file(root, "out.txt");
    assert_eq!(out, NOTE, "out.txt mismatch");
    serial_println!("out: alpha");

    let mut bare = sched::galfs::admin_cred();
    for slot in bare.tokens.iter_mut() {
        *slot = sched::galfs::Token::empty();
    }
    run_python_fs("py-deny", python, "deny.py", bare);
    let deny_code = sched::user_exit_code("py-deny");
    serial_println!("py-deny: exit {:?}", deny_code);
    assert_ne!(deny_code, Some(0), "a path with no token must not succeed");
    assert!(deny_code.is_some(), "denied open left no exit code");

    assert!(galexy_os::arch::set_secs(WHEN), "date set rejected");
    let before = galexy_os::arch::timer_ticks();
    serial_println!("clock-before {}", before);
    run_python("py-time", python, "time.py");
    let after = galexy_os::arch::timer_ticks();
    serial_println!("clock-after {}", after);
    let time_code = sched::user_exit_code("py-time");
    serial_println!("py-time: exit {:?}", time_code);
    assert_eq!(time_code, Some(0), "python time.py failed");

    serial_println!("[test-python] passed");
    exit_qemu(QemuExitCode::Success);
}

fn copy_file(parent: u16, name: &str, bytes: &[u8]) {
    let id =
        sched::galfs::create_file_under(parent, name).unwrap_or_else(|_| panic!("create {name}"));
    let n = sched::galfs::append_file(id, bytes).unwrap_or(0);
    assert_eq!(n, bytes.len(), "short write {name}: {n} of {}", bytes.len());
}

fn read_root_file(root: u16, name: &str) -> Vec<u8> {
    let id = sched::galfs::find_under(root, name).unwrap_or_else(|| panic!("{name} missing"));
    let n = sched::galfs::object_len(id).unwrap_or_else(|| panic!("{name} length"));
    let mut bytes = alloc::vec![0u8; n];
    let got =
        sched::galfs::read_file_bytes(id, &mut bytes).unwrap_or_else(|| panic!("read {name}"));
    assert_eq!(got, n, "short read {name}");
    bytes
}

fn run_python(task: &str, python: &[u8], path: &str) {
    run_python_fs(task, python, path, sched::galfs::admin_cred());
}

fn run_python_fs(task: &str, python: &[u8], path: &str, fs: sched::galfs::FsCred) {
    serial_println!("[test-python] spawn {} {}", task, path);
    sched::loader::spawn_program_with_fs(task, python, path.as_bytes(), fs)
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
