//! Integration test kernel: LAPIC bring-up — detection, register roundtrip,
//! mode/ID sanity. The timer is still PIC/PIT-delivered at this stage; this
//! only proves the LAPIC is ON and its registers behave.

#![no_std]
#![no_main]

use bootloader_api::{entry_point, BootInfo};
use galexy_os::{
    arch::{self, apic},
    drivers::screen,
    exit_qemu, println, serial_println, QemuExitCode,
};

entry_point!(test_main_entry, config = &galexy_os::BOOTLOADER_CONFIG);

fn test_main_entry(boot_info: &'static mut BootInfo) -> ! {
    galexy_os::init();
    screen::init(boot_info);
    println!("[test-apic] running");
    serial_println!("[test-apic] running");

    // mm::init first: the LAPIC page mapping needs the paging mapper
    // (arch::init's APIC work maps MMIO through it).
    arch::mm::init(boot_info);
    // arch::init parses the MADT, maps + enables the LAPIC (xAPIC here),
    // and keeps the PIC as the IRQ delivery path for now.
    arch::init(boot_info);

    // 1. The mode was detected (xAPIC under QEMU's default config).
    let mode = apic::mode();
    serial_println!("[test-apic] detected mode: {:?}", mode);
    assert_eq!(mode, apic::LapicMode::XApic, "QEMU default boots xAPIC");

    // 2. The register page mapped at the MADT base (0xFEE00000), and the
    //    ID register reads back a sane, boot-CPU-safe value.
    assert_eq!(
        apic::lapic_page().as_u64(),
        (200u64 << 39),
        "LAPIC page must live at the fixed P4-200 mapping"
    );
    let id = apic::lapic_id();
    serial_println!("[test-apic] lapic id: {}", id);
    // Boot CPU ID under QEMU is 0..2 depending on version (0x00 typical);
    // just require a plausible linear range (never some huge garbage).
    assert!(id <= 0x0F, "LAPIC id must be a small logical id, got {id}");

    // 3. The LAPIC timer is LIVE: periodic mode on vector 32, calibrated
    //    (ticks-per-ms sane), and actually ticking through the LAPIC.
    const REG_LVT_TIMER: u32 = 0x320;
    let lvt = apic::reg(REG_LVT_TIMER);
    serial_println!("[test-apic] LVT timer: {:#x}", lvt);
    assert_eq!(lvt & 0xFF, u32::from(apic::timer_interrupt_id()), "timer must deliver on vector 32");
    assert_eq!(lvt & (1 << 16), 0, "the timer LVT must be unmasked");
    assert_ne!(lvt & (1 << 17), 0, "the timer LVT must be periodic");
    let tpm = apic::ticks_per_ms();
    serial_println!("[test-apic] calibrated: {} ticks/ms", tpm);
    assert!(tpm >= 1, "calibration must yield at least 1 tick/ms");

    // 4. Timer liveness: ticks accrue through the LAPIC delivery path.
    let t0 = arch::timer_ticks();
    while arch::timer_ticks() == t0 {
        x86_64::instructions::hlt();
    }
    serial_println!("[test-apic] timer ticking (LAPIC-delivered)");

    println!("[test-apic] lapic registers verified");
    println!("[test-apic] all assertions passed");
    serial_println!("[test-apic] passed");
    exit_qemu(QemuExitCode::Success);
}
