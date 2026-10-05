//! Per-CPU Global Descriptor Table + Task State Segment.
//!
//! The TSS provides interrupt stack table (IST) stacks; the double fault
//! handler runs on its own stack so that stack overflow is diagnosable.
//! Since the SMP substrate landed, EACH CPU owns its own GDT + TSS +
//! double-fault stack (the TSS embeds addresses of CPU-local things —
//! RSP0/IST — so a shared TSS would hand ring-3 crossings of one CPU the
//! wrong stack). The GDT slot layout is IDENTICAL on every CPU by
//! construction (same append order), which keeps the selector constants
//! (STAR, iretq frames, `ltr`) valid machine-wide.
//!
//! Note: `lgdt` does not touch segment registers; all of them (including
//! `ss`/`ds`) must be set explicitly here, or later faults (e.g. `iretq`)
//! triple-fault (bootloader migration warning).

use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicBool, Ordering};

use x86_64::instructions::segmentation::{Segment, CS, DS, ES, FS, GS, SS};
use x86_64::instructions::tables::load_tss;
use x86_64::structures::gdt::{Descriptor, GlobalDescriptorTable, SegmentSelector};
use x86_64::structures::tss::TaskStateSegment;
use x86_64::VirtAddr;

use super::cpu::MAX_CPUS;
use crate::serial_println;

/// IST slot used by the double fault handler.
pub const DOUBLE_FAULT_IST_INDEX: u16 = 0;

/// IST stack size for the double fault handler.
const DOUBLE_FAULT_STACK_SIZE: usize = 4096 * 5;

/// Selector values are FIXED by the append order below (identical in every
/// CPU's slot): null=0x00, kernel code=0x08, kernel data=0x10, user
/// code=0x18, user data=0x20, TSS=0x28. The build asserts these constants
/// against what the builder actually produced, so a re-order is caught at
/// init, not by a triple fault.
pub const KERNEL_CS_SELECTOR: u16 = 0x08;
pub const KERNEL_DS_SELECTOR: u16 = 0x10;
pub const USER_CS_SELECTOR: u16 = 0x18;
pub const USER_DS_SELECTOR: u16 = 0x20;
pub const TSS_SELECTOR: u16 = 0x28;

/// One CPU's descriptor tables (GDT + TSS + double-fault IST stack).
///
/// Build + `lgdt`/`ltr` happen at that CPU's own bring-up; from then on the
/// CPU reads the tables via the hardware (outside Rust's aliasing rules) and
/// Rust only WRITES the live TSS.RSP0 (IRQ-gated, from this CPU's own
/// switch-in paths). The `UnsafeCell` interior mutability documents that
/// the CPU is a co-reader of this memory.
struct CpuSlot {
    df_stack: [u8; DOUBLE_FAULT_STACK_SIZE],
    tss: UnsafeCell<TaskStateSegment>,
    gdt: UnsafeCell<GlobalDescriptorTable>,
    /// Set by this CPU's bring-up (it owns the slot until tables are live).
    built: AtomicBool,
}

// SAFETY: each slot is used exclusively by its owning CPU; the CPU itself
// reads this memory through the hardware.
unsafe impl Sync for CpuSlot {}

impl CpuSlot {
    const fn new() -> Self {
        Self {
            df_stack: [0; DOUBLE_FAULT_STACK_SIZE],
            tss: UnsafeCell::new(TaskStateSegment::new()),
            gdt: UnsafeCell::new(GlobalDescriptorTable::empty()),
            built: AtomicBool::new(false),
        }
    }

    /// Builds this slot's GDT (TSS with per-CPU IST stack appended) and
    /// loads it (lgdt + segment reloads + ltr). Called from the owning
    /// CPU's bring-up only; `built` guards against a double build.
    ///
    /// Returns the built selectors (introspection/tests).
    fn build_and_load(&'static self) {
        assert!(
            !self.built.swap(true, Ordering::AcqRel),
            "gdt: CPU slot built twice"
        );
        // SAFETY: exclusive — this CPU's own bring-up; no other reader.
        let (tss, gdt) = unsafe { (&mut *self.tss.get(), &mut *self.gdt.get()) };
        // Stack grows downward: point at the *top* of the reserved range.
        tss.interrupt_stack_table[DOUBLE_FAULT_IST_INDEX as usize] =
            VirtAddr::from_ptr(&self.df_stack) + DOUBLE_FAULT_STACK_SIZE as u64;

        gdt.append(Descriptor::kernel_code_segment());
        gdt.append(Descriptor::kernel_data_segment());
        // Ring-3 segments (DPL 3): used by iretq/sysret entries into user
        // code. SYSRET quirk (Step A): user SS must land at user CS + 8 —
        // the append order below guarantees that layout.
        gdt.append(Descriptor::user_code_segment());
        gdt.append(Descriptor::user_data_segment());
        gdt.append(Descriptor::tss_segment(tss));

        // Lock in the safety net: selector expectations from the consts
        // above (append order is what makes every CPU's tables identical).
        debug_assert_eq!(
            KERNEL_CS_SELECTOR,
            selector_of_entry(0),
            "kernel cs selector"
        );
        debug_assert_eq!(USER_CS_SELECTOR, selector_of_entry(2), "user cs selector");
        debug_assert_eq!(USER_DS_SELECTOR, selector_of_entry(3), "user ds selector");
        debug_assert_eq!(TSS_SELECTOR, selector_of_entry(4), "tss selector");

        // SAFETY: the tables live in the static slot (per-CPU ownership);
        // 'static is enforced by borrowing through the static array.
        unsafe {
            gdt.load_unsafe();
            // Segment reloads: ring-3 selectors are NOT loaded into the
            // CPU's current state (they only appear in iret frames).
            CS::set_reg(SegmentSelector(KERNEL_CS_SELECTOR));
            SS::set_reg(SegmentSelector(KERNEL_DS_SELECTOR));
            DS::set_reg(SegmentSelector(KERNEL_DS_SELECTOR));
            ES::set_reg(SegmentSelector(KERNEL_DS_SELECTOR));
            FS::set_reg(SegmentSelector(KERNEL_DS_SELECTOR));
            GS::set_reg(SegmentSelector(KERNEL_DS_SELECTOR));
            // The task register must be (re)loaded after lgdt — without it
            // the IST dispatch (double fault) reads a stale descriptor.
            load_tss(SegmentSelector(TSS_SELECTOR));
        }
        serial_println!("[gdt] cpu slot tables live");
    }
}

/// The index → selector mapping for entry n (selector = (n + 1) << 3 —
/// entry 0 is the null descriptor, selectors start at 0x08).
const fn selector_of_entry(entry: u16) -> u16 {
    (entry + 1) << 3
}

static SLOTS: [CpuSlot; MAX_CPUS] = [const { CpuSlot::new() }; MAX_CPUS];

/// Builds + loads the calling CPU's tables (the owning CPU calls this at
/// bring-up; the BSP's call is `init()`).
pub fn bring_up(cpu_index: usize) {
    assert!(cpu_index < MAX_CPUS, "gdt: cpu_index out of range");
    SLOTS[cpu_index].build_and_load();
}

/// Loads the GDT and refreshes all segment registers (BSP bring-up).
pub fn init() {
    bring_up(0);
}

/// `(user_cs, user_ss)` selectors with RPL 3 — for fabricating ring-3 entry
/// frames (iretq) and for STAR/LSTAR setup in the syscall work.
///
/// Identical for every CPU's slot (fixed append order).
pub fn user_cs_ss() -> (u64, u64) {
    (
        u64::from(USER_CS_SELECTOR) | 3, // RPL 3
        u64::from(USER_DS_SELECTOR) | 3, // RPL 3
    )
}

/// `(kernel_cs, user_cs)` RAW selectors (no RPL bits) for `STAR`: the
/// syscall/sysret mechanism consumes bases, not RPL-decorated selectors.
pub fn syscall_selectors() -> (u16, u16) {
    (KERNEL_CS_SELECTOR, USER_CS_SELECTOR)
}

/// Writes `rsp0` into the CALLING CPU's live TSS.
///
/// TSS.RSP0 is where a ring-3 crossing pushes its frame; the CPU reads the
/// same field the Rust writer just set, and only THIS CPU ever does both —
/// which makes the IRQ-gated store race-free by per-CPU ownership. (The
/// timer switch, syscall handoff... every writer runs IRQ-gated already.)
pub fn set_tss_rsp0(rsp0: VirtAddr) {
    x86_64::instructions::interrupts::without_interrupts(|| {
        // SAFETY: set_tss_rsp0 runs on the CPU that owns the slot; the
        // hardware co-reader does not overlap Rust's gated write window.
        unsafe {
            let slot = super::cpu::current_index();
            let tss = &mut *SLOTS[slot].tss.get();
            tss.privilege_stack_table[0] = rsp0;
        }
    });
}

/// Reads the live TSS's RSP0 of the CALLING CPU — test/debug introspection.
pub fn tss_rsp0() -> VirtAddr {
    let slot = super::cpu::current_index();
    // SAFETY: readers after the owning CPU's build; RSP0 is single-writer
    // per slot (the owning CPU, under the gate).
    unsafe { (*SLOTS[slot].tss.get()).privilege_stack_table[0] }
}
