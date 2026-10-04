//! Global Descriptor Table + Task State Segment.
//!
//! The TSS provides interrupt stack table (IST) stacks; the double fault
//! handler runs on its own stack so that stack overflow is diagnosable.
//!
//! Note: `gdt.load()` does not touch segment registers; all of them (including
//! `ss`/`ds`) must be set explicitly here, or later faults (e.g. `iretq`)
//! triple-fault (bootloader migration warning).

use core::cell::UnsafeCell;

use spin::LazyLock;
use x86_64::instructions::segmentation::{Segment, CS, DS, ES, FS, GS, SS};
use x86_64::instructions::tables::load_tss;
use x86_64::structures::gdt::{Descriptor, GlobalDescriptorTable, SegmentSelector};
use x86_64::structures::tss::TaskStateSegment;
use x86_64::VirtAddr;

/// IST slot used by the double fault handler.
pub const DOUBLE_FAULT_IST_INDEX: u16 = 0;

/// IST stack size for the double fault handler.
const DOUBLE_FAULT_STACK_SIZE: usize = 4096 * 5;

/// Writable memory for the double fault IST stack.
struct DoubleFaultStack([u8; DOUBLE_FAULT_STACK_SIZE]);

// SAFETY: never written through Rust code; only the CPU's IST pointer uses it.
unsafe impl Sync for DoubleFaultStack {}

static DOUBLE_FAULT_STACK: DoubleFaultStack = DoubleFaultStack([0; DOUBLE_FAULT_STACK_SIZE]);

/// TSS holder with intentional interior mutability: [`set_tss_rsp0`]
/// updates the privilege stack table after GDT load, while the CPU reads the
/// same memory through the GDT descriptor on every ring-3 crossing.
struct TssHolder(UnsafeCell<TaskStateSegment>);

impl TssHolder {
    /// Shared access (descriptor build, introspection).
    fn get(&self) -> &TaskStateSegment {
        // SAFETY: single-core; Rust-side writers are IRQ-gated and no Rust
        // reader holds a reference across a write (the CPU is a hardware
        // reader, outside Rust's aliasing rules).
        unsafe { &*self.0.get() }
    }

    /// Exclusive access for the (IRQ-gated) RSP0 writer.
    fn get_mut(&self) -> &mut TaskStateSegment {
        // SAFETY: single-core, fenced by without_interrupts in the caller;
        // no Rust aliasing readers exist across the write.
        unsafe { &mut *self.0.get() }
    }
}

// SAFETY: the CPU (not Rust) reads this concurrently with the single
// IRQ-gated Rust writer on this single-core kernel.
unsafe impl Sync for TssHolder {}

/// The TSS; initialized on first access (needs the double-fault stack's
/// runtime address, so not const).
static TSS: LazyLock<TssHolder> = LazyLock::new(|| {
    let mut tss = TaskStateSegment::new();
    // Stack grows downward: point at the *top* of the reserved range.
    tss.interrupt_stack_table[DOUBLE_FAULT_IST_INDEX as usize] =
        VirtAddr::from_ptr(&DOUBLE_FAULT_STACK.0) + DOUBLE_FAULT_STACK_SIZE as u64;
    TssHolder(UnsafeCell::new(tss))
});

struct Selectors {
    code: SegmentSelector,
    data: SegmentSelector,
    user_code: SegmentSelector,
    user_data: SegmentSelector,
    tss: SegmentSelector,
}

static GDT: LazyLock<(GlobalDescriptorTable, Selectors)> = LazyLock::new(|| {
    let mut gdt = GlobalDescriptorTable::new();
    let code = gdt.append(Descriptor::kernel_code_segment());
    let data = gdt.append(Descriptor::kernel_data_segment());
    // Ring-3 segments (DPL 3): used by iretq/sysret entries into user code.
    // SYSRET quirk (Step A): user SS must land at user CS + 8 — the append
    // order below guarantees that layout.
    let user_code = gdt.append(Descriptor::user_code_segment());
    let user_data = gdt.append(Descriptor::user_data_segment());
    let tss = gdt.append(Descriptor::tss_segment(TSS.get()));
    (
        gdt,
        Selectors {
            code,
            data,
            user_code,
            user_data,
            tss,
        },
    )
});

/// Loads the GDT and refreshes all segment registers.
pub fn init() {
    let (gdt, selectors) = &*GDT;
    gdt.load();
    // SAFETY: the selectors describe valid segments in the GDT we just
    // loaded; ring-3 selectors are not loaded into the CPU's current state.
    unsafe {
        CS::set_reg(selectors.code);
        SS::set_reg(selectors.data);
        DS::set_reg(selectors.data);
        ES::set_reg(selectors.data);
        FS::set_reg(selectors.data);
        GS::set_reg(selectors.data);
        // The task register must be (re)loaded after lgdt — without it the
        // IST dispatch (double fault) reads a stale descriptor.
        load_tss(selectors.tss);
    }
}

/// `(user_cs, user_ss)` selectors with RPL 3 — for fabricating ring-3 entry
/// frames (iretq) and for STAR/LSTAR setup in the syscall work.
pub fn user_cs_ss() -> (u64, u64) {
    let selectors = &GDT.1;
    (
        selectors.user_code.0 as u64 | 3, // RPL 3
        selectors.user_data.0 as u64 | 3, // RPL 3
    )
}

/// Writes `rsp0` into the live TSS.
///
/// The TSS lives behind a Mutex — its `GlobalDescriptorTable` descriptor was
/// built from this same static address, so updating the field updates what
/// the CPU reads on ring 3 → ring 0 crossings. IRQ-gated: RSP0 must never be
/// observed mid-update.
pub fn set_tss_rsp0(rsp0: VirtAddr) {
    x86_64::instructions::interrupts::without_interrupts(|| {
        TSS.get_mut().privilege_stack_table[0] = rsp0;
    });
}

/// Reads the live TSS's RSP0 (the value the CPU would push to on a ring 3
/// interrupt) — test/debug introspection.
pub fn tss_rsp0() -> VirtAddr {
    TSS.get().privilege_stack_table[0]
}
