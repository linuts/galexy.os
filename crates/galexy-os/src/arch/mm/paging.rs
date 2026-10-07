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

use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use spin::Mutex;
use x86_64::registers::control::{Cr3, Cr3Flags};
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
/// Physical-memory offset (from `BOOTLOADER_CONFIG`); 0 before init.
///
/// ATOMIC, not a mutex: `phys_offset()`/`frame_virt()` are reachable from
/// IF=0 contexts (the write syscall's active-tree buffer walk) — a lock
/// whose holder can be preemptable main-loop code would wedge the timer
/// (the lock-audit rule). The value is fixed once at init and never
/// changes.
static PHYS_OFFSET: AtomicU64 = AtomicU64::new(0);
/// The kernel's page-table root (physical address), cached at [`init`].
/// [`FreshL4`] copies this root, not the table in CR3. Every task tree
/// shares its kernel half verbatim.
static KERNEL_CR3: AtomicU64 = AtomicU64::new(0);

/// Physical-memory offset accessor for internal use (panics if unset).
fn phys_offset() -> VirtAddr {
    let v = PHYS_OFFSET.load(Ordering::Relaxed);
    assert!(
        v != 0,
        "paging: not initialized (no physical memory offset)"
    );
    VirtAddr::new(v)
}

/// Initializes the mapper over the currently active page tables. Requires
/// the physical memory mapping from `BOOTLOADER_CONFIG`; idempotent.
pub fn init(phys_offset: u64) {
    PHYS_OFFSET
        .compare_exchange(0, phys_offset, Ordering::Release, Ordering::Relaxed)
        .expect("paging: physical memory offset already set to a different value");
    let mut mapper = MAPPER.lock();
    if mapper.is_some() {
        return;
    }
    let level_4_table = unsafe { active_level_4_table(VirtAddr::new(phys_offset)) };
    // SAFETY: the L4 table is the CPU's active one (read via CR3) and is
    // only ever accessed through the MAPPER lock below.
    *mapper = Some(unsafe { OffsetPageTable::new(level_4_table, VirtAddr::new(phys_offset)) });
    drop(mapper);
    // Cache the kernel's table root once (the bootloader's active CR3).
    let (kernel_frame, _) = Cr3::read();
    KERNEL_CR3.store(kernel_frame.start_address().as_u64(), Ordering::Relaxed);
    READY.store(true, Ordering::Relaxed);
    serial_println!("[mm] page mapper ready");
}

/// The kernel's page-table root (physical frame), cached at [`init`].
pub fn kernel_cr3() -> PhysFrame<Size4KiB> {
    let addr = KERNEL_CR3.load(Ordering::Relaxed);
    assert!(
        addr != PhysAddr::zero().as_u64(),
        "paging: kernel CR3 not cached (init?)"
    );
    // SAFETY: the cached address came from a real Cr3::read(); frame lookup
    // is infallible for an aligned 4 KiB frame base.
    unsafe { PhysFrame::from_start_address_unchecked(PhysAddr::new(addr)) }
}

/// Installs `frame` as CR3 (no-op when it's already active — the Redox
/// pattern: swapping costs a full TLB flush, so only swap when different).
///
/// Runs with IRQs off (callers are the switch paths under the gate).
pub fn install_cr3(frame: PhysFrame<Size4KiB>) {
    x86_64::instructions::interrupts::without_interrupts(|| {
        let (current, _) = Cr3::read();
        if current != frame {
            // SAFETY: `frame` heads a complete page-table tree whose kernel
            // half is shared with the table currently active (the FreshL4
            // contract), so the switch is safe from any kernel context.
            unsafe { Cr3::write(frame, Cr3Flags::empty()) }
        }
    });
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
    assert!(
        READY.load(Ordering::Relaxed),
        "paging: mapper not initialized"
    );
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
        let mut frame_alloc = TaskFrameAlloc;
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

/// Maps one KERNEL-half page (PRESENT | WRITABLE | NO_EXECUTE) and broadcasts
/// a TLB shootdown for it to every other CPU. The kernel half is shared
/// memory across all CPUs and task trees — a remap here is visible machine-
/// wide, so the local `map_page` flush is not enough.
///
/// Caller contract (the shootdown deadlock rule): NO Rust spin lock may be
/// held across the call — targets ack through lock-free IPI handlers, but a
/// target blocked IF=0 on a lock held by the initiator could never run its
/// handler. Lock holds stay short + IPI-free.
pub fn map_kernel_page_broadcast(
    page: Page<Size4KiB>,
    frame: PhysFrame<Size4KiB>,
) -> Result<(), PageError> {
    map_page(page, frame)?; // mapper lock + local flush; released on return
    super::shootdown::shootdown_others(&[page.start_address()]);
    Ok(())
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
///
/// Walks the KERNEL'S BOOT TREE (the mapper's fixed root): for
/// kernel-side addresses only. User-task addresses must go through
/// [`translate_active`] — since per-task address spaces (Step B) a ring-3
/// buffer lives in the calling task's own tree, invisible to this walk.
pub fn translate(virt: VirtAddr) -> Option<PhysAddr> {
    let mut result = None;
    with_mapper(|mapper| {
        result = mapper.translate_addr(virt);
    });
    result
}

/// Translates a virtual address in the CURRENTLY ACTIVE tree (CR3).
///
/// The write syscall validates user buffers here: the syscall runs with
/// the calling task's CR3 active, and its buffer lives in its own tree —
/// the kernel-tree [`translate`] cannot see it (it would report BadBuffer
/// for every user buffer; the M14 per-task-tree regression this closed).
///
/// Read-only 4-level walk through the phys map (the mapper is rooted at
/// the boot tree and cannot serve non-active trees). Huge-page entries
/// resolve to their frame base + intra-page offset. The task tree cannot
/// change mid-syscall (IF=0; switches happen only via timer/syscall
/// handoff).
pub fn translate_active(virt: VirtAddr) -> Option<PhysAddr> {
    walk_active(virt).map(|leaf| leaf.phys)
}

/// Leaf page flags for `virt` in the active tree.
///
/// `None` when the walk misses. Syscalls use this to require
/// `USER_ACCESSIBLE` (and `WRITABLE` for a destination) instead of treating
/// every present kernel page as a user buffer.
pub fn active_leaf_flags(virt: VirtAddr) -> Option<PageTableFlags> {
    walk_active(virt).map(|leaf| leaf.flags)
}

struct ActiveLeaf {
    phys: PhysAddr,
    flags: PageTableFlags,
}

/// Walks the active CR3 tree. See [`translate_active`].
fn walk_active(virt: VirtAddr) -> Option<ActiveLeaf> {
    assert!(
        READY.load(Ordering::Relaxed),
        "paging: mapper not initialized"
    );
    let (root, _) = Cr3::read();
    let phys = phys_offset();
    // SAFETY: the active CR3 target heads a complete page-table tree; the
    // walk only reads through the phys map, never writes.
    unsafe {
        let table = &*(phys + root.start_address().as_u64()).as_ptr::<PageTable>();
        let indices = [
            usize::from(virt.p4_index()),
            usize::from(virt.p3_index()),
            usize::from(virt.p2_index()),
            usize::from(virt.p1_index()),
        ];
        let mut entry = &table[indices[0]];
        for i in 1..4 {
            if !entry.flags().contains(PageTableFlags::PRESENT) {
                return None;
            }
            // 1 GiB (under P4) / 2 MiB (under P3) leaf: base + offset.
            if entry.flags().contains(PageTableFlags::HUGE_PAGE) {
                let size: u64 = if i == 1 { 1 << 30 } else { 1 << 21 };
                let base = entry.addr().as_u64() & !(size - 1);
                return Some(ActiveLeaf {
                    phys: PhysAddr::new(base + (virt.as_u64() & (size - 1))),
                    flags: entry.flags(),
                });
            }
            let frame = entry.frame().ok()?;
            let next = &*(phys + frame.start_address().as_u64()).as_ptr::<PageTable>();
            entry = &next[indices[i]];
        }
        if !entry.flags().contains(PageTableFlags::PRESENT) {
            return None;
        }
        Some(ActiveLeaf {
            phys: entry.frame().ok()?.start_address() + u64::from(virt.page_offset()),
            flags: entry.flags(),
        })
    }
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

/// The highest FREE P4 entry index in the user half (`< 256`) of the table
/// tree rooted at `root`, scanning top-down. Used for per-task user regions:
/// each task's fresh tree is private, so two tasks may pick the SAME index
/// and still never see each other's pages.
pub fn top_user_p4_index_in(root: PhysFrame<Size4KiB>) -> Option<u16> {
    assert!(
        READY.load(Ordering::Relaxed),
        "paging: mapper not initialized"
    );
    let phys = phys_offset();
    let l4 = phys + root.start_address().as_u64();
    // SAFETY: `root` heads a complete page-table tree (FreshL4 contract);
    // read-only scan through the phys map.
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

/// [`top_user_p4_index_in`] for the currently active tree.
pub fn top_user_p4_index() -> Option<u16> {
    let (l4_frame, _) = Cr3::read();
    top_user_p4_index_in(l4_frame)
}

/// Is the kernel's (boot) table the active one?
///
/// Spawn stays here: the loader allocates, and a syscall runs with
/// interrupts off, so the load stays on the main loop. [`FreshL4`] copies
/// [`kernel_cr3`], not whichever table is active.
pub fn on_kernel_tree() -> bool {
    let (current, _) = Cr3::read();
    current == kernel_cr3()
}

/// Adapter: feeds the x86_64 crate's mapping machinery from our frame
/// allocator — needed for page-table frames. Public so non-paging call
/// sites (sched's task-tree mapping) can run `map_to` themselves.
pub struct TaskFrameAlloc;

// SAFETY: allocate_frame hands out Usable, otherwise-unmapped frames; the
// mapping machinery never deallocates through this adapter.
unsafe impl FrameAllocator<Size4KiB> for TaskFrameAlloc {
    fn allocate_frame(&mut self) -> Option<PhysFrame<Size4KiB>> {
        super::allocate_frame()
    }
}

/* ---------------- fresh (non-active) page tables — Step B groundwork ---- */

/// Physical frame of a freshly allocated, near-verbatim copy of the kernel
/// L4 cached at [`init`]. Kernel higher-half entries are shared (same
/// frames). The copy does not follow the active CR3, so a task's user
/// mappings stay in that task's table. The recursive entry self-points at
/// the fresh frame.
pub struct FreshL4 {
    /// Physical frame of the new top-level table.
    pub frame: PhysFrame<Size4KiB>,
}

impl FreshL4 {
    /// Builds a fresh L4 as a copy of the kernel root cached at [`init`].
    ///
    /// The recursive entry (P4 index 511) is re-pointed at the fresh frame
    /// itself: a verbatim copy would leave it referencing the kernel L4,
    /// and once this table is loaded into CR3 the recursive mapping would
    /// address the kernel tree instead of this one (stale translations for
    /// every user-region mapping).
    pub fn new() -> Result<Self, PageError> {
        let phys = phys_offset();
        let fresh = super::allocate_frame().ok_or(PageError::NoFrame)?;
        let kernel = kernel_cr3();

        let fresh_virt = phys + fresh.start_address().as_u64();
        let kernel_virt = phys + kernel.start_address().as_u64();

        // Page ops run with IRQs off (same discipline as the mapper ops).
        x86_64::instructions::interrupts::without_interrupts(|| {
            // SAFETY: both pointers target real page-table frames owned by
            // us — the fresh frame has never been used before this copy; the
            // kernel root was cached from CR3 at init and is read-only here.
            // The copy goes through the phys map, so it does not matter which
            // table is in CR3.
            unsafe {
                let src = kernel_virt.as_ptr::<PageTable>();
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

/// Frees EVERY frame a task owns: the subtree under its own P4 entry
/// (allocated fresh at spawn — the kernel's shared subtrees live under
/// OTHER entries and are never touched) plus the root frame itself.
/// Returns the number of frames freed (page-table + data frames).
///
/// The tree must NOT be CR3-active (tombstoned task — the reaper's contract).
pub fn free_user_tree(root: PhysFrame<Size4KiB>, p4_index: u16) -> usize {
    assert!(
        READY.load(Ordering::Relaxed),
        "paging: mapper not initialized"
    );
    let phys = phys_offset();
    let mut freed = 0usize;

    // SAFETY: root heads a coherent, non-active tree (tombstoned task);
    // read access via the phys map to find the task's P3.
    let p3 = unsafe {
        let l4 = (phys + root.start_address().as_u64()).as_ptr::<PageTable>();
        // PageTable = 512 entries starting at the table pointer itself.
        let entry_ptr: *const x86_64::structures::paging::page_table::PageTableEntry =
            l4.byte_add(usize::from(p4_index) * 8).cast();
        let entry = core::ptr::read(entry_ptr);
        if entry.is_unused() {
            None
        } else {
            Some(entry.frame().expect("free_user_tree: entry without frame"))
        }
    };

    if let Some(p3_frame) = p3 {
        // SAFETY: the subtree is task-owned, coherent, not CR3-active.
        freed += unsafe { free_table_level(p3_frame, 3) };
    }
    super::deallocate_frame(root);
    freed + 1
}

/// Frees a page-table frame and everything under it. `level` 3 = P3
/// (children: P2), 2 = P2 (children: P1), 1 = P1 (entries: data frames —
/// freed directly, no recursion). Returns the count.
///
/// # Safety
///
/// The subtree must belong exclusively to the caller (non-active task
/// tree); no aliasing walkers may run concurrently (single-core + gate).
unsafe fn free_table_level(frame: PhysFrame<Size4KiB>, level: u8) -> usize {
    let phys = phys_offset();
    let mut count = 1usize; // this frame
                            // SAFETY: contract above.
    unsafe {
        let table: *const PageTable = (phys + frame.start_address().as_u64()).as_ptr();
        for i in 0..512usize {
            let entry_ptr: *const x86_64::structures::paging::page_table::PageTableEntry =
                table.byte_add(i * 8).cast();
            let entry = core::ptr::read(entry_ptr);
            if entry.is_unused() {
                continue;
            }
            let child = entry.frame().expect("free tree: non-frame entry");
            if level == 1 {
                super::deallocate_frame(child);
                count += 1;
            } else {
                count += free_table_level(child, level - 1);
            }
        }
    }
    super::deallocate_frame(frame);
    count
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
pub unsafe fn with_table<F, R>(root_frame: PhysFrame<Size4KiB>, f: F) -> R
where
    F: FnOnce(&mut OffsetPageTable<'static>) -> R,
{
    let phys = phys_offset();
    // SAFETY: caller contract — root_frame heads a coherent, non-active tree.
    let root = (phys + root_frame.start_address().as_u64()).as_mut_ptr::<PageTable>();
    let mut table = unsafe { OffsetPageTable::new(&mut *root, phys) };
    x86_64::instructions::interrupts::without_interrupts(|| f(&mut table))
}
