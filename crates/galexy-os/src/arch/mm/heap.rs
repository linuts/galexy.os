//! Kernel heap: a `linked_list_allocator` over a page-mapped virtual area.
//!
//! The heap lives at a fixed fresh virtual address (P4 entry beyond the
//! bootloader's dynamic mappings, which fill from index 0; our fixed
//! mappings are physical memory at 32 and recursive at 511). Backing frames
//! come from the frame allocator, mapped via the paging mapper.

use core::alloc::{GlobalAlloc, Layout};
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use linked_list_allocator::LockedHeap;
use x86_64::instructions::interrupts;
use x86_64::structures::paging::{Page, Size4KiB};
use x86_64::VirtAddr;

use crate::serial_println;

/// Heap start: P4 entry 43 (`0x5555_5555_0000`), canonical, unused.
const HEAP_START: u64 = 0x0000_5555_5555_0000;
/// Heap size in 4 KiB pages.
const HEAP_PAGES: usize = 100;
/// Heap size in bytes (400 KiB).
const HEAP_SIZE: usize = HEAP_PAGES * 4096;
/// Pages added per growth step (64 KiB chunks).
const GROW_CHUNK_PAGES: usize = 16;
/// One page-run can grow the heap arbitrarily far: P4 entry 43 spans 512 GiB
/// and the P3s it needs are mapped on demand.
const HEAP_MAX_PAGES: usize = 64 * 1024;

/// Currently mapped heap size in bytes (grows over time).
static HEAP_CURRENT_SIZE: AtomicUsize = AtomicUsize::new(HEAP_SIZE);
/// Set once by [`init`]; growth before that is a bug.
static READY: AtomicBool = AtomicBool::new(false);
/// Set while a growth step is in progress (a second OOM allocation must not
/// map the same pages twice; it simply fails instead).
static GROWING: AtomicBool = AtomicBool::new(false);

static INNER: LockedHeap = LockedHeap::empty();

/// Lock-audit adapter (see docs/DESIGN.md): the ONLY preemptor is the timer
/// IRQ, so every lock that preemptable code can hold must be held with
/// interrupts off. Allocation can happen anywhere — wrap it.
pub struct InterruptSafeAlloc;

// SAFETY: all operations delegate to INNER under without_interrupts; the
// inner allocator is itself thread-safe via its own lock. `alloc` may map
// more heap space on exhaustion (grow-on-demand).
unsafe impl GlobalAlloc for InterruptSafeAlloc {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        interrupts::without_interrupts(|| {
            let ptr = INNER.alloc(layout);
            if ptr.is_null() {
                grow();
                return INNER.alloc(layout);
            }
            ptr
        })
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        interrupts::without_interrupts(|| INNER.dealloc(ptr, layout))
    }
}

#[global_allocator]
static ALLOCATOR: InterruptSafeAlloc = InterruptSafeAlloc;

/// Maps the heap pages and activates the global allocator.
///
/// Must be called after [`super::init`] (frame allocator + page mapper) and
/// before any heap allocation (String, Vec, Box, ...).
pub fn init() {
    let start = VirtAddr::new(HEAP_START);
    for i in 0..HEAP_PAGES {
        let page = Page::<Size4KiB>::containing_address(start + (i as u64) * 4096);
        let frame = super::allocate_frame().expect("heap: frame allocation failed");
        super::map_page(page, frame).expect("heap: page mapping failed");
    }
    // SAFETY: [HEAP_START, +HEAP_SIZE) is exclusively mapped above; the
    // allocator takes ownership of the whole range.
    unsafe {
        INNER.lock().init(HEAP_START as *mut u8, HEAP_SIZE);
    }
    READY.store(true, Ordering::Relaxed);
    serial_println!(
        "[heap] ready: {} KiB at {:#x}",
        HEAP_SIZE / 1024,
        HEAP_START
    );
}

/// Grows the heap by one [`GROW_CHUNK_PAGES`] chunk: maps fresh pages at the
/// end of the current range, then feeds them to the allocator.
///
/// Runs with interrupts off (GlobalAlloc adapter contract). No-ops when a
/// growth step is already in flight (a second failing alloc simply fails) or
/// when the cap is reached. Frames live in the frame allocator; failing to
/// get them or map them propagates as an allocation failure (null).
fn grow() -> bool {
    if !READY.load(Ordering::Relaxed) || GROWING.compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed).is_err() {
        return false;
    }
    let current = HEAP_CURRENT_SIZE.load(Ordering::Relaxed);
    let current_pages = current / 4096;
    if current_pages + GROW_CHUNK_PAGES > HEAP_MAX_PAGES {
        GROWING.store(false, Ordering::Release);
        return false;
    }

    let start = VirtAddr::new(HEAP_START + current as u64);
    let mut mapped = 0usize;
    for i in 0..GROW_CHUNK_PAGES {
        let page = Page::<Size4KiB>::containing_address(start + (i as u64) * 4096);
        let Some(frame) = super::allocate_frame() else {
            break;
        };
        if super::map_page(page, frame).is_err() {
            break;
        }
        mapped += 1;
    }
    if mapped == 0 {
        GROWING.store(false, Ordering::Release);
        return false;
    }

    // SAFETY: [HEAP_START + current, +mapped*4096) was mapped above and has
    // never been handed to the allocator; `extend` claims it as one hole.
    unsafe { INNER.lock().extend(mapped * 4096) };
    let new_size = current + mapped * 4096;
    HEAP_CURRENT_SIZE.store(new_size, Ordering::Relaxed);
    GROWING.store(false, Ordering::Release);
    serial_println!(
        "[heap] grown: {} KiB (+{} KiB, {} pages)",
        new_size / 1024,
        mapped * 4,
        mapped
    );
    true
}

/// Heap statistics: `(start, size)`; `size` grows over time.
pub fn stats() -> (u64, u64) {
    (HEAP_START, HEAP_CURRENT_SIZE.load(Ordering::Relaxed) as u64)
}

/// Bytes currently allocated on the heap. IRQ-gated (heap lock).
pub fn used_bytes() -> usize {
    use x86_64::instructions::interrupts;
    interrupts::without_interrupts(|| INNER.lock().used())
}

/// Bytes currently free on the heap. IRQ-gated (heap lock).
pub fn free_bytes() -> usize {
    use x86_64::instructions::interrupts;
    interrupts::without_interrupts(|| INNER.lock().free())
}
