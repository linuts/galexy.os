//! Global Descriptor Table + Task State Segment.
//!
//! The TSS provides interrupt stack table (IST) stacks; the double fault
//! handler runs on its own stack so that stack overflow is diagnosable.
//!
//! Note: `gdt.load()` does not touch segment registers; all of them (including
//! `ss`/`ds`) must be set explicitly here, or later faults (e.g. `iretq`)
//! triple-fault (bootloader migration warning).

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

static TSS: LazyLock<TaskStateSegment> = LazyLock::new(|| {
    let mut tss = TaskStateSegment::new();
    // Stack grows downward: point at the *top* of the reserved range.
    tss.interrupt_stack_table[DOUBLE_FAULT_IST_INDEX as usize] =
        VirtAddr::from_ptr(&DOUBLE_FAULT_STACK.0) + DOUBLE_FAULT_STACK_SIZE as u64;
    tss
});

struct Selectors {
    code: SegmentSelector,
    data: SegmentSelector,
    tss: SegmentSelector,
}

static GDT: LazyLock<(GlobalDescriptorTable, Selectors)> = LazyLock::new(|| {
    let mut gdt = GlobalDescriptorTable::new();
    let code = gdt.append(Descriptor::kernel_code_segment());
    let data = gdt.append(Descriptor::kernel_data_segment());
    let tss = gdt.append(Descriptor::tss_segment(&TSS));
    (gdt, Selectors { code, data, tss })
});

/// Loads the GDT and refreshes all segment registers.
pub fn init() {
    let (gdt, selectors) = &*GDT;
    gdt.load();
    // SAFETY: the selectors describe valid ring-0 segments in the GDT we
    // just loaded.
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
