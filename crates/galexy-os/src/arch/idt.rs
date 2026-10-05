//! Interrupt Descriptor Table: fault and device interrupt handlers.

use spin::{LazyLock, Mutex};
use x86_64::instructions::port::Port;
use x86_64::structures::idt::{InterruptDescriptorTable, InterruptStackFrame, PageFaultErrorCode};
use x86_64::VirtAddr;

use super::apic;
use super::gdt;
use super::pics::{KEYBOARD_INTERRUPT_ID, TIMER_INTERRUPT_ID};
use crate::drivers::keyboard;

/// The IDT sits behind a mutex so tests (and later, demand paging) can
/// replace handlers at runtime; loading uses `load_unsafe` because the
/// static's identity guarantees the lifetime.
static IDT: LazyLock<Mutex<InterruptDescriptorTable>> = LazyLock::new(|| {
    let mut idt = InterruptDescriptorTable::new();
    idt.breakpoint.set_handler_fn(breakpoint_handler);
    // The page-fault vector uses a NAKED handler (installed by raw address
    // in `init` below): ring-3 faults must tombstone the faulting task and
    // switch away — the x86-interrupt ABI can't hand the CPU elsewhere.
    // Tests can still swap it via `set_page_fault_handler`.
    // SAFETY: index 0 is a valid IST slot we reserved in the TSS.
    unsafe {
        idt.double_fault
            .set_handler_fn(double_fault_handler)
            .set_stack_index(gdt::DOUBLE_FAULT_IST_INDEX);
    }
    // Timer (IRQ0) is a NAKED handler: it switches task contexts directly
    // (see sched/context.rs), so it bypasses the x86-interrupt ABI and is
    // installed by raw address.
    // Keyboard (IRQ1) stays a regular x86-interrupt handler.
    idt[KEYBOARD_INTERRUPT_ID].set_handler_fn(keyboard_handler);
    // The LAPIC's spurious vector MUST have an IDT entry once the LAPIC is
    // enabled by `arch::apic::init` (vector 0xFF): an unhandled stray
    // spurious would hit an empty gate and triple-fault. EOI a real spurious;
    // a spurious needs NO EOI when the vector has no handler — but the LAPIC
    // marks the bit itself, so just count + re-mask via EOI (harmless).
    idt[apic::SPURIOUS_VECTOR].set_handler_fn(spurious_handler);
    Mutex::new(idt)
});

/// Loads the IDT, installing the naked timer + page-fault handlers by
/// address.
pub fn init() {
    let mut idt = IDT.lock();
    // SAFETY: installing valid handler addresses in the live IDT.
    unsafe {
        let timer_fn: unsafe extern "C" fn() = crate::sched::context::timer_handler_naked;
        idt[TIMER_INTERRUPT_ID].set_handler_addr(VirtAddr::from_ptr(timer_fn as *const ()));
        let pf_fn: unsafe extern "C" fn() = crate::sched::context::page_fault_handler_naked;
        idt.page_fault.set_handler_addr(VirtAddr::from_ptr(pf_fn as *const ()));
    }
    drop(idt);
    // SAFETY: the IDT is never moved (it lives in the static) — the lifetime
    // constraint `load` normally enforces holds.
    unsafe { IDT.lock().load_unsafe() };
}

extern "x86-interrupt" fn breakpoint_handler(stack_frame: InterruptStackFrame) {
    crate::serial_println!("EXCEPTION: BREAKPOINT\n{:#?}", stack_frame);
}

/// Installs `handler` as the page-fault handler (replacing the naked
/// kill-or-park default). For tests and, later, demand paging.
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

extern "x86-interrupt" fn keyboard_handler(_stack_frame: InterruptStackFrame) {
    // SAFETY: port 0x60 is the PS/2 data port; IRQ1 only fires when a
    // scancode byte is available.
    unsafe {
        let mut data_port = Port::<u8>::new(0x60);
        let scancode = data_port.read();
        keyboard::add_scancode(scancode);
    }
    // Since the I/O APIC wiring, the keyboard is LAPIC-delivered (edge
    // RTE): the LAPIC EOI is the one true EOI.
    apic::eoi();
}

/// The LAPIC spurious interrupt (vector 0xFF): no device work, just log-free
/// silence. A spurious needs no EOI (the ISR bit for it is never set), so
/// this body is a no-op — the handler exists purely so the gate is mapped.
extern "x86-interrupt" fn spurious_handler(_stack_frame: InterruptStackFrame) {}
