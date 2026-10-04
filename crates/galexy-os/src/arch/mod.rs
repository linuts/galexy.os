//! x86_64 platform support: GDT/TSS, IDT, PICs, PIT timer.
//!
//! This is **the port wall**: the only place in the kernel that touches
//! hardware ports, CPU registers, or platform specifics. Drivers and kernel
//! primitives call `arch` APIs; they never write to ports themselves.
//! Porting to another architecture means replacing this module tree — and
//! only this module tree.

mod gdt;
mod idt;
pub mod mm;
mod pics;
mod timer;

pub use idt::set_page_fault_handler;

/// Initializes the whole interrupt subsystem and enables interrupts.
pub fn init() {
    gdt::init();
    idt::init();
    pics::init();
    timer::init();
    x86_64::instructions::interrupts::enable();
}
