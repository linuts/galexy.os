//! Thread Local Storage (the xous scheme, with the FS base register as
//! the per-thread table pointer).
//!
//! Each thread owns one 4 KiB table of `*mut u8` slots, allocated from
//! `System` on first use and addressed by the FS base. The kernel saves
//! and restores FS base across thread switches and never writes it in
//! ring 3 (CR4.FSGSBASE lets us do that ourselves), so a fresh thread
//! starts with `0` = "no TLS yet". A key is an index into the table; the
//! index is the same in every thread, the slot it names is not.
//!
//! # Dtor registration (stolen from Windows, via xous)
//!
//! The kernel does not run destructors, so we keep our own list and walk
//! it from `destroy_tls` when a thread exits. Keys are never destroyed.

use crate::alloc::{GlobalAlloc, Layout, System};
use crate::mem::ManuallyDrop;
use crate::ptr;
use crate::sync::atomic::Ordering::{Acquire, Relaxed, Release};
use crate::sync::atomic::{Atomic, AtomicPtr, AtomicUsize};
use crate::sys::pal::syscall::{read_fs_base, write_fs_base};

pub type Key = usize;
pub type Dtor = unsafe extern "C" fn(*mut u8);

const TLS_MEMORY_SIZE: usize = 4096;
const TLS_SLOTS: usize = TLS_MEMORY_SIZE / size_of::<*mut u8>();

/// TLS keys start at `1`. Index `0` is unused
#[cfg(not(test))]
#[unsafe(export_name = "_ZN16__rust_internals3std3sys6galexy16thread_local_key13TLS_KEY_INDEXE")]
static TLS_KEY_INDEX: Atomic<usize> = AtomicUsize::new(1);

#[cfg(not(test))]
#[unsafe(export_name = "_ZN16__rust_internals3std3sys6galexy16thread_local_key9DTORSE")]
static DTORS: Atomic<*mut Node> = AtomicPtr::new(ptr::null_mut());

#[cfg(test)]
unsafe extern "Rust" {
    #[link_name = "_ZN16__rust_internals3std3sys6galexy16thread_local_key13TLS_KEY_INDEXE"]
    static TLS_KEY_INDEX: Atomic<usize>;

    #[link_name = "_ZN16__rust_internals3std3sys6galexy16thread_local_key9DTORSE"]
    static DTORS: Atomic<*mut Node>;
}

#[inline]
fn tls_ptr_addr() -> *mut *mut u8 {
    ptr::with_exposed_provenance_mut(read_fs_base() as usize)
}

/// The calling thread's table, allocating it on first use.
#[inline]
fn tls_table() -> &'static mut [*mut u8] {
    let tp = tls_ptr_addr();
    if !tp.is_null() {
        unsafe { core::slice::from_raw_parts_mut(tp, TLS_SLOTS) }
    } else {
        tls_table_slow()
    }
}

#[cold]
fn tls_table_slow() -> &'static mut [*mut u8] {
    // The System allocator (not the global one) so a `#[global_allocator]`
    // that itself uses TLS cannot recurse into us.
    let layout = rtunwrap!(Ok, Layout::from_size_align(TLS_MEMORY_SIZE, TLS_MEMORY_SIZE));
    let base = unsafe { System.alloc_zeroed(layout) };
    if base.is_null() {
        rtabort!("Unable to allocate memory for thread local storage");
    }
    let tp = base.cast::<*mut u8>();
    write_fs_base(tp.expose_provenance() as u64);
    unsafe { core::slice::from_raw_parts_mut(tp, TLS_SLOTS) }
}

#[inline]
pub fn create(dtor: Option<Dtor>) -> Key {
    // Allocate a new TLS key. These keys are shared among all threads.
    #[allow(unused_unsafe)]
    let key = unsafe { TLS_KEY_INDEX.fetch_add(1, Relaxed) };
    if let Some(f) = dtor {
        unsafe { register_dtor(key, f) };
    }
    key
}

#[inline]
pub unsafe fn set(key: Key, value: *mut u8) {
    rtassert!((key < TLS_SLOTS) && (key >= 1));
    let table = tls_table();
    *rtunwrap!(Some, table.get_mut(key)) = value;
}

#[inline]
pub unsafe fn get(key: Key) -> *mut u8 {
    rtassert!((key < TLS_SLOTS) && (key >= 1));
    let table = tls_table();
    *rtunwrap!(Some, table.get(key))
}

#[inline]
pub unsafe fn destroy(_key: Key) {
    // Just leak the key. Probably not great on long-running systems that create
    // lots of TLS variables, but in practice that's not an issue.
}

struct Node {
    dtor: Dtor,
    key: Key,
    next: *mut Node,
}

unsafe fn register_dtor(key: Key, dtor: Dtor) {
    // We use the System allocator here to avoid interfering with a potential
    // Global allocator using thread-local storage.
    let mut node =
        ManuallyDrop::new(Box::new_in(Node { key, dtor, next: ptr::null_mut() }, System));

    #[allow(unused_unsafe)]
    let mut head = unsafe { DTORS.load(Acquire) };
    loop {
        node.next = head;
        #[allow(unused_unsafe)]
        match unsafe { DTORS.compare_exchange(head, &mut **node, Release, Acquire) } {
            Ok(_) => return, // nothing to drop, we successfully added the node to the list
            Err(cur) => head = cur,
        }
    }
}

/// Runs the destructors of the calling thread's values and frees its table.
pub unsafe fn destroy_tls() {
    let tp = tls_ptr_addr();

    // If the pointer address is 0, then this thread has no TLS.
    if tp.is_null() {
        return;
    }

    unsafe { run_dtors() };

    // Finally, free the TLS array and forget it.
    write_fs_base(0);
    let layout = rtunwrap!(Ok, Layout::from_size_align(TLS_MEMORY_SIZE, TLS_MEMORY_SIZE));
    unsafe { System.dealloc(tp.cast::<u8>(), layout) };
}

// This is marked inline(never) to prevent dealloc calls from being reordered
// to after the TLS has been destroyed.
// See https://github.com/rust-lang/rust/pull/144465#pullrequestreview-3289729950
// for more context.
#[inline(never)]
unsafe fn run_dtors() {
    let mut any_run = true;

    // Run the destructor "some" number of times. This is 5x on Windows,
    // so we copy it here. This allows TLS variables to create new
    // TLS variables upon destruction that will also get destroyed.
    // Keep going until we run out of tries or until we have nothing
    // left to destroy.
    for _ in 0..5 {
        if !any_run {
            break;
        }
        any_run = false;
        #[allow(unused_unsafe)]
        let mut cur = unsafe { DTORS.load(Acquire) };
        while !cur.is_null() {
            let ptr = unsafe { get((*cur).key) };

            if !ptr.is_null() {
                unsafe { set((*cur).key, ptr::null_mut()) };
                unsafe { ((*cur).dtor)(ptr as *mut _) };
                any_run = true;
            }

            unsafe { cur = (*cur).next };
        }
    }

    crate::rt::thread_cleanup();
}
