//! Integration test kernel: the shootdown IPI machinery, end to end (M19
//! commit 1). Asserts — under QEMU `-smp 2` — that:
//! 1. Both CPUs are online (a real cross-CPU broadcast, not a no-op).
//! 2. `shootdown_others` round-trips: the BSP broadcasts kernel-half VAs,
//!    the AP's lock-free handler INVLPGs them and acks via the per-CPU
//!    `seen` matrix, and the initiator's wait converges (returns).
//! 3. Sequence numbers advance and the completed-broadcast count grows.

#![no_std]
#![no_main]

extern crate alloc;

use bootloader_api::{entry_point, BootInfo};
use galexy_os::{arch, drivers::screen, exit_qemu, println, serial_println, QemuExitCode};
use x86_64::VirtAddr;

entry_point!(test_main_entry, config = &galexy_os::BOOTLOADER_CONFIG);

fn test_main_entry(boot_info: &'static mut BootInfo) -> ! {
    galexy_os::init();
    screen::init(boot_info);
    println!("[test-ipi] running");
    serial_println!("[test-ipi] running");

    arch::mm::init(boot_info);
    arch::init(boot_info);

    // 1: both CPUs online — otherwise the "broadcast" below would be a
    // self-targeted no-op and prove nothing.
    assert_eq!(
        arch::cpu::online(),
        2,
        "both CPUs must be online under -smp 2"
    );

    // 2: round-trip a broadcast. The VA choice is deliberately arbitrary:
    // INVLPG is idempotent and safe for any canonical VA (mapped, unmapped,
    // stale — all fine). Flushing a mapped kernel page costs one TLB miss
    // later, which is the whole point of the machinery.
    let probe = arch::mm::heap::stats().0; // the heap's fixed base VA
    let seq1 = arch::mm::shootdown::shootdown_others(&[VirtAddr::new(probe)]);
    serial_println!("[test-ipi] broadcast 1 done (seq {})", seq1);
    assert!(seq1 >= 1, "sequence numbers start at 1");
    assert_eq!(
        arch::mm::shootdown::broadcast_count(),
        1,
        "one completed broadcast"
    );

    // The AP must have consumed the request: its `seen` for the slot we
    // (as the first initiator) used reaches our seq.
    let seen = arch::mm::shootdown::seen_by(1);
    assert!(
        seen.iter().any(|&s| s >= seq1),
        "AP's seen must reach the broadcast seq: {seen:?}"
    );

    // 3: a second, multi-VA broadcast (a full slot's worth) round-trips too.
    let vas: alloc::vec::Vec<VirtAddr> = (0..16)
        .map(|i| VirtAddr::new(probe + (i as u64) * 0x1000))
        .collect();
    let seq2 = arch::mm::shootdown::shootdown_others(&vas);
    serial_println!("[test-ipi] broadcast 2 done (seq {})", seq2);
    assert!(seq2 > seq1, "sequence numbers advance strictly");
    assert_eq!(
        arch::mm::shootdown::broadcast_count(),
        2,
        "two completed broadcasts"
    );
    let seen = arch::mm::shootdown::seen_by(1);
    assert!(
        seen.iter().any(|&s| s >= seq2),
        "AP's seen must reach the second seq: {seen:?}"
    );

    println!("[test-ipi] all assertions passed");
    serial_println!("[test-ipi] passed");
    exit_qemu(QemuExitCode::Success);
}
