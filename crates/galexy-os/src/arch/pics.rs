//! Legacy 8259 PIC: kept quiet, not removed.
//!
//! Since the APIC work (M17), ALL interrupt delivery is APIC: the LAPIC
//! timer carries the timer vector, the I/O APIC routes the keyboard. The
//! 8259 pair still physically exists on BIOS boots (SeaBIOS leaves it in
//! whatever state it likes) — an UNMASKED legacy line would assert its IRQ
//! on the PIC, never get an EOI in PIC terms, and steal/double-deliver.
//! So: remap (keeps the vectors well-defined) and fully mask BOTH 8259s.
//! The controller stays here, quiet, for real-hardware boots where the
//! I/O APIC might be absent (falling back to PIC delivery is future work).

use pic8259::ChainedPics;
use spin::{LazyLock, Mutex};

use crate::serial_println;

/// Base vector for the primary PIC's IRQs.
pub const PIC_1_OFFSET: u8 = 32;
/// Base vector for the secondary PIC's IRQs (chained to primary IRQ 2).
pub const PIC_2_OFFSET: u8 = PIC_1_OFFSET + 8;

/// Vector the timer fires on (LAPIC timer; the legacy PIT IRQ0 vector).
pub const TIMER_INTERRUPT_ID: u8 = PIC_1_OFFSET;
/// Vector the PS/2 keyboard fires on (I/O APIC route of legacy IRQ1).
pub const KEYBOARD_INTERRUPT_ID: u8 = PIC_1_OFFSET + 1;

static PICS: LazyLock<Mutex<ChainedPics>> = LazyLock::new(|| {
    // SAFETY: PICs are only initialized through this single static instance.
    Mutex::new(unsafe { ChainedPics::new(PIC_1_OFFSET, PIC_2_OFFSET) })
});

/// Remaps the PICs and masks them completely (legacy-quiet bring-up).
///
/// The PS/2 controller enable + stale-buffer drain that used to live here
/// belongs to the KEYBOARD driver now (see `drivers/keyboard::init`) — it
/// is about the i8042, not about which controller delivers its line.
pub fn init() {
    // SAFETY: done once at boot, before any interrupts are enabled.
    unsafe {
        let mut pics = PICS.lock();
        pics.initialize();
        // The OCW1 masks are whatever the BIOS left (SeaBIOS runs a POLLED
        // keyboard); don't rely on the inherited state. ALL lines masked:
        // the APIC family owns delivery now; masked lines never assert, so
        // there are no lost-EOI ghosts and no double delivery.
        pics.write_masks(0b1111_1111, 0b1111_1111);
    }
    serial_println!("[pics] legacy 8259s remapped + fully masked (APIC delivers)");
}
