//! Interrupt subsystem: GDT/TSS, IDT, PICs, timer and keyboard handlers.
//!
//! Init order matters: GDT first (segment registers + TSS stacks), then IDT
//! (handlers for faults while setting up), then PICs (unmask), then enable.

mod gdt;
mod idt;
mod pics;
mod timer;

/// Initializes the whole interrupt subsystem and enables interrupts.
pub fn init() {
    gdt::init();
    idt::init();
    pics::init();
    timer::init();
    x86_64::instructions::interrupts::enable();
}
