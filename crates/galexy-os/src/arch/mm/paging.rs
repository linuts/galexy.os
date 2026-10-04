//! Virtual memory paging: page-table access and page mapping.
//!
//! The mapper operates on the bootloader-created active page tables, accessed
//! through the recursive mapping configured in `BOOTLOADER_CONFIG` (canonical,
//! P4 index 511). Initialization reads CR3 directly.

use spin::Mutex;
use x86_64::registers::control::Cr3;
use x86_64::structures::paging::{
    FrameAllocator, Mapper, OffsetPageTable, Page, PageTable, PhysFrame, Size4KiB, Translate,
};
use x86_64::{PhysAddr, VirtAddr};

use crate::serial_println;

/// The active page-table mapper; `None` until [`init`] runs.
static MAPPER: Mutex<Option<OffsetPageTable<'static>>> = Mutex::new(None);

/// Initializes the mapper over the currently active page tables. Requires
/// the physical memory mapping from `BOOTLOADER_CONFIG`; idempotent.
pub fn init(phys_offset: u64) {
    let mut mapper = MAPPER.lock();
    if mapper.is_some() {
        return;
    }
    let level_4_table = unsafe { active_level_4_table(VirtAddr::new(phys_offset)) };
    // SAFETY: the L4 table is the CPU's active one (read via CR3) and is
    // only ever accessed through the MAPPER lock below.
    *mapper = Some(unsafe { OffsetPageTable::new(level_4_table, VirtAddr::new(phys_offset)) });
    drop(mapper);
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

/// Runs `f` with the mapper (panics if `init` hasn't run).
fn with_mapper<F>(f: F)
where
    F: FnOnce(&mut OffsetPageTable<'static>),
{
    let mut mapper = MAPPER.lock();
    let mapper = mapper.as_mut().expect("paging: mapper not initialized");
    f(mapper);
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
    use x86_64::structures::paging::mapper::MapToError;
    let flags = x86_64::structures::paging::PageTableFlags::PRESENT
        | x86_64::structures::paging::PageTableFlags::WRITABLE
        | x86_64::structures::paging::PageTableFlags::NO_EXECUTE;
    let mut result = Ok(());
    with_mapper(|mapper| {
        let mut frame_alloc = PageTableFrameAllocator;
        // SAFETY: exclusive access via MAPPER; frame is Usable per the
        // allocator contract.
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
