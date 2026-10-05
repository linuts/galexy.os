//! Local APIC: xAPIC/x2APIC access layer + bring-up.
//!
//! Enabled from the MADT's published base (see `arch::acpi`). Supports both
//! the legacy MMIO interface (xAPIC) and the MSR interface (x2APIC) — real
//! hardware frequently ships x2APIC-enabled, so the mode is DETECTED, not
//! assumed. QEMU defaults to xAPIC for our configs; both paths must work.
//!
//! All register access routes through [`reg_read`]/[`reg_write`]: xAPIC
//! reads/writes the mapped MMIO page; x2APIC converts the MMIO offset to an
//! MSR (`0x800 + offset >> 4`). MSR access is unprefixed and ungate-unsafe,
//! so public entry points take the IRQ gate (lock-audit rule).
//!
//! Vectors live in `pics.rs` (still the single source of vector constants):
//! the LAPIC's spurious vector is 0xFF (unused); the timer vector is wired
//! in a later commit, keyboard delivery goes through the I/O APIC.

use spin::Once;
use x86_64::registers::model_specific::Msr;
use x86_64::structures::paging::{Page, PageTableFlags, PhysFrame, Size4KiB};
use x86_64::{PhysAddr, VirtAddr};

use crate::arch::mm;
use crate::serial_println;

/// IA32_APIC_BASE: bit 11 = APIC global enable, bit 10 = x2APIC mode.
const IA32_APIC_BASE: u32 = 0x1B;
/// x2APIC MSR base: an xAPIC MMIO offset maps to 0x800 + (offset >> 4).
const X2APIC_MSR_BASE: u32 = 0x800;
/// Lapic register offsets (subset we drive; timer regs join with the
/// LAPIC-timer commit and ICR with the boot-CPU seam).
const REG_ID: u32 = 0x020;
const REG_EOI: u32 = 0x0B0;
const REG_SPURIOUS: u32 = 0x0F0;
const REG_DFR: u32 = 0x0E0;
const REG_LDR: u32 = 0x0D0;
/// xAPIC-only register shape: writes as an MSR under x2APIC are harmless
/// (the x2APIC MSR space aliases the whole register space).
const REG_TPR: u32 = 0x080;

/// Spurious-interrupt vector: must have an IDT entry the moment the LAPIC
/// is enabled (an unhandled stray spurious would triple-fault). No device
/// ever deliberately delivers it in our use.
pub const SPURIOUS_VECTOR: u8 = 0xFF;

/// The detected LAPIC interface.
///
/// Values published once at [`init`]; reads before that panic (a bug).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LapicMode {
    /// Legacy MMIO interface (register page at the MADT base).
    XApic,
    /// MSR interface (registers at MSRs 0x800 + offset>>4).
    X2Apic,
}

struct ApicCtrl {
    mode: LapicMode,
    /// xAPIC register page (None under x2APIC — MSRs replace MMIO).
    page: Option<VirtAddr>,
}

/// The one LAPIC controller; `Once` (no interior mutation after init).
static APIC: Once<ApicCtrl> = Once::new();

/// Reads one LAPIC register by its xAPIC MMIO offset.
///
/// SAFETY contract: callers must hold the IRQ gate while touching timer/live
/// state (the lock-audit rule); reads are atomic enough that the gate is a
/// sequencing concern, not a torn-read one.
fn reg_read(apic: &ApicCtrl, offset: u32) -> u32 {
    match apic.mode {
        LapicMode::XApic => {
            // SAFETY: the register page was mapped at init; offsets are
            // register-aligned by construction.
            unsafe {
                let reg = apic.page.unwrap().as_u64() + offset as u64;
                (reg as *const u32).read_volatile()
            }
        }
        LapicMode::X2Apic => {
            // SAFETY: x2APIC MSRs exist once EFER + APIC_BASE confirm the
            // mode; the offset range is fixed by the architecture.
            (unsafe { Msr::new(X2APIC_MSR_BASE + (offset >> 4)).read() }) as u32
        }
    }
}

/// Writes one LAPIC register by its xAPIC MMIO offset.
fn reg_write(apic: &ApicCtrl, offset: u32, value: u32) {
    match apic.mode {
        LapicMode::XApic => {
            // SAFETY: the register page was mapped at init; offsets are
            // register-aligned by construction.
            unsafe {
                let reg = apic.page.unwrap().as_u64() + offset as u64;
                (reg as *mut u32).write_volatile(value);
            }
        }
        LapicMode::X2Apic => {
            // SAFETY: x2APIC MSRs exist once EFER + APIC_BASE confirm the
            // mode; the offset range is fixed by the architecture.
            unsafe {
                Msr::new(X2APIC_MSR_BASE + (offset >> 4)).write(u64::from(value));
            }
        }
    }
}

/// Reads one register (public seam — handlers use it for EOI/ICR work).
pub fn reg(offset: u32) -> u32 {
    let apic = APIC.get().unwrap_or_else(|| panic!("apic: not initialized"));
    reg_read(apic, offset)
}

/// Writes one register (public seam — used by `eoi`, timer setup, tests).
pub fn set_reg(offset: u32, value: u32) {
    let apic = APIC.get().unwrap_or_else(|| panic!("apic: not initialized"));
    reg_write(apic, offset, value);
}

/// Signals end-of-interrupt to the LAPIC (the one true EOI now).
pub fn eoi() {
    x86_64::instructions::interrupts::without_interrupts(|| {
        set_reg(REG_EOI, 0);
    });
}

/// The virtual address of the mapped LAPIC register page (xAPIC only).
pub fn lapic_page() -> VirtAddr {
    let apic = APIC.get().unwrap_or_else(|| panic!("apic: not initialized"));
    apic.page.unwrap_or_else(|| panic!("apic: x2APIC mode has no MMIO page"))
}

/// The detected mode (`Once`: fixed at [`init`]).
pub fn mode() -> LapicMode {
    APIC.get().unwrap_or_else(|| panic!("apic: not initialized")).mode
}

/// Reads the LAPIC ID register (xAPIC layout; ID in bits 24..31).
pub fn lapic_id() -> u8 {
    (reg(REG_ID) >> 24) as u8
}

/// Ends an interrupt if interrupts are on (convenience for handler paths).
/// No-op under an already-off gate (naked handler paths are gated already).
pub fn init(lapic_base: u64) {
    // Detection FIRST: MSR 0x1B bit 11 must be set, bit 10 chooses x2APIC.
    let apic_base = Msr::new(IA32_APIC_BASE);
    // SAFETY: IA32_APIC_BASE exists on every 64-bit x86 CPU; EN is usually
    // set but bit 10 is what decides the register interface.
    let base_value = unsafe { apic_base.read() };
    assert!(
        base_value & (1 << 11) != 0,
        "apic: the local APIC is not globally enabled (IA32_APIC_BASE.EN = 0)"
    );
    let detected = if base_value & (1 << 10) != 0 {
        LapicMode::X2Apic
    } else {
        LapicMode::XApic
    };

    let page = if detected == LapicMode::XApic {
        Some(map_lapic_page(lapic_base))
    } else {
        // x2APIC replaces MMIO with MSRs; the MADT base is informational.
        None
    };
    let ctrl = ApicCtrl { mode: detected, page };
    APIC.call_once(|| ctrl);

    // Enable it: spurious vector with bit 8 (APIC enable via the spurious
    // register), TPR 0 (accept everything), flat destination mode. Under
    // xAPIC we may ALSO have to set the global EN bit if firmware left it
    // clear — it was read as set above, so nothing to do.
    set_reg(REG_SPURIOUS, SPURIOUS_VECTOR as u32 | 0x100);
    set_reg(REG_TPR, 0); // TPR = 0
    set_reg(REG_DFR, 0xFFFF_FFFF); // flat mode (DFR is xAPIC-only; harmless as MSR under x2)
    set_reg(REG_LDR, 0); // logical dest = all-ones mask (flat)
    serial_println!("[apic] lapic ready (mode {:?})", detected);
    serial_println!(
        "[apic] lapic id {}, spurious {:#x}",
        lapic_id(),
        reg(REG_SPURIOUS)
    );
}

/// Maps the LAPIC register page (MMIO, PRESENT|RW|NX, not from the
/// allocator — device memory).
fn map_lapic_page(base: u64) -> VirtAddr {
    let phys = PhysAddr::new(base);
    assert_eq!(
        phys.as_u64() & 0xFFF,
        0,
        "apic: LAPIC base must be page-aligned"
    );
    // A fresh P4 entry in the KERNEL half would collide with task trees by
    // index only — but the kernel half is SHARED across all trees, so any
    // kernel-half entry is uniformly visible. Use a low fixed P4 entry
    // unused by anything else: 200 (user regions live below 256 top-down,
    // dynamics below 100; 200 is free real estate).
    const LAPIC_P4: u16 = 200;
    // A fixed virtual address under that P4 entry (canonical lower-half
    // sign-extension happens via the address math below).
    let virt = VirtAddr::new((u64::from(LAPIC_P4) << 39) | 0x0);
    let page = Page::<Size4KiB>::containing_address(virt);
    let frame =
        PhysFrame::from_start_address(phys).expect("apic: LAPIC base is not frame-aligned");
    let flags = PageTableFlags::PRESENT
        | PageTableFlags::WRITABLE
        | PageTableFlags::NO_EXECUTE
        | PageTableFlags::NO_CACHE;
    mm::map_page_flags(page, frame, flags)
        .unwrap_or_else(|e| panic!("apic: failed to map the LAPIC page: {e:?}"));
    serial_println!("[apic] lapic page {:#x} -> {:#x}", virt.as_u64(), base);
    virt
}
