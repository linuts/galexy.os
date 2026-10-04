//! Kernel heap: a `linked_list_allocator` over a page-mapped virtual area.
//!
//! The heap lives at a fixed fresh virtual address (P4 entry beyond the
//! bootloader's dynamic mappings, which fill from index 0; our fixed
//! mappings are physical memory at 32 and recursive at 511). Backing frames
//! come from the frame allocator, mapped via the paging mapper.

use linked_list_allocator::LockedHeap;
use x86_64::structures::paging::{Page, Size4KiB};
use x86_64::VirtAddr;

use crate::serial_println;

/// Heap start: P4 entry 43 (`0x5555_5555_0000`), canonical, unused.
const HEAP_START: u64 = 0x0000_5555_5555_0000;
/// Heap size in 4 KiB pages.
const HEAP_PAGES: usize = 100;
/// Heap size in bytes (400 KiB).
const HEAP_SIZE: usize = HEAP_PAGES * 4096;

#[global_allocator]
static ALLOCATOR: LockedHeap = LockedHeap::empty();

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
        ALLOCATOR.lock().init(HEAP_START as *mut u8, HEAP_SIZE);
    }
    serial_println!(
        "[heap] ready: {} KiB at {:#x}",
        HEAP_SIZE / 1024,
        HEAP_START
    );
}

/// Heap statistics for the boot banner.
pub fn stats() -> (u64, u64) {
    (HEAP_START, HEAP_SIZE as u64)
}
