//! Boot-level integration tests: each boots a galexy.os kernel image in
//! headless QEMU and asserts on the exit code and COM1 output.

mod common;

use common::{
    boot, boot_and_type, boot_and_type_uefi, boot_liveness, boot_uefi, boot_with_galfs,
    boot_with_galfs_recover, image, QEMU_EXIT_SUCCESS,
};
use std::time::Duration;

#[test]
fn test_kernel_runs_and_passes() {
    let (code, serial) = boot(&image("test-basic"));
    assert_eq!(
        code,
        Some(QEMU_EXIT_SUCCESS),
        "test-basic should exit with Success; serial:\n{serial}"
    );
    assert!(
        serial.contains("[test-basic] passed"),
        "test-basic success marker missing; serial:\n{serial}"
    );
}

#[test]
fn should_panic_kernel_exits_successfully() {
    let (code, serial) = boot(&image("test-should-panic"));
    assert_eq!(
        code,
        Some(QEMU_EXIT_SUCCESS),
        "expected panic should map to Success; serial:\n{serial}"
    );
    assert!(
        serial.contains("[PANIC]"),
        "panic report missing from serial:\n{serial}"
    );
}

#[test]
fn memory_test_passes() {
    let (code, serial) = boot(&image("test-memory"));
    assert_eq!(
        code,
        Some(QEMU_EXIT_SUCCESS),
        "test-memory should exit with Success; serial:\n{serial}"
    );
    assert!(
        serial.contains("[test-memory] passed"),
        "test-memory success marker missing; serial:\n{serial}"
    );
    assert!(
        serial.contains("[mm] frame allocator ready"),
        "frame allocator init marker missing; serial:\n{serial}"
    );
}

#[test]
fn paging_test_passes() {
    let (code, serial) = boot(&image("test-paging"));
    assert_eq!(
        code,
        Some(QEMU_EXIT_SUCCESS),
        "test-paging should exit with Success; serial:\n{serial}"
    );
    assert!(
        serial.contains("[test-paging] page fault fired as expected"),
        "unmapped-access fault marker missing; serial:\n{serial}"
    );
}

#[test]
fn heap_test_passes() {
    let (code, serial) = boot(&image("test-heap"));
    assert_eq!(
        code,
        Some(QEMU_EXIT_SUCCESS),
        "test-heap should exit with Success; serial:\n{serial}"
    );
    assert!(
        serial.contains("[test-heap] passed"),
        "test-heap success marker missing; serial:\n{serial}"
    );
    assert!(
        serial.contains("[heap] ready"),
        "heap init marker missing; serial:\n{serial}"
    );
}

#[test]
fn shutdown_test_powers_off() {
    let (code, serial) = boot(&image("test-shutdown"));
    assert!(
        serial.contains("[test-shutdown] powering off"),
        "shutdown never started; serial:\n{serial}"
    );
    assert!(
        !serial.contains("[PANIC]"),
        "shutdown returned; serial:\n{serial}"
    );
    assert!(
        code.is_some(),
        "QEMU did not exit after shutdown; serial:\n{serial}"
    );
}

#[test]
fn reboot_test_resets() {
    let (code, serial) = boot(&image("test-reboot"));
    assert!(
        serial.contains("[test-reboot] resetting"),
        "reboot never started; serial:\n{serial}"
    );
    assert!(
        !serial.contains("[PANIC]"),
        "reboot returned; serial:\n{serial}"
    );
    assert!(
        code.is_some(),
        "QEMU did not exit after reboot; serial:\n{serial}"
    );
}

#[test]
fn acpi_test_passes() {
    // MADT discovery is architecture truth for both boot paths: run it on
    // the BIOS image AND the UEFI image (OVMF's tables must parse too).
    let (code, serial) = boot(&image("test-acpi"));
    assert_eq!(
        code,
        Some(QEMU_EXIT_SUCCESS),
        "test-acpi should exit with Success; serial:\n{serial}"
    );
    assert!(
        serial.contains("[test-acpi] passed"),
        "test-acpi success marker missing; serial:\n{serial}"
    );
    assert!(
        serial.contains("[acpi] madt ready"),
        "MADT discovery marker missing; serial:\n{serial}"
    );

    // The UEFI image boots OVMF; its tables must validate the same way.
    let mut last = String::new();
    for _ in 0..3 {
        last = boot_uefi(&image("test-acpi"), Duration::from_secs(30));
        if last.contains("[test-acpi] passed") {
            break;
        }
    }
    assert!(
        last.contains("[test-acpi] passed"),
        "test-acpi under UEFI never passed after 3 attempts; serial:\n{last}"
    );
}

#[test]
fn apic_test_passes() {
    let (code, serial) = boot(&image("test-apic"));
    assert_eq!(
        code,
        Some(QEMU_EXIT_SUCCESS),
        "test-apic should exit with Success; serial:\n{serial}"
    );
    assert!(
        serial.contains("[test-apic] passed"),
        "test-apic success marker missing; serial:\n{serial}"
    );
    assert!(
        serial.contains("[apic] lapic up"),
        "LAPIC enable marker missing; serial:\n{serial}"
    );
    assert!(
        serial.contains("detected mode: XApic"),
        "LAPIC mode detection marker missing; serial:\n{serial}"
    );
}

#[test]
fn heap_grow_test_passes() {
    let (code, serial) = boot(&image("test-heapgrow"));
    assert_eq!(
        code,
        Some(QEMU_EXIT_SUCCESS),
        "test-heapgrow should exit with Success; serial:\n{serial}"
    );
    assert!(
        serial.contains("[test-heapgrow] passed"),
        "test-heapgrow success marker missing; serial:\n{serial}"
    );
    assert!(
        serial.contains("[heap] grown:"),
        "heap growth marker missing; serial:\n{serial}"
    );
}

#[test]
fn screen_test_passes() {
    let (code, serial) = boot(&image("test-screen"));
    assert_eq!(
        code,
        Some(QEMU_EXIT_SUCCESS),
        "test-screen should exit with Success; serial:\n{serial}"
    );
    assert!(
        serial.contains("[test-screen] passed"),
        "test-screen success marker missing; serial:\n{serial}"
    );
    assert!(
        serial.contains("\u{1b}[31mZ"),
        "console did not mirror the CSI bytes to serial:\n{serial}"
    );
}

#[test]
fn sched_test_passes() {
    let (code, serial) = boot(&image("test-sched"));
    assert_eq!(
        code,
        Some(QEMU_EXIT_SUCCESS),
        "test-sched should exit with Success; serial:\n{serial}"
    );
    assert!(
        serial.contains("[test-sched] passed"),
        "test-sched success marker missing; serial:\n{serial}"
    );
}

#[test]
fn preempt_test_passes() {
    let (code, serial) = boot(&image("test-preempt"));
    assert_eq!(
        code,
        Some(QEMU_EXIT_SUCCESS),
        "test-preempt should exit with Success; serial:\n{serial}"
    );
    assert!(
        serial.contains("[test-preempt] passed"),
        "test-preempt success marker missing; serial:\n{serial}"
    );
}

#[test]
fn cloneroot_test_passes() {
    let (code, serial) = boot(&image("test-cloneroot"));
    assert_eq!(
        code,
        Some(QEMU_EXIT_SUCCESS),
        "test-cloneroot should exit with Success; serial:\n{serial}"
    );
    assert!(
        serial.contains("[test-cloneroot] passed"),
        "test-cloneroot success marker missing; serial:\n{serial}"
    );
}

#[test]
fn freshl4_test_passes() {
    let (code, serial) = boot(&image("test-freshl4"));
    assert_eq!(
        code,
        Some(QEMU_EXIT_SUCCESS),
        "test-freshl4 should exit with Success; serial:\n{serial}"
    );
    assert!(
        serial.contains("[test-freshl4] passed"),
        "test-freshl4 success marker missing; serial:\n{serial}"
    );
}

#[test]
fn reuse_test_passes() {
    let (code, serial) = boot(&image("test-reuse"));
    assert_eq!(
        code,
        Some(QEMU_EXIT_SUCCESS),
        "test-reuse should exit with Success; serial:\n{serial}"
    );
    assert!(
        serial.contains("[test-reuse] keeper live, exited 80"),
        "test-reuse live-name marker missing; serial:\n{serial}"
    );
    assert!(
        serial.contains("[test-reuse] passed"),
        "test-reuse success marker missing; serial:\n{serial}"
    );
}

#[test]
fn threadexit_test_passes() {
    let (code, serial) = boot(&image("test-threadexit"));
    assert_eq!(
        code,
        Some(QEMU_EXIT_SUCCESS),
        "test-threadexit should exit with Success; serial:\n{serial}"
    );
    assert!(
        serial.contains("[test-threadexit] passed"),
        "test-threadexit success marker missing; serial:\n{serial}"
    );
    // Per-CPU reaping (SMP M18): the three exits can be reaped in chunks
    // per owner CPU (any split of 3, e.g. "reaped 3" or "reaped 1" twice).
    let total: usize = serial
        .lines()
        .filter_map(|l| {
            let idx = l.find("reaped ")?;
            let rest = &l[idx + "reaped ".len()..];
            let word = rest.split(' ').next()?;
            word.parse::<usize>().ok()
        })
        .sum();
    assert!(
        total == 3,
        "reaper freed {} stack(s) across CPUs; serial:\n{serial}",
        total
    );
}

#[test]
fn rings_test_passes() {
    let (code, serial) = boot(&image("test-rings"));
    assert_eq!(
        code,
        Some(QEMU_EXIT_SUCCESS),
        "test-rings should exit with Success; serial:\n{serial}"
    );
    assert!(
        serial.contains("[test-rings] passed"),
        "test-rings success marker missing; serial:\n{serial}"
    );
}

#[test]
fn userpreempt_test_passes() {
    let (code, serial) = boot(&image("test-userpreempt"));
    assert_eq!(
        code,
        Some(QEMU_EXIT_SUCCESS),
        "test-userpreempt should exit with Success; serial:\n{serial}"
    );
    assert!(
        serial.contains("[test-userpreempt] passed"),
        "test-userpreempt success marker missing; serial:\n{serial}"
    );
    assert!(
        serial.contains("ready (own tree cr3="),
        "user task spawn marker missing; serial:\n{serial}"
    );
}

#[test]
fn syscall_test_passes() {
    let (code, serial) = boot(&image("test-syscall"));
    assert_eq!(
        code,
        Some(QEMU_EXIT_SUCCESS),
        "test-syscall should exit with Success; serial:\n{serial}"
    );
    assert!(
        serial.contains("[test-syscall] passed"),
        "test-syscall success marker missing; serial:\n{serial}"
    );
    assert!(
        serial.contains("SYSCALL/SYSRET live"),
        "syscall MSR init marker missing; serial:\n{serial}"
    );
}

#[test]
fn user_lifecycle_test_passes() {
    let (code, serial) = boot(&image("test-user"));
    assert_eq!(
        code,
        Some(QEMU_EXIT_SUCCESS),
        "test-user should exit with Success; serial:\n{serial}"
    );
    assert!(
        serial.contains("[test-user] passed"),
        "test-user success marker missing; serial:\n{serial}"
    );
    assert!(
        serial.contains("task 'uhello' exited (syscall)"),
        "user task exit marker missing; serial:\n{serial}"
    );
    assert!(
        serial.contains("ready (own tree cr3="),
        "user task spawn marker missing; serial:\n{serial}"
    );
}

#[test]
fn treechurn_test_passes() {
    let (code, serial) = boot(&image("test-treechurn"));
    assert_eq!(
        code,
        Some(QEMU_EXIT_SUCCESS),
        "test-treechurn should exit with Success; serial:\n{serial}"
    );
    assert!(
        serial.contains("[test-treechurn] passed"),
        "test-treechurn success marker missing; serial:\n{serial}"
    );
    assert!(
        serial.contains("freed task 'churn' tree"),
        "tree-walk marker missing; serial:\n{serial}"
    );
}

#[test]
fn userfault_test_passes() {
    let (code, serial) = boot(&image("test-userfault"));
    assert_eq!(
        code,
        Some(QEMU_EXIT_SUCCESS),
        "test-userfault should exit with Success; serial:\n{serial}"
    );
    assert!(
        serial.contains("[test-userfault] passed"),
        "test-userfault success marker missing; serial:\n{serial}"
    );
    assert!(
        serial.contains("task 'boomer' exited (page fault)"),
        "fault-kill marker missing; serial:\n{serial}"
    );
}

#[test]
fn rm_test_passes() {
    let (code, serial) = boot(&image("test-rm"));
    assert_eq!(
        code,
        Some(QEMU_EXIT_SUCCESS),
        "test-rm should exit with Success; serial:\n{serial}"
    );
    assert!(
        serial.contains("[test-rm] passed"),
        "test-rm success marker missing; serial:\n{serial}"
    );
}

#[test]
fn galfs_test_passes() {
    let (code, serial) = boot(&image("test-galfs"));
    assert_eq!(
        code,
        Some(QEMU_EXIT_SUCCESS),
        "test-galfs should exit with Success; serial:\n{serial}"
    );
    assert!(
        serial.contains("[test-galfs] passed"),
        "test-galfs success marker missing; serial:\n{serial}"
    );
}

#[test]
fn galfs_disk_persists_across_reboot() {
    let (code1, serial1, code2, serial2) = boot_with_galfs(&image("test-galfs-disk"));
    assert_eq!(
        code1,
        Some(QEMU_EXIT_SUCCESS),
        "test-galfs-disk write boot should exit with Success; serial:\n{serial1}"
    );
    assert!(
        serial1.contains("[test-galfs-disk] wrote"),
        "write marker missing; serial:\n{serial1}"
    );
    assert_eq!(
        code2,
        Some(QEMU_EXIT_SUCCESS),
        "test-galfs-disk verify boot should exit with Success; serial:\n{serial2}"
    );
    assert!(
        serial2.contains("[test-galfs-disk] passed"),
        "verify marker missing; serial:\n{serial2}"
    );
}

#[test]
fn galfs_disk_recovers_from_corrupt_slot() {
    let (code1, serial1, code2, serial2) = boot_with_galfs_recover(&image("test-galfs-disk"));
    assert_eq!(
        code1,
        Some(QEMU_EXIT_SUCCESS),
        "write boot should succeed; serial:\n{serial1}"
    );
    assert!(
        serial1.contains("[test-galfs-disk] wrote"),
        "write marker missing; serial:\n{serial1}"
    );
    assert_eq!(
        code2,
        Some(QEMU_EXIT_SUCCESS),
        "recover boot should succeed from older slot; serial:\n{serial2}"
    );
    assert!(
        serial2.contains("[test-galfs-disk] passed"),
        "recover marker missing; serial:\n{serial2}"
    );
}

#[test]
fn pipe_test_passes() {
    let (code, serial) = boot(&image("test-pipe"));
    assert_eq!(
        code,
        Some(QEMU_EXIT_SUCCESS),
        "test-pipe should exit with Success; serial:\n{serial}"
    );
    assert!(
        serial.contains("[test-pipe] passed"),
        "test-pipe success marker missing; serial:\n{serial}"
    );
}

#[test]
fn seek_test_passes() {
    let (code, serial) = boot(&image("test-seek"));
    assert_eq!(
        code,
        Some(QEMU_EXIT_SUCCESS),
        "test-seek should exit with Success; serial:\n{serial}"
    );
    assert!(
        serial.contains("[test-seek] passed"),
        "test-seek success marker missing; serial:\n{serial}"
    );
}

#[test]
fn users_test_passes() {
    let (code, serial) = boot(&image("test-users"));
    assert_eq!(
        code,
        Some(QEMU_EXIT_SUCCESS),
        "test-users should exit with Success; serial:\n{serial}"
    );
    assert!(
        serial.contains("[test-users] passed"),
        "test-users success marker missing; serial:\n{serial}"
    );
}

#[test]
fn scratch_test_passes() {
    let (code, serial) = boot(&image("test-scratch"));
    assert_eq!(
        code,
        Some(QEMU_EXIT_SUCCESS),
        "test-scratch should exit with Success; serial:\n{serial}"
    );
    assert!(
        serial.contains("[test-scratch] passed"),
        "test-scratch success marker missing; serial:\n{serial}"
    );
}

#[test]
fn open_test_passes() {
    let (code, serial) = boot(&image("test-open"));
    assert_eq!(
        code,
        Some(QEMU_EXIT_SUCCESS),
        "test-open should exit with Success; serial:\n{serial}"
    );
    assert!(
        serial.contains("[test-open] passed"),
        "test-open success marker missing; serial:\n{serial}"
    );
}

#[test]
fn ramdisk_test_passes() {
    let (code, serial) = boot(&image("test-ramdisk"));
    assert_eq!(
        code,
        Some(QEMU_EXIT_SUCCESS),
        "test-ramdisk should exit with Success; serial:\n{serial}"
    );
    assert!(
        serial.contains("[test-ramdisk] passed"),
        "test-ramdisk success marker missing; serial:\n{serial}"
    );
    assert!(
        serial.contains("tar entry: 'banner.txt'"),
        "ramdisk entry marker missing; serial:\n{serial}"
    );
}

#[test]
fn realprogram_test_passes() {
    let (code, serial) = boot(&image("test-realprogram"));
    assert_eq!(
        code,
        Some(QEMU_EXIT_SUCCESS),
        "test-realprogram should exit with Success; serial:\n{serial}"
    );
    assert!(
        serial.contains("[test-realprogram] passed"),
        "test-realprogram success marker missing; serial:\n{serial}"
    );
    assert!(
        serial.contains("[loader] program 'hello' ready"),
        "loader spawn marker missing; serial:\n{serial}"
    );
    // The write syscall must actually SUCCEED: hello's text reaches COM1
    // through the console mirror (guards the active-tree buffer walk —
    // a kernel-tree walk reports BadBuffer for every user buffer).
    assert!(
        serial.contains(HELLO_TEXT),
        "hello's console output missing from serial (write syscall failed?); serial:\n{serial}"
    );
}

#[test]
fn runshell_test_passes() {
    let (code, serial) = boot(&image("test-runshell"));
    assert_eq!(
        code,
        Some(QEMU_EXIT_SUCCESS),
        "test-runshell should exit with Success; serial:\n{serial}"
    );
    assert!(
        serial.contains("[test-runshell] passed"),
        "test-runshell success marker missing; serial:\n{serial}"
    );
    // hello's output reached COM1 through the console mirror (exactly the
    // text the real program writes).
    assert!(
        serial.contains("Hello from a real Rust user program!"),
        "user program console output missing from serial:\n{serial}"
    );
    assert!(
        serial.contains("exited (syscall)"),
        "user task exit marker missing; serial:\n{serial}"
    );
}

/// The exact text the real hello program prints through the console
/// syscall (mirrored to COM1 by the kernel).
const HELLO_TEXT: &str = "Hello from a real Rust user program!";

/// Qcode + expected-echo pairs for typing `hello` + Enter (typing
/// E2E). Each key syncs on the shell's echo of it (console = screen +
/// serial); the Enter key syncs on hello's program output (the write
/// syscall's console mirror) — proof the whole dispatch ran.
const RUN_HELLO_KEYS: &[(&str, &str)] = &[
    ("h", "h"),
    ("e", "e"),
    ("l", "l"),
    ("l", "l"),
    ("o", "o"),
    ("ret", HELLO_TEXT),
];

/// `hello`, then `linger`, then `crash`. The prompt returns while `linger`
/// is still running. `crash` kills the shell; the next `x` is echoed by the
/// shell the kernel loaded again, and `beat` is still arriving.
const SUPERVISOR_KEYS: &[(&str, &str)] = &[
    ("h", "h"),
    ("e", "e"),
    ("l", "l"),
    ("l", "l"),
    ("o", "o"),
    ("ret", HELLO_TEXT),
    ("l", "l"),
    ("i", "i"),
    ("n", "n"),
    ("g", "g"),
    ("e", "e"),
    ("r", "r"),
    ("ret", "up\n"),
    ("c", "c"),
    ("r", "r"),
    ("a", "a"),
    ("s", "s"),
    ("h", "h"),
    ("ret", "killing the task"),
    // `x` is echoed by the new shell. Sync on the prompt: a bare `x` also
    // matches the `0x` in the loader's log line.
    ("x", "admin@galexy> "),
    ("y", "beat\n"),
];

/// F2 selects the second shell. `echo hi` there, then F1 and a key on the
/// first shell. Serial mirrors only the console on screen, so each side is
/// proven by text that appears after its switch.
const TTY_KEYS: &[(&str, &str)] = &[
    ("f2", "[tty] 2"),
    ("e", "e"),
    ("c", "c"),
    ("h", "h"),
    ("o", "o"),
    ("spc", " "),
    ("h", "h"),
    ("i", "i"),
    ("ret", "hi"),
    ("f1", "[tty] 1"),
    ("z", "z"),
];

/// `stats`, then `threads`, then `tasks`. Each Enter syncs on a line only
/// the query `read` produces (the banner's "frames free" is screen-only).
const QUERY_KEYS: &[(&str, &str)] = &[
    ("s", "s"),
    ("t", "t"),
    ("a", "a"),
    ("t", "t"),
    ("s", "s"),
    ("ret", "frames free:"),
    ("t", "t"),
    ("h", "h"),
    ("r", "r"),
    ("e", "e"),
    ("a", "a"),
    ("d", "d"),
    ("s", "s"),
    ("ret", "main loop:"),
    ("t", "t"),
    ("a", "a"),
    ("s", "s"),
    ("k", "k"),
    ("s", "s"),
    ("ret", "cooperative tasks:"),
    ("l", "l"),
    ("s", "s"),
    ("ret", "banner.txt"),
];

/// `cat`, redirection, `mkdir` / `cd` / `ls`. Enter syncs on text that
/// appears only after the command runs. Utilities use `SPAWN_WAIT`, so a
/// redirected `echo`, `mkdir`, and a successful `rm` finish before the
/// prompt returns; syncs still use the exit line or command output.
const UTIL_KEYS: &[(&str, &str)] = &[
    ("c", "c"),
    ("a", "a"),
    ("t", "t"),
    ("spc", " "),
    ("b", "b"),
    ("a", "a"),
    ("n", "n"),
    ("n", "n"),
    ("e", "e"),
    ("r", "r"),
    ("dot", "."),
    ("t", "t"),
    ("x", "x"),
    ("t", "t"),
    ("ret", "plumbing works"),
    ("e", "e"),
    ("c", "c"),
    ("h", "h"),
    ("o", "o"),
    ("spc", " "),
    ("s", "s"),
    ("c", "c"),
    ("r", "r"),
    ("a", "a"),
    ("t", "t"),
    ("c", "c"),
    ("h", "h"),
    ("minus", "-"),
    ("h", "h"),
    ("i", "i"),
    ("spc", " "),
    ("shift+dot", ">"),
    ("spc", " "),
    ("n", "n"),
    ("o", "o"),
    ("t", "t"),
    ("e", "e"),
    ("ret", "[sched] task 'echo' exited"),
    ("c", "c"),
    ("a", "a"),
    ("t", "t"),
    ("spc", " "),
    ("n", "n"),
    ("o", "o"),
    ("t", "t"),
    ("e", "e"),
    ("ret", "scratch-hi\n"),
    ("e", "e"),
    ("c", "c"),
    ("h", "h"),
    ("o", "o"),
    ("spc", " "),
    ("m", "m"),
    ("o", "o"),
    ("r", "r"),
    ("e", "e"),
    ("spc", " "),
    ("shift+dot", ">"),
    ("shift+dot", ">"),
    ("spc", " "),
    ("n", "n"),
    ("o", "o"),
    ("t", "t"),
    ("e", "e"),
    ("ret", "[sched] task 'echo' exited"),
    ("c", "c"),
    ("a", "a"),
    ("t", "t"),
    ("spc", " "),
    ("n", "n"),
    ("o", "o"),
    ("t", "t"),
    ("e", "e"),
    ("ret", "more\n"),
    ("m", "m"),
    ("k", "k"),
    ("d", "d"),
    ("i", "i"),
    ("r", "r"),
    ("spc", " "),
    ("b", "b"),
    ("o", "o"),
    ("x", "x"),
    ("ret", "[sched] task 'mkdir' exited"),
    ("c", "c"),
    ("d", "d"),
    ("spc", " "),
    ("b", "b"),
    ("o", "o"),
    ("x", "x"),
    ("ret", "admin@galexy:/box> "),
    ("e", "e"),
    ("c", "c"),
    ("h", "h"),
    ("o", "o"),
    ("spc", " "),
    ("i", "i"),
    ("n", "n"),
    ("b", "b"),
    ("o", "o"),
    ("x", "x"),
    ("spc", " "),
    ("shift+dot", ">"),
    ("spc", " "),
    ("l", "l"),
    ("e", "e"),
    ("a", "a"),
    ("f", "f"),
    ("ret", "[sched] task 'echo' exited"),
    ("l", "l"),
    ("s", "s"),
    ("ret", "leaf\n"),
    ("c", "c"),
    ("a", "a"),
    ("t", "t"),
    ("spc", " "),
    ("l", "l"),
    ("e", "e"),
    ("a", "a"),
    ("f", "f"),
    ("ret", "inbox\n"),
    ("c", "c"),
    ("d", "d"),
    ("spc", " "),
    ("dot", "."),
    ("dot", "."),
    ("ret", "admin@galexy> "),
    ("l", "l"),
    ("s", "s"),
    ("ret", "box/"),
    ("c", "c"),
    ("a", "a"),
    ("t", "t"),
    ("spc", " "),
    ("h", "h"),
    ("e", "e"),
    ("l", "l"),
    ("l", "l"),
    ("o", "o"),
    ("ret", "cat: not text"),
    ("e", "e"),
    ("c", "c"),
    ("h", "h"),
    ("o", "o"),
    ("spc", " "),
    ("n", "n"),
    ("o", "o"),
    ("spc", " "),
    ("shift+dot", ">"),
    ("spc", " "),
    ("b", "b"),
    ("a", "a"),
    ("n", "n"),
    ("n", "n"),
    ("e", "e"),
    ("r", "r"),
    ("dot", "."),
    ("t", "t"),
    ("x", "x"),
    ("t", "t"),
    ("ret", "echo: cannot replace"),
    ("r", "r"),
    ("m", "m"),
    ("spc", " "),
    ("n", "n"),
    ("o", "o"),
    ("t", "t"),
    ("e", "e"),
    ("ret", "[sched] task 'rm' exited"),
    ("c", "c"),
    ("a", "a"),
    ("t", "t"),
    ("spc", " "),
    ("n", "n"),
    ("o", "o"),
    ("t", "t"),
    ("e", "e"),
    ("ret", "cat: no such file"),
    ("r", "r"),
    ("m", "m"),
    ("spc", " "),
    ("b", "b"),
    ("o", "o"),
    ("x", "x"),
    ("ret", "rm: directory not empty"),
    ("c", "c"),
    ("d", "d"),
    ("spc", " "),
    ("b", "b"),
    ("o", "o"),
    ("x", "x"),
    ("ret", "admin@galexy:/box> "),
    ("r", "r"),
    ("m", "m"),
    ("spc", " "),
    ("l", "l"),
    ("e", "e"),
    ("a", "a"),
    ("f", "f"),
    ("ret", "[sched] task 'rm' exited"),
    ("c", "c"),
    ("d", "d"),
    ("spc", " "),
    ("dot", "."),
    ("dot", "."),
    ("ret", "admin@galexy> "),
    ("r", "r"),
    ("m", "m"),
    ("spc", " "),
    ("b", "b"),
    ("o", "o"),
    ("x", "x"),
    ("ret", "[sched] task 'rm' exited"),
];

/// Typing `shell` must not start a nested console (reserved name).
const SHELL_RESERVE_KEYS: &[(&str, &str)] = &[
    ("s", "s"),
    ("h", "h"),
    ("e", "e"),
    ("l", "l"),
    ("l", "l"),
    ("ret", "shell: reserved"),
];

/// The ring-3 shell's query caps: typed `stats` / `threads` / `tasks` /
/// `ls` come back as console text (screen + serial), including a live
/// thread and the ramdisk's `banner.txt`.
#[test]
fn shell_query_typing_e2e() {
    let serial = boot_and_type(
        &image("galexy-os"),
        QUERY_KEYS,
        "[boot] main loop ready",
        "",
        Duration::from_millis(30),
        Duration::from_secs(60),
    );
    assert!(
        serial.contains("frames free:"),
        "typed `stats` never produced the frame line; serial:\n{serial}"
    );
    assert!(
        serial.contains("thread-a:"),
        "typed `threads` never listed thread-a; serial:\n{serial}"
    );
    assert!(
        serial.contains("cooperative tasks:"),
        "typed `tasks` never produced the task line; serial:\n{serial}"
    );
    assert!(
        serial.contains("banner.txt"),
        "typed `ls` never listed banner.txt; serial:\n{serial}"
    );
    assert!(
        serial.contains("hello\n"),
        "typed `ls` never listed hello; serial:\n{serial}"
    );
}

/// Typing `shell` is refused: F1–F12 own those task names.
#[test]
fn shell_nested_spawn_refused_e2e() {
    let serial = boot_and_type(
        &image("galexy-os"),
        SHELL_RESERVE_KEYS,
        "[boot] main loop ready",
        "shell: reserved",
        Duration::from_millis(30),
        Duration::from_secs(45),
    );
    assert!(
        serial.contains("shell: reserved"),
        "typed `shell` was not refused; serial:\n{serial}"
    );
    assert!(
        !serial.contains("keyboard denied"),
        "typed `shell` started a keyboard-less shell; serial:\n{serial}"
    );
}

/// `cat banner.txt`, a scratch file written with `echo`, and `mkdir` /
/// `cd` / `ls`. `hello` is not console text. A tar name cannot be replaced.
/// `rm` drops a scratch file and refuses a directory that still has a child.
#[test]
fn shell_util_typing_e2e() {
    let serial = boot_and_type(
        &image("galexy-os"),
        UTIL_KEYS,
        "[boot] main loop ready",
        "rm: directory not empty",
        Duration::from_millis(30),
        Duration::from_secs(150),
    );
    assert!(
        serial.contains("plumbing works"),
        "typed `cat banner.txt` missed the archive text; serial:\n{serial}"
    );
    assert!(
        serial.contains("scratch-hi\n"),
        "typed `echo >` did not round-trip; serial:\n{serial}"
    );
    assert!(
        serial.contains("more\n"),
        "typed `echo >>` did not append; serial:\n{serial}"
    );
    assert!(
        serial.contains("admin@galexy:/box> "),
        "typed `cd box` did not enter the directory; serial:\n{serial}"
    );
    assert!(
        serial.contains("inbox\n"),
        "typed create/write/read inside the directory failed; serial:\n{serial}"
    );
    assert!(
        serial.contains("box/"),
        "typed `ls` after `cd ..` missed the directory; serial:\n{serial}"
    );
    assert!(
        serial.contains("cat: not text"),
        "typed `cat hello` should fail the console charset; serial:\n{serial}"
    );
    assert!(
        serial.contains("echo: cannot replace"),
        "typed `echo > banner.txt` should be rejected; serial:\n{serial}"
    );
    assert!(
        serial.contains("cat: no such file"),
        "typed `rm note` should drop the scratch file; serial:\n{serial}"
    );
    assert!(
        serial.contains("rm: directory not empty"),
        "typed `rm box` should refuse a directory that still has a child; serial:\n{serial}"
    );
}

/// True end-to-end: TYPES `hello` into the running kernel through
/// QEMU's QMP `send-key` (real PS/2 IRQs into the keyboard driver) and
/// asserts the user program's console output on COM1 (screen+serial
/// mirror make the result observable headless).
#[test]
fn shell_run_hello_typing_e2e() {
    // Typing starts only after the kernel's main loop (the shell's key
    // consumer) is live — serial marker, not a sleep: init timing under
    // TCG varies. Each key syncs on the guest's echo, so host load can
    // never overflow the i8042 queue between keys.
    let serial = boot_and_type(
        &image("galexy-os"),
        SUPERVISOR_KEYS,
        "[boot] main loop ready",
        "beat\n",
        Duration::from_millis(30),
        Duration::from_secs(90),
    );
    assert!(
        serial.contains(HELLO_TEXT),
        "typed `hello` never produced user output; serial:\n{serial}"
    );
    assert!(
        serial.contains("exited (syscall)"),
        "user task exit marker missing after typed run; serial:\n{serial}"
    );
    let fault_at = serial
        .find("[pf] ring-3 task fault")
        .expect("typed `crash` never faulted the shell");
    let after_fault = &serial[fault_at..];
    let prompt_at = after_fault.find("admin@galexy> ").unwrap_or_else(|| {
        panic!("a new shell prompt never appeared after the fault; serial:\n{serial}")
    });
    assert!(
        after_fault[prompt_at..].contains("beat\n"),
        "linger stopped printing after the shell restarted; serial:\n{serial}"
    );
}

/// True end-to-end under UEFI: the same `hello` typing flow, into the
/// OVMF-booted kernel — the APIC delivery path (LAPIC timer + I/O APIC
/// keyboard) drives the whole thing there. OVMF boot flakiness → 3 retries.
#[test]
fn shell_run_hello_typing_e2e_uefi() {
    let image = image("galexy-os");
    let mut last = String::new();
    for _ in 0..3 {
        last = boot_and_type_uefi(
            &image,
            RUN_HELLO_KEYS,
            "[boot] main loop ready",
            "exited (syscall)",
            Duration::from_millis(30),
            Duration::from_secs(90),
        );
        if last.contains(HELLO_TEXT) && last.contains("exited (syscall)") {
            break;
        }
    }
    assert!(
        last.contains(HELLO_TEXT),
        "typed `hello` under UEFI never produced user output; serial:\n{last}"
    );
    assert!(
        last.contains("exited (syscall)"),
        "user task exit marker missing after typed run (UEFI); serial:\n{last}"
    );
}

/// F2 runs a command on the second shell; F1 returns to the first.
#[test]
fn shell_tty_switch_e2e() {
    let serial = boot_and_type(
        &image("galexy-os"),
        TTY_KEYS,
        "[boot] main loop ready",
        "hi",
        Duration::from_millis(30),
        Duration::from_secs(90),
    );
    let tty2 = serial.find("[tty] 2").expect("F2 never switched consoles");
    let hi = serial[tty2..]
        .find("hi")
        .expect("echo on tty 2 never printed");
    let tty1 = serial[tty2 + hi..]
        .find("[tty] 1")
        .expect("F1 never switched back");
    assert!(
        serial[tty2 + hi + tty1..].contains('z'),
        "the first shell did not echo after F1; serial:\n{serial}"
    );
}

#[test]
fn smp_test_passes() {
    let (code, serial) = boot(&image("test-smp"));
    assert_eq!(
        code,
        Some(QEMU_EXIT_SUCCESS),
        "test-smp should exit with Success; serial:\n{serial}"
    );
    assert!(
        serial.contains("[test-smp] passed"),
        "test-smp success marker missing; serial:\n{serial}"
    );
    assert!(
        serial.contains("[test-smp] online=2"),
        "the SMP substrate must count 2 PCs; serial:\n{serial}"
    );
    assert!(
        serial.contains("owners: t1=0 t2=1 t3=0 t4=1"),
        "pin-at-spawn distribution marker missing; serial:\n{serial}"
    );
}

#[test]
fn ipi_test_passes() {
    // The shootdown IPI machinery (M19): broadcast + lock-free ack, end to
    // end across the two CPUs the harness always provides.
    let (code, serial) = boot(&image("test-ipi"));
    assert_eq!(
        code,
        Some(QEMU_EXIT_SUCCESS),
        "test-ipi should exit with Success; serial:\n{serial}"
    );
    assert!(
        serial.contains("[test-ipi] passed"),
        "test-ipi success marker missing; serial:\n{serial}"
    );
}

#[test]
fn smpuser_test_passes() {
    // The ring-3 blobs print via the console mirror (screen + serial), so
    // the fact the AP-side task's WRITE syscall ran is visible headless.
    let (code, serial) = boot(&image("test-smpuser"));
    assert_eq!(
        code,
        Some(QEMU_EXIT_SUCCESS),
        "test-smpuser should exit with Success; serial:\n{serial}"
    );
    assert!(
        serial.contains("[test-smpuser] passed"),
        "test-smpuser success marker missing; serial:\n{serial}"
    );
    // Ring-3 completion proof, matching the milestone pattern (the blob
    // prints to the SCREEN; the observe-headless path is the scratch page
    // — see test-user's design note): both DONE marks must appear.
    assert!(
        serial.contains("[test-smpuser] task 0 marked done")
            && serial.contains("[test-smpuser] task 1 marked done"),
        "both ring-3 tasks must reach their scratch-mark (both CPUs ran a program); serial:\n{serial}"
    );
}

#[test]
fn smpstress_test_passes() {
    // M19 stress: idle-CPU steal proof + concurrent heap growth (shootdown
    // crossings) + hammering with exact frame closure.
    let (code, serial) = boot(&image("test-smpstress"));
    assert_eq!(
        code,
        Some(QEMU_EXIT_SUCCESS),
        "test-smpstress should exit with Success; serial:\n{serial}"
    );
    assert!(
        serial.contains("[test-smpstress] passed"),
        "test-smpstress success marker missing; serial:\n{serial}"
    );
    assert!(
        serial.contains("stole 'spinner'"),
        "the steal proof must show the owner flip; serial:\n{serial}"
    );
}

#[test]
fn uefi_image_boots_and_timer_ticks() {
    // Since the APIC work (M17), the timer is LAPIC-delivered on every boot
    // path — the UEFI image is live, not just booting: assert the heartbeat.
    //
    // OVMF's first-boot device enumeration is flaky under QEMU (the disk is
    // sometimes "Not Found" when BDS builds boot options) — retry up to 3x.
    let image = image("galexy-os");
    let mut last = String::new();
    for _ in 0..3 {
        last = boot_uefi(&image, Duration::from_secs(45));
        if last.contains("[timer] 1s up") {
            break;
        }
    }
    assert!(
        last.contains("boot info: rsdp_addr"),
        "UEFI boot info marker missing after 3 attempts; serial:\n{last}"
    );
    assert!(
        last.contains("[mm] frame allocator ready"),
        "UEFI memory bring-up marker missing; serial:\n{last}"
    );
    assert!(
        last.contains("[timer] 1s up"),
        "UEFI timer heartbeat missing (LAPIC timer must tick under OVMF); serial:\n{last}"
    );
    assert!(
        last.contains("[acpi] madt ready"),
        "UEFI MADT discovery marker missing; serial:\n{last}"
    );
    assert!(
        last.contains("[apic] lapic up"),
        "UEFI LAPIC enable marker missing; serial:\n{last}"
    );
}

#[test]
fn main_kernel_boots_and_timer_ticks() {
    // The interactive kernel never exits; verify liveness markers instead.
    // (Generous window: TCG boot + banner render stretch badly when the
    // host is loaded — 20 s was observed to cut the first heartbeat off.)
    let serial = boot_liveness(&image("galexy-os"), Duration::from_secs(45));
    assert!(
        serial.contains("boot info: rsdp_addr"),
        "boot info marker missing; serial:\n{serial}"
    );
    assert!(
        serial.contains("[timer] 1s up"),
        "timer heartbeat missing; serial:\n{serial}"
    );
    assert!(
        serial.contains("[loader] program 'shell' ready"),
        "ring-3 shell was not spawned; serial:\n{serial}"
    );
}
