//! galexy.os kernel library: everything the kernel binaries share.
//!
//! Binaries under `src/bin/` (the normal kernel and the test kernels) link
//! this library and supply only their entry function. The panic handler and
//! the QEMU exit device live here, so any binary gets identical panic and
//! exit behavior.

#![no_std]
#![feature(abi_x86_interrupt)]
#![deny(clippy::all)]

// Kernel heap lives in `arch::mm::heap`; this makes `alloc` (String, Vec,
// Box, ...) usable everywhere in the kernel.
extern crate alloc;

// Textual order matters: declare macros first so every module below can
// use print!/println!.
#[macro_use]
mod macros;

pub mod arch;
pub mod banner;
pub mod drivers;
pub mod sched;
pub mod shell;
pub mod sync;

use crate::sync::Mutex;
use bootloader_api::config::{BootloaderConfig, Mapping};
use core::panic::PanicInfo;
use x86_64::instructions::port::Port;

/// Bootloader configuration shared by every kernel binary.
///
/// Requests fixed mappings for physical memory (gives us
/// `BootInfo::physical_memory_offset`) and the recursive page table
/// (groundwork for the paging phase).
pub static BOOTLOADER_CONFIG: BootloaderConfig = {
    let mut config = BootloaderConfig::new_default();
    config.kernel_stack_size = 256 * 1024; // default 80 KiB is tight for mm init
    config.mappings.physical_memory = Some(Mapping::FixedAddress(0x0000_4000_0000_0000));
    // Canonical, 512-GiB-aligned address whose P4 index is 511: the classic
    // top-of-address-space recursive page-table mapping.
    config.mappings.page_table_recursive =
        Some(Mapping::FixedAddress((0xFFFF << 48) | (511 << 39)));
    // Randomize dynamic mappings (kernel image, stack, framebuffer,
    // ramdisk, boot info) inside P4 indexes 1..=24. That band sits
    // below the user image (index 25) and clear of the kernel's fixed
    // slots: physical memory 128, heap 170, LAPIC 200, I/O APIC 201.
    // The recursive map is index 511. The kernel is a PIE. The BIOS
    // stage-4 stack patch in `third_party/` is what makes `aslr` boot.
    config.mappings.aslr = true;
    config.mappings.dynamic_range_start = Some(1 << 39);
    config.mappings.dynamic_range_end = Some((25 << 39) - 1);
    config
};

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

/// Registers that the next panic is intentional: the panic handler matches
/// `marker` against the panic location's file path or the panic message; a
/// match exits QEMU with Success instead of Failed.
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
    let message = info.message().as_str();
    let matched = expected.is_some_and(|marker| {
        location.is_some_and(|file| file.contains(marker))
            || message.is_some_and(|msg| msg.contains(marker))
    });
    if matched {
        exit_qemu(QemuExitCode::Success);
    }
    exit_qemu(QemuExitCode::Failed);
}
