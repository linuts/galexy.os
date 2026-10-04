//! Virtual memory paging: page-table access and page mapping.
//!
//! The mapper operates on the bootloader-created active page tables, accessed
//! through the recursive mapping configured in `BOOTLOADER_CONFIG` (canonical,
//! P4 index 511). Initialization reads CR3 directly. All page operations hold
//! the mapper lock under `without_interrupts` (lock-audit rule: the timer is
//! the only preemptor) — page ops are thus callable from any kernel context,
//! including IRQ handlers.
//!
//! `FreshL4` / `with_table` are the groundwork for ring-3 isolation
//! (roadmap Step B): fresh L4 trees cloned from the active table (kernel
//! higher-half shared, user region empty) and mapping through a NON-active
//! tree rooted at an arbitrary frame, via the physical-memory mapping.

use core::sync::atomic::{AtomicBool, Ordering};

use spin::Mutex;
use x86_64::registers::control::Cr3;
use x86_64::structures::paging::{
    FrameAllocator, Mapper, OffsetPageTable, Page, PageTable, PageTableFlags, PhysFrame, Size4KiB,
    Translate,
};
use x86_64::{PhysAddr, VirtAddr};

use crate::serial_println;

/// The active page-table mapper; `Some` after [`init`].
static MAPPER: Mutex<Option<OffsetPageTable<'static>>> = Mutex::new(None);
/// Set by [`init`]; all ops panic before that.
static READY: AtomicBool = AtomicBool::new(false);
/// Physical-memory offset (from `BOOTLOADER_CONFIG`); `None` until init.
static PHYS_OFFSET: Mutex<Option<VirtAddr>> = Mutex::new(None);

/// Physical-memory offset accessor for internal use (panics if unset).
///
/// Note: `spin::Mutex::lock()` returns the guard directly (no poisoning), so
/// this `expect` is `Option<VirtAddr>::expect` through the guard's deref —
/// it unwraps the stored offset, not a lock result.
fn phys_offset() -> VirtAddr {
    PHYS_OFFSET.lock().expect("paging: not initialized (no physical memory offset)")
}

/// Initializes the mapper over the currently active page tables. Requires
/// the physical memory mapping from `BOOTLOADER_CONFIG`; idempotent.
pub fn init(phys_offset: u64) {
    {
        let mut stored = PHYS_OFFSET.lock();
        if stored.is_none() {
            *stored = Some(VirtAddr::new(phys_offset));
        }
    }
    let mut mapper = MAPPER.lock();
    if mapper.is_some() {
        return;
    }
    let level_4_table = unsafe { active_level_4_table(VirtAddr::new(phys_offset)) };
    // SAFETY: the L4 table is the CPU's active one (read via CR3) and is
    // only ever accessed through the MAPPER lock below.
    *mapper = Some(unsafe { OffsetPageTable::new(level_4_table, VirtAddr::new(phys_offset)) });
    drop(mapper);
    READY.store(true, Ordering::Relaxed);
    serial_println!("[mm] page mapper ready");
}

/// Returns a mutable reference to the active level-4 page table.
///
/// # Safety
///
/// The result aliases live page tables; only `MAPPER`-lock holders may use
/// it (init guarantees exclusivity).
unsafe fn active_level_4_table(phys_offset: VirtAddr) -> &'static mut PageTable {
    let (level_4_table_frame, _) = Cr3::read();
    let virt = phys_offset + level_4_table_frame.start_address().as_u64();
    // SAFETY: page tables live for the whole run; access is exclusive via
    // the MAPPER lock.
    unsafe { &mut *(virt.as_mut_ptr()) }
}

/// Runs `f` with the mapper under the IRQ gate (lock-audit rule: the mapper
/// lock is reached by the timer switch path, so it must never be held by
/// preemptable code with interrupts on). Page ops are safe from any context.
///
/// Panics if `init` hasn't run (a bug, not a condition to handle).
fn with_mapper<F>(f: F)
where
    F: FnOnce(&mut OffsetPageTable<'static>),
{
    assert!(READY.load(Ordering::Relaxed), "paging: mapper not initialized");
    x86_64::instructions::interrupts::without_interrupts(|| {
        let mut mapper = MAPPER.lock();
        let mapper = mapper.as_mut().expect("paging: mapper lock init race");
        f(mapper);
    });
}

/// Why a page operation failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PageError {
    /// The page was already mapped.
    AlreadyMapped,
    /// The page was not mapped.
    NotMapped,
    /// No frame left for a page-table page.
    NoFrame,
    /// Any other mapping error.
    Internal,
}

/// Maps `frame` to `page` with PRESENT | WRITABLE | NO_EXECUTE.
pub fn map_page(page: Page<Size4KiB>, frame: PhysFrame<Size4KiB>) -> Result<(), PageError> {
    let flags = PageTableFlags::PRESENT | PageTableFlags::WRITABLE | PageTableFlags::NO_EXECUTE;
    map_page_flags(page, frame, flags)
}

/// Maps `frame` to `page` with explicit flags (the USER_ACCESSIBLE / NX
/// permutation space user tasks need).
pub fn map_page_flags(
    page: Page<Size4KiB>,
    frame: PhysFrame<Size4KiB>,
    flags: PageTableFlags,
) -> Result<(), PageError> {
    use x86_64::structures::paging::mapper::MapToError;
    let mut result = Ok(());
    with_mapper(|mapper| {
        let mut frame_alloc = PageTableFrameAllocator;
        // SAFETY: exclusive access via MAPPER under the IRQ gate; frame is
        // Usable per the allocator contract.
        unsafe {
            match mapper.map_to(page, frame, flags, &mut frame_alloc) {
                Ok(flush) => flush.flush(),
                Err(MapToError::FrameAllocationFailed) => result = Err(PageError::NoFrame),
                Err(MapToError::PageAlreadyMapped(_)) => result = Err(PageError::AlreadyMapped),
                Err(_) => result = Err(PageError::Internal),
            }
        }
    });
    result
}

/// Unmaps `page` and returns the frame it pointed at (TLB flushed).
pub fn unmap_page(page: Page<Size4KiB>) -> Result<PhysFrame<Size4KiB>, PageError> {
    let mut result = Err(PageError::NotMapped);
    with_mapper(|mapper| {
        if let Ok((frame, flush)) = mapper.unmap(page) {
            flush.flush();
            result = Ok(frame);
        }
    });
    result
}

/// Translates a virtual address to its physical address, if mapped.
pub fn translate(virt: VirtAddr) -> Option<PhysAddr> {
    let mut result = None;
    with_mapper(|mapper| {
        result = mapper.translate_addr(virt);
    });
    result
}

/// Physical→virtual conversion for direct access to allocated frames.
///
/// Safe only for frames the allocator handed out (they are `Usable` and
/// otherwise unmapped), per the `BootInfo::physical_memory_offset` contract.
pub fn phys_to_virt(phys: PhysAddr, phys_offset: u64) -> VirtAddr {
    VirtAddr::new(phys.as_u64() + phys_offset)
}

/// Virtual address of an allocator-owned frame through the physical-memory
/// mapping (no offset parameter needed). Exclusive access per the frame
/// allocator's contract.
pub fn frame_virt(phys: PhysAddr) -> VirtAddr {
    phys_offset() + phys.as_u64()
}

/// The highest FREE P4 entry index in the user half (`< 256`), scanning
/// top-down. Bootload dynamics fill P4 upward from 0 (kernel, framebuffers
/// mapped low), fixed mappings are phys memory at 32 / recursive at 511 /
/// heap at 43 — so fresh 512-GiB user regions come from the top of the
/// user half downward. Each pick is immediately PRESENT in the L4 (the
/// caller maps into it right away), so consecutive picks return distinct
/// indices.
pub fn top_user_p4_index() -> Option<u16> {
    assert!(READY.load(Ordering::Relaxed), "paging: mapper not initialized");
    let phys = phys_offset();
    let (l4_frame, _) = Cr3::read();
    let l4 = phys + l4_frame.start_address().as_u64();
    // SAFETY: the L4 table is the CPU's active one; read-only scan through
    // the phys map (same access pattern the FreshL4 clone uses).
    unsafe {
        let table = &*(l4.as_ptr::<PageTable>());
        for i in (0..256u16).rev() {
            if table[usize::from(i)].is_unused() {
                return Some(i);
            }
        }
    }
    None
}

/// Adapter: feeds the x86_64 crate's mapping machinery from our frame
/// allocator — needed for page-table frames.
struct PageTableFrameAllocator;

// SAFETY: allocate_frame hands out Usable, otherwise-unmapped frames; the
// mapping machinery never deallocates through this adapter.
unsafe impl FrameAllocator<Size4KiB> for PageTableFrameAllocator {
    fn allocate_frame(&mut self) -> Option<PhysFrame<Size4KiB>> {
        super::allocate_frame()
    }
}

/* ---------------- fresh (non-active) page tables — Step B groundwork ---- */

/// Physical frame of a freshly allocated, near-verbatim copy of the ACTIVE
/// L4: kernel higher-half entries are shared (same frames), the user region
/// is empty (nothing below the kernel half is mapped in the active tables),
/// and the recursive entry self-points at the fresh frame.
pub struct FreshL4 {
    /// Physical frame of the new top-level table.
    pub frame: PhysFrame<Size4KiB>,
}

impl FreshL4 {
    /// Builds a fresh L4 as a copy of the currently active one.
    ///
    /// The recursive entry (P4 index 511) is re-pointed at the fresh frame
    /// itself: a verbatim copy would leave it referencing the ORIGINAL L4,
    /// and once this table is loaded into CR3 the recursive mapping would
    /// address the old tree instead of this one (stale translations for
    /// every user-region mapping).
    pub fn new() -> Result<Self, PageError> {
        let phys = phys_offset();
        let fresh = super::allocate_frame().ok_or(PageError::NoFrame)?;
        let (active_frame, _) = Cr3::read();

        let fresh_virt = phys + fresh.start_address().as_u64();
        let active_virt = phys + active_frame.start_address().as_u64();

        // Page ops run with IRQs off (same discipline as the mapper ops).
        x86_64::instructions::interrupts::without_interrupts(|| {
            // SAFETY: both pointers target real page-table frames owned by
            // us — the fresh frame has never been used before this copy; the
            // active one is the CPU's current CR3 target, read-only here.
            unsafe {
                let src = active_virt.as_ptr::<PageTable>();
                let dst = fresh_virt.as_mut_ptr::<PageTable>();
                // PageTable has no Copy impl — copy the 512 entries as u64s.
                core::ptr::copy_nonoverlapping(src as *const u64, dst as *mut u64, 512);
                // Self-pointing recursive entry for the fresh tree.
                let table = &mut *dst;
                table[511].set_frame(fresh, PageTableFlags::PRESENT | PageTableFlags::WRITABLE);
                debug_assert_eq!(
                    table[511].addr(),
                    fresh.start_address(),
                    "fresh L4: recursive entry must self-point"
                );
            }
        });

        Ok(FreshL4 { frame: fresh })
    }

    /// Virtual address of the new table through the physical memory map.
    pub fn virt(&self) -> VirtAddr {
        phys_offset() + self.frame.start_address().as_u64()
    }
}

/// Runs `f` with a mapper over a NON-active page-table tree rooted at
/// `root_frame`, accessed through the physical memory map. Built for Step B
/// consumers: mapping into a task's table before (or while not) loading it
/// into CR3 — mapping pages in a tree no CPU can address needs no TLB flush;
/// `f` must not leave stale flush obligations behind for translated pages.
///
/// # Safety
///
/// `root_frame` must head a complete, coherent L4 page-table tree not
/// currently loaded in any CPU's CR3.
pub unsafe fn with_table<F>(root_frame: PhysFrame<Size4KiB>, f: F)
where
    F: FnOnce(&mut OffsetPageTable<'static>),
{
    let phys = phys_offset();
    // SAFETY: caller contract — root_frame heads a coherent, non-active tree.
    let root = (phys + root_frame.start_address().as_u64()).as_mut_ptr::<PageTable>();
    let mut table = unsafe { OffsetPageTable::new(&mut *root, phys) };
    x86_64::instructions::interrupts::without_interrupts(|| f(&mut table));
}
