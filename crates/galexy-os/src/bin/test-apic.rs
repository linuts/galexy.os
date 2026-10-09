//! Integration test kernel: LAPIC bring-up — detection, register roundtrip,
//! mode/ID sanity, and one-shot timer arming (tickless / deadline path).

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
    // arch::init parses the MADT and enables the LAPIC. x2APIC is the MSR
    // path when CPUID reports it (`-cpu max,+x2apic`).
    arch::init(boot_info);

    // 1. Mode follows CPUID. The runner passes `+x2apic`, so this is the
    //    MSR path (no MMIO page).
    let mode = apic::mode();
    serial_println!("[test-apic] detected mode: {:?}", mode);
    let (_eax, _ebx, ecx, _edx) = arch::cpu::cpuid(1, 0);
    if ecx & (1 << 21) != 0 {
        assert_eq!(
            mode,
            apic::LapicMode::X2Apic,
            "CPUID x2APIC must take the MSR path"
        );
    } else {
        assert_eq!(mode, apic::LapicMode::XApic, "no x2APIC bit stays on MMIO");
        assert_eq!(
            apic::lapic_page().as_u64(),
            (200u64 << 39),
            "LAPIC page must live at the fixed P4-200 mapping"
        );
    }
    let id = apic::lapic_id();
    serial_println!("[test-apic] lapic id: {}", id);
    assert!(id <= 0x0F, "LAPIC id must be a small logical id, got {id}");

    // 3. The LAPIC timer is LIVE on vector 32. TSC-deadline (LVT bits
    //    18:17 = 10b) when CPUID.1 ECX bit 24 is set, else one-shot.
    const REG_LVT_TIMER: u32 = 0x320;
    let lvt = apic::reg(REG_LVT_TIMER);
    serial_println!("[test-apic] LVT timer: {:#x}", lvt);
    assert_eq!(
        lvt & 0xFF,
        u32::from(apic::timer_interrupt_id()),
        "timer must deliver on vector 32"
    );
    assert_eq!(lvt & (1 << 16), 0, "the timer LVT must be unmasked");
    if ecx & (1 << 24) != 0 {
        assert_eq!(lvt & (3 << 17), 2 << 17, "TSC-deadline mode (CPUID bit 24)");
    } else {
        assert_eq!(lvt & (3 << 17), 0, "the timer LVT must be one-shot");
    }
    let tpm = apic::ticks_per_ms();
    serial_println!("[test-apic] calibrated: {} ticks/ms", tpm);
    assert!(tpm >= 1, "calibration must yield at least 1 tick/ms");

    // 4. Timer liveness: ticks accrue through the LAPIC delivery path.
    //    Re-arm a short deadline so we do not depend on a prior quantum
    //    that may already have expired between `arch::init` and here.
    apic::arm_oneshot_ms(2);
    let t0 = arch::timer_ticks();
    while arch::timer_ticks() == t0 {
        x86_64::instructions::hlt();
    }
    serial_println!("[test-apic] timer ticking (LAPIC one-shot)");

    println!("[test-apic] lapic registers verified");
    println!("[test-apic] all assertions passed");
    serial_println!("[test-apic] passed");
    exit_qemu(QemuExitCode::Success);
}
