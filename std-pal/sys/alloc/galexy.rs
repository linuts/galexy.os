//! `System` allocator: dlmalloc over the kernel's `Map` syscall.
//!
//! `Map(pages)` appends zeroed 4 KiB frames to the task heap and returns
//! their base; successive calls are contiguous, so dlmalloc's segments
//! merge. Nothing is ever unmapped (there is no `Unmap` syscall; the whole
//! heap goes back with the process), so `free` reports `false` and the
//! allocator recycles the chunk itself. The per-task budget is
//! `USER_HEAP_PAGES` (Milestone 71 lifts it); past that, `alloc` returns
//! null and `handle_alloc_error` aborts.
//!
//! One spin lock serializes the threads of a process (dlmalloc is not
//! thread-safe). The holder never blocks inside the allocator, so a waiter
//! yields its quantum instead of spinning hot.

// FIXME(static_mut_refs): use raw pointers instead of references
#![allow(static_mut_refs)]

use crate::alloc::Layout;
use crate::sys::pal::syscall;

/// 16 KiB system-allocation granularity: four `Map` pages per request,
/// so a small program stays well inside the 32-page budget while
/// `Vec` growth does not call the kernel for every doubling.
const GRANULARITY: usize = 4 * 4096;

struct GalexyMap;

// SAFETY: `Map` hands out fresh zeroed pages; nothing else maps the heap
// window.
unsafe impl dlmalloc::Allocator for GalexyMap {
    fn alloc(&self, size: usize) -> (*mut u8, usize, u32) {
        let pages = size.div_ceil(4096).max(1);
        match syscall::map_pages(pages as u64) {
            Ok(base) => (base as *mut u8, pages * 4096, 0),
            // `NoResource`: the per-task budget is spent.
            Err(_) => (crate::ptr::null_mut(), 0, 0),
        }
    }

    fn remap(&self, _ptr: *mut u8, _oldsize: usize, _newsize: usize, _can_move: bool) -> *mut u8 {
        crate::ptr::null_mut()
    }

    fn free_part(&self, _ptr: *mut u8, _oldsize: usize, _newsize: usize) -> bool {
        false
    }

    fn free(&self, _ptr: *mut u8, _size: usize) -> bool {
        false
    }

    fn can_release_part(&self, _flags: u32) -> bool {
        false
    }

    fn allocates_zeros(&self) -> bool {
        true
    }

    fn page_size(&self) -> usize {
        4096
    }
}

#[cfg(not(test))]
#[unsafe(export_name = "_ZN16__rust_internals3std3sys6galexy5alloc8DLMALLOCE")]
static mut DLMALLOC: dlmalloc::Dlmalloc<GalexyMap> = const {
    let mut a = dlmalloc::Dlmalloc::new_with_allocator(GalexyMap);
    assert!(a.set_granularity(GRANULARITY));
    // Nothing is ever released to the kernel, so the periodic release
    // pass is wasted work.
    a.set_max_release_check_rate(0);
    a
};

#[cfg(test)]
unsafe extern "Rust" {
    #[link_name = "_ZN16__rust_internals3std3sys6galexy5alloc8DLMALLOCE"]
    static mut DLMALLOC: dlmalloc::Dlmalloc<GalexyMap>;
}

#[inline]
pub unsafe fn alloc(layout: Layout) -> *mut u8 {
    // SAFETY: DLMALLOC access is guaranteed to be safe because the lock gives us unique and non-reentrant access.
    // Calling malloc() is safe because preconditions on this function match the trait method preconditions.
    let _lock = lock::lock();
    unsafe { DLMALLOC.malloc(layout.size(), layout.align()) }
}

#[inline]
pub unsafe fn alloc_zeroed(layout: Layout) -> *mut u8 {
    // SAFETY: as in `alloc`.
    let _lock = lock::lock();
    unsafe { DLMALLOC.calloc(layout.size(), layout.align()) }
}

#[inline]
pub unsafe fn dealloc(ptr: *mut u8, layout: Layout) {
    // SAFETY: as in `alloc`.
    let _lock = lock::lock();
    unsafe { DLMALLOC.free(ptr, layout.size(), layout.align()) }
}

#[inline]
pub unsafe fn realloc(ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
    // SAFETY: as in `alloc`.
    let _lock = lock::lock();
    unsafe { DLMALLOC.realloc(ptr, layout.size(), layout.align(), new_size) }
}

mod lock {
    use crate::sync::atomic::Ordering::{Acquire, Release};
    use crate::sync::atomic::{Atomic, AtomicI32};

    static LOCKED: Atomic<i32> = AtomicI32::new(0);

    pub struct DropLock;

    pub fn lock() -> DropLock {
        loop {
            if LOCKED.swap(1, Acquire) == 0 {
                return DropLock;
            }
            crate::sys::pal::syscall::yield_now();
        }
    }

    impl Drop for DropLock {
        fn drop(&mut self) {
            let r = LOCKED.swap(0, Release);
            debug_assert_eq!(r, 1);
        }
    }
}
