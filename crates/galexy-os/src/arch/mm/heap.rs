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
/// map the same pages twice; it waits for the in-flight chunk instead).
static GROWING: AtomicBool = AtomicBool::new(false);
/// Spins a second grower waits behind an in-flight growth before declaring
/// the grower wedged (2^28 pause-loads: tens of seconds under loaded TCG,
/// a few on metal — a real growth step is microseconds to milliseconds).
const GROW_WAIT_SPINS: u64 = 1 << 28;

static INNER: LockedHeap = LockedHeap::empty();

/// Lock-audit adapter (see docs/DESIGN.md): the ONLY preemptor is the timer
/// IRQ, so every lock that preemptable code can hold must be held with
/// interrupts off. Allocation can happen anywhere — wrap it.
pub struct InterruptSafeAlloc;

// SAFETY: all operations delegate to INNER; the inner allocator is itself
// thread-safe via its own lock. `alloc` may map more heap space on
// exhaustion (grow-on-demand).
//
// GATE SPLIT (SMP M19): the fast path holds the gate (lock-audit rule —
// the heap lock must never be held by preemptable code with IF=1), but the
// OOM path runs LOCK-FREE between gates: `grow` maps under the gate, then
// broadcasts its shootdowns with NO lock of its own held and NO gate held.
// The CALLER may still hold locks (any `format!` under `THREADS`, say) —
// `GlobalAlloc` cannot know. That is why every kernel spin lock services
// shootdowns from its spin loop (`crate::sync`): a target blocked IF=0 on
// the caller's lock acks anyway. Naked/IRQ paths never reach the OOM path
// at all (they do not allocate).
unsafe impl GlobalAlloc for InterruptSafeAlloc {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        loop {
            let ptr = interrupts::without_interrupts(|| INNER.alloc(layout));
            if !ptr.is_null() {
                return ptr;
            }
            if !grow() {
                return core::ptr::null_mut();
            }
            // A chunk landed (ours, or another CPU's in-flight growth we
            // waited out) — the retry may fit now; otherwise grow again.
        }
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
/// end of the current range (kernel half — broadcast-remapped, SMP M19),
/// then feeds them to the allocator.
///
/// SMP protocol: the GROWING flag serializes growth machine-wide. When a
/// second CPU hits OOM mid-growth, it does NOT fail — it waits for the
/// in-flight chunk to land, servicing the other CPU's shootdown mailbox
/// while it spins (the broadcaster needs our ack; polling delivers it
/// without re-enabling interrupts inside the caller's gate), and reports
/// progress so the caller retries its alloc.
///
/// Gate discipline: mapping + extend run under the gate (mapper/heap
/// locks); the shootdown broadcast runs LOCK-FREE between them (no Rust
/// lock may be held across it — see `mm::shootdown`). Naked/IRQ paths never
/// allocate, so they never reach here. Returns `true` when the heap gained
/// at least one page since the caller's failed alloc.
fn grow() -> bool {
    if !READY.load(Ordering::Relaxed) {
        return false;
    }
    if GROWING
        .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
        .is_err()
    {
        // Another CPU is mid-growth. Wait for its chunk to land. Its
        // broadcast needs OUR ack, so service the mailbox while spinning
        // — the caller may be IF=0 inside a gate holding locks, and
        // enabling interrupts here would let the timer preempt that gate
        // (the lock-audit rule); the poll acks without touching IF.
        // Watchdog: a stuck grower is a kernel bug — fail loudly instead
        // of hanging. Bounded by spins, not ticks: with IF=0 on every CPU
        // the tick counter may not advance.
        let mut spins: u64 = 0;
        while GROWING.load(Ordering::Relaxed) {
            super::shootdown::service_pending();
            core::hint::spin_loop();
            spins += 1;
            if spins > GROW_WAIT_SPINS {
                panic!("heap: stuck behind in-flight growth (deadlocked grower?)");
            }
        }
        return true; // progress happened; the caller's retry may fit now
    }
    let current = HEAP_CURRENT_SIZE.load(Ordering::Relaxed);
    let current_pages = current / 4096;
    if current_pages + GROW_CHUNK_PAGES > HEAP_MAX_PAGES {
        GROWING.store(false, Ordering::Release);
        return false;
    }

    let start = VirtAddr::new(HEAP_START + current as u64);
    // Map the chunk under the gate: the mapper lock + frame allocator are
    // IRQ-gated by their own APIs; the gate here keeps the whole map loop
    // atomic against the timer switch (the caller holds no locks we need).
    let mut mapped = 0usize;
    let mut broadcast_vas = [0u64; GROW_CHUNK_PAGES];
    interrupts::without_interrupts(|| {
        for (i, va) in broadcast_vas.iter_mut().enumerate() {
            let page = Page::<Size4KiB>::containing_address(start + (i as u64) * 4096);
            let Some(frame) = super::allocate_frame() else {
                break;
            };
            if super::map_page(page, frame).is_err() {
                break;
            }
            *va = page.start_address().as_u64();
            mapped += 1;
        }
    });
    if mapped == 0 {
        GROWING.store(false, Ordering::Release);
        return false;
    }

    // Shootdown broadcast: LOCK-FREE and holding NO lock by design (the
    // deadlock rule). Fresh mappings technically cannot sit stale in other
    // CPUs' TLBs, but broadcasting here MECHANIZES the "kernel half is
    // map-only" assumption — every kernel-half remap flows through the
    // shootdown path from day one. The VA list lives on the stack: this is
    // the OOM path with GROWING held, so a heap allocation here could fail
    // and re-enter `grow`, which would then wait on itself.
    let mut vas = [VirtAddr::zero(); GROW_CHUNK_PAGES];
    for (dst, &src) in vas.iter_mut().zip(&broadcast_vas[..mapped]) {
        *dst = VirtAddr::new(src);
    }
    let seq = super::shootdown::shootdown_others(&vas[..mapped]);

    // SAFETY: [HEAP_START + current, +mapped*4096) was mapped above and has
    // never been handed to the allocator; `extend` claims it as one hole.
    interrupts::without_interrupts(|| unsafe { INNER.lock().extend(mapped * 4096) });
    let new_size = current + mapped * 4096;
    HEAP_CURRENT_SIZE.store(new_size, Ordering::Relaxed);
    GROWING.store(false, Ordering::Release);
    serial_println!(
        "[heap] grown: {} KiB (+{} KiB, {} pages, shootdown seq {})",
        new_size / 1024,
        mapped * 4,
        mapped,
        seq
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
