//! x86_64 platform support: GDT/TSS, IDT, PICs, PIT timer.
//!
//! This is **the port wall**: the only place in the kernel that touches
//! hardware ports, CPU registers, or platform specifics. Drivers and kernel
//! primitives call `arch` APIs; they never write to ports themselves.
//! Porting to another architecture means replacing this module tree — and
//! only this module tree.

pub mod gdt;
mod idt;
pub mod mm;
mod pics;
pub mod syscall;
pub mod timer;

pub use gdt::{set_tss_rsp0, syscall_selectors, tss_rsp0, user_cs_ss};
pub use idt::set_page_fault_handler;
pub use timer::tick as timer_tick;
pub use timer::ticks as timer_ticks;

/// Signals end-of-interrupt for the timer vector (called by the timer
/// switch before entering the next task).
pub fn end_timer_interrupt() {
    pics::end_of_interrupt(pics::TIMER_INTERRUPT_ID);
}

/// Brings up the whole interrupt subsystem and enables interrupts.
pub fn init() {
    gdt::init();
    syscall::init();
    idt::init();
    pics::init();
    timer::init();
    x86_64::instructions::interrupts::enable();
}
