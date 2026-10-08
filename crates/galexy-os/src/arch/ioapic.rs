//! I/O APIC: external interrupt routing.
//!
//! Maps the boot I/O APIC (MMIO base + GSI base from the MADT, see
//! `arch::acpi`) and wires two external lines: the PS/2 keyboard's ISA
//! IRQ1 (its GSI from the MADT's Interrupt Source Override when present,
//! `1` otherwise) onto vector 33, and COM1's ISA IRQ4 onto vector 36.
//!
//! All other redirection entries stay MASKED — an unmasked dead RTE can
//! never assert, but a stray on a wired-but-unhandled vector would be an
//! empty IDT gate (triple fault), so the rule is: mask what we don't use.
//!
//! EOI: edge-triggered RTEs need no I/O APIC EOI — the LAPIC's EOI
//! (`arch::apic::eoi`) clears the delivery. Level-triggered lines would
//! need the EOI Register (0xF0); the keyboard is edge-triggered.
//!
//! Register interface (MEMORY-mapped, not I/O ports): IOREGSEL at page+0x00
//! selects the 32-bit register index; IOWIN at page+0x10 reads/writes it.
//! Redirection entries live at indices 0x10 + 2*pin (even = low word, odd
//! = high word).

use core::sync::atomic::{AtomicU64, Ordering};

use x86_64::structures::paging::{Page, PageTableFlags, PhysFrame, Size4KiB};
use x86_64::{PhysAddr, VirtAddr};

use crate::arch::acpi;
use crate::arch::apic;
use crate::arch::mm;
use crate::arch::pics::{KEYBOARD_INTERRUPT_ID, SERIAL_INTERRUPT_ID};
use crate::serial_println;

/// IOREGSEL: 32-bit register selector (memory offset within the page).
const REGSEL: u64 = 0x00;
/// IOWIN: window onto the selected register.
const WINDOW: u64 = 0x10;
/// First redirection-table register index.
const REDTBL_BASE: u32 = 0x10;
/// RTE fields (fixed delivery, physical dest, edge, active-high = all 0):
/// only the vector, the mask bit and the destination are driven.
const MASK_BIT: u64 = 1 << 16;
const DEST_SHIFT: u32 = 56;

/// The mapped I/O APIC register page; 0 before `init`.
static IOAPIC_PAGE: AtomicU64 = AtomicU64::new(0);

/// Reads one 32-bit register by its index.
fn read_reg(page: VirtAddr, index: u32) -> u32 {
    // SAFETY: fixed MMIO interface (IOREGSEL/IOWIN pair), page mapped at
    // init; indices are register-aligned by construction.
    unsafe {
        let sel = (page.as_u64() + REGSEL) as *mut u32;
        let win = (page.as_u64() + WINDOW) as *const u32;
        sel.write_volatile(index);
        win.read_volatile()
    }
}

/// Writes one 32-bit register by its index.
fn write_reg(page: VirtAddr, index: u32, value: u32) {
    // SAFETY: fixed MMIO interface; same contract as read_reg.
    unsafe {
        let sel = (page.as_u64() + REGSEL) as *mut u32;
        let win = (page.as_u64() + WINDOW) as *mut u32;
        sel.write_volatile(index);
        win.write_volatile(value);
    }
}

/// Reads redirection-table entry `pin` (packed: low word | high word << 32).
fn read_redtbl(page: VirtAddr, pin: u8) -> u64 {
    let base = REDTBL_BASE + 2 * u32::from(pin);
    u64::from(read_reg(page, base)) | u64::from(read_reg(page, base + 1)) << 32
}

/// Writes redirection-table entry `pin`.
fn write_redtbl(page: VirtAddr, pin: u8, entry: u64) {
    let base = REDTBL_BASE + 2 * u32::from(pin);
    write_reg(page, base, entry as u32);
    write_reg(page, base + 1, (entry >> 32) as u32);
}

/// Maps the I/O APIC register page (MMIO, PRESENT|RW|NX|uncached).
fn map_ioapic_page(base: u64) -> VirtAddr {
    let phys = PhysAddr::new(base);
    assert_eq!(
        phys.as_u64() & 0xFFF,
        0,
        "ioapic: base must be page-aligned"
    );
    // P4 entry 201: sibling of the LAPIC's fixed mapping (200); kernel-half
    // mappings are shared verbatim across every task tree (FreshL4 contract).
    const IOAPIC_P4: u16 = 201;
    let virt = VirtAddr::new(u64::from(IOAPIC_P4) << 39);
    let page = Page::<Size4KiB>::containing_address(virt);
    let frame = PhysFrame::from_start_address(phys).expect("ioapic: base is not frame-aligned");
    let flags = PageTableFlags::PRESENT
        | PageTableFlags::WRITABLE
        | PageTableFlags::NO_EXECUTE
        | PageTableFlags::NO_CACHE;
    mm::map_page_flags(page, frame, flags)
        .unwrap_or_else(|e| panic!("ioapic: failed to map the register page: {e:?}"));
    virt
}

/// Brings up the I/O APIC: maps the register page, masks ALL redirection
/// entries, then wires the keyboard and COM1. Called by `arch::init` after
/// the LAPIC is enabled (its ID is the routing destination).
pub fn init() {
    let madt = acpi::madt();
    let page = map_ioapic_page(madt.ioapic_base());
    IOAPIC_PAGE.store(page.as_u64(), Ordering::Release);

    // Identity sanity: I/O APICVER's low 8 bits carry the version (0x11 or
    // 0x20 on QEMU).
    let version = read_reg(page, 0x01) & 0xFF;
    assert!(
        version >= 0x11,
        "ioapic: bad version {version:#x} — not an I/O APIC at the MADT base?"
    );

    // I/O APICVER bits 23..16 = max redirection entry (23 on QEMU).
    let pins = (read_reg(page, 0x01) >> 16) + 1;
    // Mask EVERYTHING first — the inherited state is firmware's, not ours.
    for pin in 0..pins {
        write_redtbl(page, pin as u8, MASK_BIT);
    }
    serial_println!(
        "[ioapic] register page {:#x} (version {:#x}, {} pins, all masked)",
        page.as_u64(),
        version,
        pins
    );

    // ISA IRQ -> GSI (override or identity) -> pin, edge-triggered,
    // active-high, physical dest = this CPU's LAPIC ID.
    wire_isa(page, pins, 1, KEYBOARD_INTERRUPT_ID, "keyboard");
    wire_isa(page, pins, 4, SERIAL_INTERRUPT_ID, "serial");
}

/// Unmasks one ISA IRQ onto `vector`. Edge, active-high, BSP destination.
fn wire_isa(page: VirtAddr, pins: u32, isa_irq: u8, vector: u8, what: &str) {
    let madt = acpi::madt();
    let gsi = madt.isa_gsi(isa_irq);
    assert!(
        gsi >= madt.ioapic_gsi_base(),
        "ioapic: {what} GSI {gsi} is below the boot I/O APIC's GSI base {}",
        madt.ioapic_gsi_base()
    );
    let pin = gsi - madt.ioapic_gsi_base();
    assert!(
        pin < pins,
        "ioapic: {what} GSI {gsi} is beyond the boot I/O APIC's pin count {pins}"
    );
    let dest = u64::from(apic::lapic_id()) << DEST_SHIFT;
    let entry = u64::from(vector) | dest;
    write_redtbl(page, pin as u8, entry);
    serial_println!(
        "[ioapic] {}: isa irq {} -> gsi {} -> pin {}, vector {}",
        what,
        isa_irq,
        gsi,
        pin,
        vector
    );
}

/// Raw read of redirection entry `pin` (test/inspection seam).
pub fn redtbl(pin: u8) -> u64 {
    let page = IOAPIC_PAGE.load(Ordering::Acquire);
    assert!(page != 0, "ioapic: not initialized");
    read_redtbl(VirtAddr::new(page), pin)
}
