//! Integration test kernel: ACPI discovery — RSDP → XSDT/RSDT → MADT.
//! Asserts the parsed table is sane under QEMU (LAPIC at the classic
//! 0xFEE00000 base, an I/O APIC present, ISA overrides parsed, ≥1 CPU).

#![no_std]
#![no_main]

use bootloader_api::{entry_point, BootInfo};
use galexy_os::{
    arch,
    drivers::screen,
    exit_qemu, println, serial_println, QemuExitCode,
};

entry_point!(test_main_entry, config = &galexy_os::BOOTLOADER_CONFIG);

fn test_main_entry(boot_info: &'static mut BootInfo) -> ! {
    galexy_os::init();
    screen::init(boot_info);
    println!("[test-acpi] running");
    serial_println!("[test-acpi] running");

    // arch::init walks RSDP → root table → MADT (checksums enforced on the
    // way). A malformed table would already have panicked the kernel.
    arch::init(boot_info);

    let madt = arch::madt();
    let lapic = madt.lapic_base();
    let ioapic = madt.ioapic_base();
    let gsi_base = madt.ioapic_gsi_base();
    let cpus = madt.cpus();
    let bsp = madt.boot_cpu_apic_id();
    serial_println!(
        "[test-acpi] madt: lapic {:#x}, ioapic {:#x}, gsi_base {}, cpus {}, bsp {}",
        lapic,
        ioapic,
        gsi_base,
        cpus,
        bsp
    );

    // QEMU's being-sane assertions: the LAPIC lives at the classic base
    // under both SeaBIOS and OVMF, the boot I/O APIC covers GSI 0, and at
    // least one CPU is enabled.
    assert_eq!(lapic, 0xFEE0_0000, "LAPIC base must be the classic MMIO base");
    assert_eq!(gsi_base, 0, "boot I/O APIC must cover GSI 0");
    assert!(cpus >= 1, "at least the BSP must be enabled in the MADT");
    assert!(
        ioapic != 0,
        "an I/O APIC with a nonzero MMIO base must exist"
    );

    // ISA overrides: under QEMU, IRQ0 (timer) is overridden to GSI 2 (the
    // classic i8202 PIC remap inherited by the platforms); IRQ1 (keyboard)
    // stays identity-mapped. Publish what we saw for the serial record.
    let gsi0 = madt.isa_gsi(0);
    let gsi1 = madt.isa_gsi(1);
    serial_println!(
        "[test-acpi] isa gsi: irq0 -> gsi {}, irq1 -> gsi {}",
        gsi0,
        gsi1
    );
    assert_eq!(gsi0, 2, "QEMU overrides the PIT's ISA line to GSI 2");
    assert_eq!(gsi1, 1, "the keyboard's ISA line stays identity-mapped");

    println!("[test-acpi] madt parse verified");
    println!("[test-acpi] all assertions passed");
    serial_println!("[test-acpi] passed");
    exit_qemu(QemuExitCode::Success);
}
