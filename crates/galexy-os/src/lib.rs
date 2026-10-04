//! galexy.os kernel library: everything the kernel binaries share.
//!
//! Binaries under `src/bin/` (the normal kernel and the test kernels) link
//! this library and supply only their entry function. The panic handler and
//! the QEMU exit device live here, so any binary gets identical panic and
//! exit behavior.

#![no_std]
#![feature(abi_x86_interrupt)]
#![deny(clippy::all)]

pub mod arch;
pub mod drivers;
pub mod echo;
pub mod sched;

#[macro_use]
mod macros;

use core::panic::PanicInfo;
use spin::Mutex;
use x86_64::instructions::port::Port;

/// Exit codes delivered to QEMU through the `isa-debug-exit` device
/// (registered by the runner at I/O port `0xF4`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum QemuExitCode {
    /// Test (or normal run) completed successfully.
    Success = 0x10,
    /// Test failed or an unexpected panic occurred.
    Failed = 0x11,
}

/// Exits QEMU via the `isa-debug-exit` device.
///
/// QEMU maps the written value to its process exit code; the runner-side
/// tests expect the raw codes (Success → 33, Failed → 35).
pub fn exit_qemu(exit_code: QemuExitCode) -> ! {
    // SAFETY: fixed exit-device port; the device exists whenever the runner
    // boots us, and is inert (unknown port) on real hardware.
    unsafe {
        Port::<u32>::new(0xF4).write(exit_code as u32);
    }
    // Exit is asynchronous from QEMU's perspective; never return.
    loop {
        x86_64::instructions::hlt();
    }
}

/// Marker a test kernel registers with [`expect_panic`] before triggering an
/// intentional panic; the panic handler matches it against the panic location.
static EXPECTED_PANIC: Mutex<Option<&'static str>> = Mutex::new(None);

/// Registers that the next panic is intentional and must occur in a file
/// whose path contains `marker`; such panics exit QEMU with Success.
pub fn expect_panic(marker: &'static str) {
    *EXPECTED_PANIC.lock() = Some(marker);
}

/// Brings up subsystems shared by every kernel binary.
pub fn init() {
    drivers::serial::init();
}

/// Panic handler: reports over serial (safe while other locks may be held),
/// then exits QEMU — Failed normally, Success if the panic was expected.
///
/// The `exit_qemu` port is inert on real hardware; a panicking kernel on
/// bare metal simply parks after the write.
#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    serial_println!("[PANIC] {}", info);
    let expected = EXPECTED_PANIC.lock().take();
    let location = info.location().map(|loc| loc.file());
    if let (Some(marker), Some(file)) = (expected, location) {
        if file.contains(marker) {
            exit_qemu(QemuExitCode::Success);
        }
    }
    exit_qemu(QemuExitCode::Failed);
}
