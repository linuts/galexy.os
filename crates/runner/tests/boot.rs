//! Boot-level integration tests: each boots a galexy.os kernel image in
//! headless QEMU and asserts on the exit code and COM1 output.

mod common;

use common::{boot, boot_liveness, image, QEMU_EXIT_SUCCESS};
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
fn main_kernel_boots_and_timer_ticks() {
    // The interactive kernel never exits; verify liveness markers instead.
    let serial = boot_liveness(&image("galexy-os"), Duration::from_secs(20));
    assert!(
        serial.contains("boot info: rsdp_addr"),
        "boot info marker missing; serial:\n{serial}"
    );
    assert!(
        serial.contains("[timer] 1s up"),
        "timer heartbeat missing; serial:\n{serial}"
    );
}
