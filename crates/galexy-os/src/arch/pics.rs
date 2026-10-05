//! Legacy 8259 PIC setup and interrupt vector constants.
//!
//! BIOS-booted systems use the legacy PIC; IRQs are remapped to vectors
//! 32..47. (UEFI boot requires the APIC instead — future work, see docs.)

use pic8259::ChainedPics;
use spin::{LazyLock, Mutex};
use x86_64::instructions::port::Port;

use crate::serial_println;

/// Base vector for the primary PIC's IRQs.
pub const PIC_1_OFFSET: u8 = 32;
/// Base vector for the secondary PIC's IRQs (chained to primary IRQ 2).
pub const PIC_2_OFFSET: u8 = PIC_1_OFFSET + 8;

/// Vector the PIT timer (IRQ0) fires on.
pub const TIMER_INTERRUPT_ID: u8 = PIC_1_OFFSET;
/// Vector the PS/2 keyboard (IRQ1) fires on.
pub const KEYBOARD_INTERRUPT_ID: u8 = PIC_1_OFFSET + 1;

static PICS: LazyLock<Mutex<ChainedPics>> = LazyLock::new(|| {
    // SAFETY: PICs are only initialized through this single static instance.
    Mutex::new(unsafe { ChainedPics::new(PIC_1_OFFSET, PIC_2_OFFSET) })
});

/// Remaps the PICs and enables the PS/2 first port.
///
/// The timer is LAPIC-delivered since the APIC-timer commit — its legacy
/// IRQ0 line stays MASKED (a masked line never asserts, so no double
/// delivery and no lost-EOI ghosts). Only IRQ1 (keyboard) is still
/// PIC-delivered (until the I/O APIC wiring).
pub fn init() {
    // SAFETY: done once at boot, before any interrupts are enabled.
    unsafe {
        let mut pics = PICS.lock();
        pics.initialize();
        // The OCW1 masks are whatever the BIOS left (SeaBIOS runs a POLLED
        // keyboard); don't rely on the inherited state. Unmask only the
        // line this OS still drives through the PIC — IRQ1 (keyboard).
        // IRQ0 is LAPIC-delivered now; the cascade (IRQ2) is unused.
        pics.write_masks(0b1111_1101, 0b1111_1111);
    }
    // Make sure the PS/2 controller's first port (keyboard) is enabled.
    // SAFETY: fixed controller command port.
    unsafe {
        Port::new(0x64).write(0xAE_u8);
        // Drain any bytes the BIOS left in the output buffer: a full
        // buffer never re-asserts IRQ1, so the first real keystroke would
        // black-hole.
        let mut status = Port::<u8>::new(0x64);
        let mut data = Port::<u8>::new(0x60);
        let mut guard = 0u32;
        while status.read() & 0x01 != 0 && guard < 64 {
            let _ = data.read();
            guard += 1;
        }
    }
    serial_println!("[pics] ready (masks master 0b1111_1101, slave 0b1111_1111)");
}

/// Signals end-of-interrupt for a handled vector (from handlers only).
pub fn end_of_interrupt(interrupt_id: u8) {
    // SAFETY: correct usage per pic8259 contract.
    unsafe {
        PICS.lock().notify_end_of_interrupt(interrupt_id);
    }
}
