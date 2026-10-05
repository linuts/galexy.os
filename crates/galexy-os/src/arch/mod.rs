//! x86_64 platform support: ACPI discovery, GDT/TSS, IDT, PICs, PIT timer.
//!
//! This is **the port wall**: the only place in the kernel that touches
//! hardware ports, CPU registers, or platform specifics. Drivers and kernel
//! primitives call `arch` APIs; they never write to ports themselves.
//! Porting to another architecture means replacing this module tree — and
//! only this module tree.

pub mod acpi;
pub mod apic;
pub mod cpu;
pub mod gdt;
mod idt;
pub mod ioapic;
pub mod mm;
mod pics;
pub mod syscall;
pub mod timer;

use bootloader_api::info::BootInfo;

pub use acpi::madt;
pub use gdt::{set_tss_rsp0, syscall_selectors, tss_rsp0, user_cs_ss};
pub use idt::set_page_fault_handler;
pub use timer::tick as timer_tick;
pub use timer::ticks as timer_ticks;

/// Signals end-of-interrupt for the timer vector (called by the timer
/// switch before entering the next task). Since the LAPIC-timer commit the
/// LAPIC is the delivery path — its EOI register is the one true EOI.
pub fn end_timer_interrupt() {
    apic::eoi();
}

/// Brings up the whole interrupt subsystem and enables interrupts.
pub fn init(boot_info: &BootInfo) {
    // ACPI discovery must run FIRST: the per-CPU bring-up reads the BSP's
    // APIC ID from the MADT. It needs nothing else (the phys map exists
    // from boot, fixed in BOOTLOADER_CONFIG).
    let phys_offset = boot_info
        .physical_memory_offset
        .into_option()
        .expect("physical memory must be mapped (see BOOTLOADER_CONFIG)");
    acpi::init(boot_info.rsdp_addr.into_option(), phys_offset);

    gdt::init();
    // Per-CPU substrate AFTER gdt::init (the GS selector load resets the
    // GS base — WRGSBASE must be the last GS-base writer).
    cpu::init_bsp();
    syscall::init();
    idt::init();
    apic::init(acpi::madt().lapic_base());
    // Legacy PICs remapped + fully masked (APIC delivers from here on);
    // then the I/O APIC wires the keyboard line onto its vector.
    pics::init();
    ioapic::init();
    // Shootdown IPI machinery (lazy until the first broadcast; the IDT gate
    // is registered in idt::init).
    mm::shootdown::init();
    // The i8042 first port enable + stale-buffer drain (moved out of the
    // PIC's init — the keyboard driver owns its controller now).
    crate::drivers::keyboard::init();
    timer::init();
    // AP bring-up runs LAST, still with IRQs off: the trampoline sequence
    // waits on global clock ports (PIT delays) and IPIs from the BSP.
    cpu::boot_aps();
    x86_64::instructions::interrupts::enable();
}
