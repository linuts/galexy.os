//! Physical memory management (x86_64).
//!
//! Frames of `Usable` memory-map regions are tracked in fixed `.bss`
//! bitmaps (`USED` + `USABLE`), sized to cover the first `TRACKED_FRAMES`
//! of RAM. Const-initialized statics mean no stack pressure at init time.
//!
//! Frames outside that range or outside `Usable` regions can never be
//! allocated, and deallocating them panics — so bootloader-owned and
//! kernel-owned memory is protected by construction.
//!
//! Virtual memory (page mapping) lives in the [`paging`] submodule.

pub mod paging;

pub mod heap;
pub use paging::{
    frame_virt, free_user_tree, install_cr3, kernel_cr3, map_page, map_page_flags,
    on_kernel_tree, phys_to_virt, top_user_p4_index, top_user_p4_index_in, translate,
    translate_active, unmap_page, FreshL4, PageError, TaskFrameAlloc, with_table,
};

use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use galexy_core::Bitmap;
use spin::Mutex;
use x86_64::structures::paging::{PhysFrame, Size4KiB};
use x86_64::PhysAddr;

use crate::serial_println;
use bootloader_api::info::{BootInfo, MemoryRegionKind};

/// Frames tracked by the allocator (2 Ki bitmap = 512 MiB coverage).
const BITMAP_WORDS: usize = 2048;
/// Physical page size.
const FRAME_SIZE: usize = 4096;
/// Total trackable frames.
const TRACKED_FRAMES: usize = BITMAP_WORDS * 64;

/// `USED[frame]` = 1 when allocated or non-usable (starts all-used at init).
static USED: Mutex<Bitmap<BITMAP_WORDS>> = Mutex::new(Bitmap::new());
/// `USABLE[frame]` = 1 only for frames inside `Usable` regions.
static USABLE: Mutex<Bitmap<BITMAP_WORDS>> = Mutex::new(Bitmap::new());
/// Number of currently free frames.
static FREE_COUNT: AtomicUsize = AtomicUsize::new(0);
/// Set once by [`init`]; allocation before that is a bug.
static READY: AtomicBool = AtomicBool::new(false);

/// Initializes the frame allocator from the bootloader's memory map.
///
/// Must be called exactly once, before any allocation. Requires
/// `physical_memory_offset` to be mapped (see `BOOTLOADER_CONFIG`).
pub fn init(boot_info: &BootInfo) {
    let phys_offset = boot_info
        .physical_memory_offset
        .into_option()
        .expect("physical memory must be mapped (see BOOTLOADER_CONFIG)");

    paging::init(phys_offset);

    // Lock order: USED before USABLE (the only nested locking in this module).
    let mut used = USED.lock();
    let mut usable = USABLE.lock();
    used.fill(true); // all frames start used; `Usable` regions are freed below

    let mut free = 0usize;
    let mut skipped = 0u64;
    for region in boot_info.memory_regions.iter() {
        if region.kind != MemoryRegionKind::Usable {
            continue;
        }
        let trackable_end = (TRACKED_FRAMES * FRAME_SIZE) as u64;
        let covered_end = region.end.min(trackable_end);
        skipped += region.end.saturating_sub(covered_end);
        let start_frame = (region.start as usize).div_ceil(FRAME_SIZE);
        let end_frame = (covered_end as usize) / FRAME_SIZE;
        for frame in start_frame..end_frame {
            usable.set(frame, true);
            used.set(frame, false);
            free += 1;
        }
    }
    drop(usable);
    drop(used);

    if skipped > 0 {
        serial_println!(
            "[mm] warning: {} bytes of usable memory beyond allocator coverage are unused",
            skipped
        );
    }
    FREE_COUNT.store(free, Ordering::Relaxed);
    READY.store(true, Ordering::Relaxed);
    serial_println!("[mm] frame allocator ready: {} free frames", free);

    // Virtual memory is up: bring the heap online too.
    heap::init();
}

/// Allocates a physical 4 KiB frame, first-fit.
pub fn allocate_frame() -> Option<PhysFrame<Size4KiB>> {
    if !READY.load(Ordering::Relaxed) {
        panic!("allocate_frame: allocator not initialized");
    }
    let mut used = USED.lock();
    let index = used.first_clear()?;
    used.set(index, true);
    drop(used);
    FREE_COUNT.fetch_sub(1, Ordering::Relaxed);
    PhysFrame::from_start_address(PhysAddr::new((index * FRAME_SIZE) as u64)).ok()
}

/// Returns a previously allocated frame to the allocator.
///
/// # Panics
///
/// Panics on double-free and on frames outside the allocator's tracked range
/// (including all non-`Usable` frames — bootloader/kernel memory).
pub fn deallocate_frame(frame: PhysFrame<Size4KiB>) {
    let index = (frame.start_address().as_u64() as usize) / FRAME_SIZE;
    let mut used = USED.lock();
    let usable = USABLE.lock();
    assert!(
        index < TRACKED_FRAMES,
        "deallocate_frame: frame outside tracked range"
    );
    assert!(
        usable.test(index),
        "deallocate_frame: frame was never allocatable (not Usable)"
    );
    assert!(used.test(index), "deallocate_frame: double free of frame");
    used.set(index, false);
    drop(usable);
    drop(used);
    FREE_COUNT.fetch_add(1, Ordering::Relaxed);
}

/// Number of currently free frames.
pub fn free_frames() -> usize {
    FREE_COUNT.load(Ordering::Relaxed)
}
