//! x86_64 platform support: ACPI discovery, GDT/TSS, IDT, PICs, PIT timer.
//!
//! This is **the port wall**: the only place in the kernel that touches
//! hardware ports, CPU registers, or platform specifics. Drivers and kernel
//! primitives call `arch` APIs; they never write to ports themselves.
//! Porting to another architecture means replacing this module tree — and
//! only this module tree.

pub mod acpi;
pub mod apic;
pub mod gdt;
mod idt;
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
    gdt::init();
    syscall::init();
    idt::init();
    // ACPI discovery runs before any controller init: the APIC bring-up
    // consumes the MADT, and the phys map (fixed in BOOTLOADER_CONFIG)
    // exists from boot, so even before-mm::init kernels can walk tables.
    // (The offset comes from BootInfo, not the mm module, to keep this
    // independent of init order.)
    let phys_offset = boot_info
        .physical_memory_offset
        .into_option()
        .expect("physical memory must be mapped (see BOOTLOADER_CONFIG)");
    acpi::init(boot_info.rsdp_addr.into_option(), phys_offset);
    // LAPIC enable is behavior-neutral until the timer/IOAPIC wiring lands
    // (M17 commits in sequence); the PIC still delivers everything today.
    apic::init(acpi::madt().lapic_base());
    pics::init();
    timer::init();
    x86_64::instructions::interrupts::enable();
}
