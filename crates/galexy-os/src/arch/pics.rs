//! Legacy 8259 PIC: masked, not a delivery path.
//!
//! When the FADT `IAPC_BOOT_ARCH` bit 0 says the pair is present, remap
//! then mask both controllers so a BIOS-left line cannot assert. When the
//! flag says there is no 8259, skip the remap and only write the mask
//! ports. Delivery is the APIC either way.

use crate::sync::Mutex;
use pic8259::ChainedPics;
use spin::LazyLock;
use x86_64::instructions::port::Port;

use crate::arch::acpi;
use crate::serial_println;

/// Base vector for the primary PIC's IRQs.
pub const PIC_1_OFFSET: u8 = 32;
/// Base vector for the secondary PIC's IRQs (chained to primary IRQ 2).
pub const PIC_2_OFFSET: u8 = PIC_1_OFFSET + 8;

/// Vector the timer fires on (LAPIC timer; the legacy PIT IRQ0 vector).
pub const TIMER_INTERRUPT_ID: u8 = PIC_1_OFFSET;
/// Vector the PS/2 keyboard fires on (I/O APIC route of legacy IRQ1).
pub const KEYBOARD_INTERRUPT_ID: u8 = PIC_1_OFFSET + 1;
/// Vector COM1 fires on (I/O APIC route of legacy IRQ4).
pub const SERIAL_INTERRUPT_ID: u8 = PIC_1_OFFSET + 4;

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
    if acpi::has_8259() {
        // SAFETY: done once at boot, before any interrupts are enabled.
        unsafe {
            let mut pics = PICS.lock();
            pics.initialize();
            // ALL lines masked: the APIC family owns delivery.
            pics.write_masks(0b1111_1111, 0b1111_1111);
        }
        serial_println!("[pics] legacy 8259s remapped + fully masked (APIC delivers)");
    } else {
        // No ICW sequence. Mask ports only, in case the decode still exists.
        // SAFETY: fixed 8259 data ports; writes are ignored when no PIC is there.
        unsafe {
            Port::<u8>::new(0x21).write(0xFF);
            Port::<u8>::new(0xA1).write(0xFF);
        }
        serial_println!("[pics] 8259 absent (FADT boot-arch); mask only, remap skipped");
    }
}
