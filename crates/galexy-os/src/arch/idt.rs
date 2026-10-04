//! Interrupt Descriptor Table: fault and device interrupt handlers.

use spin::{LazyLock, Mutex};
use x86_64::instructions::port::Port;
use x86_64::structures::idt::{InterruptDescriptorTable, InterruptStackFrame, PageFaultErrorCode};

use super::gdt;
use super::pics::{KEYBOARD_INTERRUPT_ID, TIMER_INTERRUPT_ID};
use super::{pics, timer};
use crate::drivers::keyboard;

/// The IDT sits behind a mutex so tests (and later, demand paging) can
/// replace handlers at runtime; loading uses `load_unsafe` because the
/// static's identity guarantees the lifetime.
static IDT: LazyLock<Mutex<InterruptDescriptorTable>> = LazyLock::new(|| {
    let mut idt = InterruptDescriptorTable::new();
    idt.breakpoint.set_handler_fn(breakpoint_handler);
    idt.page_fault.set_handler_fn(page_fault_handler);
    // SAFETY: index 0 is a valid IST slot we reserved in the TSS.
    unsafe {
        idt.double_fault
            .set_handler_fn(double_fault_handler)
            .set_stack_index(gdt::DOUBLE_FAULT_IST_INDEX);
    }
    // Legacy PIC vectors: IRQ0 = timer, IRQ1 = keyboard.
    idt[TIMER_INTERRUPT_ID].set_handler_fn(timer_handler);
    idt[KEYBOARD_INTERRUPT_ID].set_handler_fn(keyboard_handler);
    Mutex::new(idt)
});

/// Loads the IDT.
pub fn init() {
    let idt = IDT.lock();
    // SAFETY: the IDT is never moved (it lives in the static) — the lifetime
    // constraint `load` normally enforces holds.
    unsafe { idt.load_unsafe() };
}

extern "x86-interrupt" fn breakpoint_handler(stack_frame: InterruptStackFrame) {
    crate::serial_println!("EXCEPTION: BREAKPOINT\n{:#?}", stack_frame);
}

extern "x86-interrupt" fn page_fault_handler(
    stack_frame: InterruptStackFrame,
    error_code: PageFaultErrorCode,
) {
    use x86_64::registers::control::Cr2;
    let faulting_address = Cr2::read();
    crate::serial_println!(
        "EXCEPTION: PAGE FAULT ({:?}) while accessing {:#?}\n{:#?}",
        error_code,
        faulting_address,
        stack_frame
    );
    // The faulting instruction would just fault again; park here.
    loop {
        x86_64::instructions::hlt();
    }
}

/// Installs `handler` as the page-fault handler (replacing the default
/// report-and-park one). For tests and, later, demand paging.
pub fn set_page_fault_handler(
    handler: extern "x86-interrupt" fn(InterruptStackFrame, PageFaultErrorCode),
) {
    let mut idt = IDT.lock();
    // SAFETY: `handler` is a valid x86-interrupt handler; installing a
    // handler in a live IDT is the supported use of `set_handler_fn`.
    idt.page_fault.set_handler_fn(handler);
}

extern "x86-interrupt" fn double_fault_handler(
    stack_frame: InterruptStackFrame,
    _error_code: u64,
) -> ! {
    panic!("EXCEPTION: DOUBLE FAULT\n{:#?}", stack_frame);
}

extern "x86-interrupt" fn timer_handler(_stack_frame: InterruptStackFrame) {
    timer::tick();
    pics::end_of_interrupt(TIMER_INTERRUPT_ID);
}

extern "x86-interrupt" fn keyboard_handler(_stack_frame: InterruptStackFrame) {
    // SAFETY: port 0x60 is the PS/2 data port; IRQ1 only fires when a
    // scancode byte is available.
    unsafe {
        let mut data_port = Port::<u8>::new(0x60);
        let scancode = data_port.read();
        keyboard::add_scancode(scancode);
    }
    pics::end_of_interrupt(KEYBOARD_INTERRUPT_ID);
}
