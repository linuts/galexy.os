//! TLB shootdown: precise-INVLPG IPI broadcast (SMP, Milestone 19).
//!
//! Kernel-half page-table entries are shared memory across every CPU (and
//! every task tree). The moment one CPU remaps a kernel-half page, every
//! OTHER CPU's TLB may hold a stale translation — mechanizing the old M18
//! "kernel half is map-only" assumption away.
//!
//! Protocol (deadlock rule baked in):
//! - The INITIATOR claims a mailbox slot, writes the VAs, publishes a
//!   monotonically increasing sequence number (`seq`, Release), sends a
//!   FIXED IPI on vector 0xF8 to every other online CPU, and spins until
//!   every target's per-slot `seen` value has caught up with `seq`.
//! - TARGETS run the vector 0xF8 handler — a LOCK-FREE x86-interrupt
//!   handler: scan the mailbox for unseen `seq` values, run `invlpg` on
//!   each listed VA, record `seen`. No locks, ever: a target mid-IRQ-gated
//!   critical section (or spinning on a lock) must still be able to take
//!   the IPI and ack, or a broadcasting initiator could never progress.
//! - CALLER contract (the deadlock rule): the initiator must hold NO Rust
//!   spin lock while broadcasting — a target that would block IF=0 on that
//!   lock could never service the IPI, and the initiator would spin
//!   forever. Lock holds stay short + IPI-free; the broadcast is lock-free.
//!
//! ABA: `seq` values come from one machine-global monotonic counter, and a
//! slot is only reused once EVERY target's `seen` has consumed its last
//! `seq` — a target can never mistake a new request for an old one (and a
//! stale re-process is harmless anyway: invlpg is idempotent).

use core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use x86_64::structures::idt::InterruptStackFrame;
use x86_64::VirtAddr;

use crate::arch::cpu;
use crate::serial_println;

/// The dedicated shootdown IPI vector (top-of-range, no device contention).
pub const SD_VECTOR: u8 = 0xF8;
/// VAs carried per mailbox slot (one heap-growth chunk = 16 pages fits one
/// slot exactly, per the M19 design).
pub const SLOT_VAS: usize = 16;
/// Concurrent mailbox slots (broadcasts in flight at once).
const SLOT_N: usize = 8;

/// One broadcast mailbox: VAs + the publishing sequence number.
struct SdSlot {
    /// The request's machine-global sequence number (monotonic; the ABA
    /// counter). A target processes a slot whenever its `seen` differs.
    seq: AtomicU64,
    /// Number of valid VAs in [`SdSlot::vas`] (≤ [`SLOT_VAS`]).
    len: AtomicUsize,
    /// Page-base VAs to INVLPG (plain writes, ordered by the `seq` release).
    vas: [AtomicU64; SLOT_VAS],
    /// In-flight marker: claimed via CAS by an initiator; released only
    /// after every target's `seen` consumed the request's `seq`. Two
    /// concurrent initiators can therefore never share a slot.
    busy: AtomicBool,
}

impl SdSlot {
    const fn new() -> Self {
        Self {
            seq: AtomicU64::new(0),
            len: AtomicUsize::new(0),
            vas: [const { AtomicU64::new(0) }; SLOT_VAS],
            busy: AtomicBool::new(false),
        }
    }
}

/// The mailbox pool.
static MAILBOX: [SdSlot; SLOT_N] = [const { SdSlot::new() }; SLOT_N];
/// Machine-global request counter (`seq` source — strictly increasing).
static GLOBAL_SEQ: AtomicU64 = AtomicU64::new(0);
/// Per-CPU `seen` matrix: `SEEN[cpu][slot]` = the last `seq` that CPU's
/// handler has fully processed for that slot. WRITTEN only by the owning
/// CPU's IPI handler; READ cross-CPU by waiting initiators (atomics).
static SEEN: [[AtomicU64; SLOT_N]; cpu::MAX_CPUS] =
    [const { [const { AtomicU64::new(0) }; SLOT_N] }; cpu::MAX_CPUS];
/// Number of completed broadcasts (diagnostics + test assertions).
static BROADCASTS: AtomicU64 = AtomicU64::new(0);

/// Broadcasts a shootdown of `vas` (page bases) to every other online CPU
/// and waits for all of them to confirm the INVLPG. Returns the request's
/// sequence number — when this returns, every target's `seen` for the slot
/// has reached it (the ack itself, observable by tests).
///
/// Caller contract: NO Rust spin lock may be held across this call (see the
/// module docs); any IRQ state is fine — targets ack whenever they next run
/// IF=1 code.
pub fn shootdown_others(vas: &[VirtAddr]) -> u64 {
    debug_assert!(
        !vas.is_empty() && vas.len() <= SLOT_VAS,
        "shootdown: broadcast must carry 1..=SLOT_VAS VAs"
    );
    let me = cpu::current_index();
    let (slot_index, slot) = claim_slot();

    // Publish the request: VAs first, then the seq (Release) — targets read
    // the VAs only after Acquiring the new seq.
    let seq = GLOBAL_SEQ.fetch_add(1, Ordering::Relaxed) + 1;
    for (i, v) in vas.iter().take(SLOT_VAS).enumerate() {
        slot.vas[i].store(v.as_u64(), Ordering::Relaxed);
    }
    slot.len.store(vas.len().min(SLOT_VAS), Ordering::Relaxed);
    slot.seq.store(seq, Ordering::Release);

    // Broadcast: every other online CPU (slot lookup; a slot is "online"
    // once its per-CPU bring-up filled the identity fields).
    for c in 0..cpu::MAX_CPUS {
        if c == me {
            continue;
        }
        if let Some(apic_id) = cpu::apic_id_of(c) {
            crate::arch::apic::send_fixed_ipi(apic_id as u8, SD_VECTOR);
        }
    }

    // Wait for every target to consume THIS seq for THIS slot (the per-CPU
    // per-slot ack the initiator spins on). Lock-free spin: targets' IPI
    // handlers take no locks, so this always converges.
    for (c, row) in SEEN.iter().enumerate() {
        if c == me || cpu::apic_id_of(c).is_none() {
            continue;
        }
        while row[slot_index].load(Ordering::Acquire) < seq {
            core::hint::spin_loop();
        }
    }

    // Fully consumed everywhere: safe to hand the slot to the next initiator.
    slot.busy.store(false, Ordering::Release);
    BROADCASTS.fetch_add(1, Ordering::Relaxed);
    seq
}

/// Claims a free mailbox slot via CAS (spin when all are in flight — the
/// wait is bounded: in-flight broadcasts complete against lock-free
/// handlers). Returns the slot's index and a reference to it.
fn claim_slot() -> (usize, &'static SdSlot) {
    loop {
        for (i, s) in MAILBOX.iter().enumerate() {
            if s.busy
                .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
                .is_ok()
            {
                return (i, s);
            }
        }
        core::hint::spin_loop();
    }
}

/// The vector 0xF8 handler: LOCK-FREE by the deadlock rule. Scans every
/// mailbox slot whose `seq` differs from this CPU's `seen`, INVLPGs each
/// listed VA, records `seen`, EOIs. Runs in IRQ context on the target's
/// current stack — it must never touch a lock (a target holding ANY lock
/// while IF=0 would deadlock the broadcaster otherwise).
pub(crate) extern "x86-interrupt" fn shootdown_handler(_frame: InterruptStackFrame) {
    let me = cpu::current_index();
    for (i, s) in MAILBOX.iter().enumerate() {
        let seq = s.seq.load(Ordering::Acquire);
        if SEEN[me][i].load(Ordering::Relaxed) == seq {
            continue;
        }
        let len = s.len.load(Ordering::Acquire).min(SLOT_VAS);
        for v in &s.vas[..len] {
            let va = VirtAddr::new(v.load(Ordering::Relaxed));
            // SAFETY: invlpg is a pure TLB invalidation of a canonical VA;
            // stale or future mappings are both safe to flush.
            unsafe { core::arch::asm!("invlpg [{}]", in(reg) va.as_u64(), options(nostack)) };
        }
        SEEN[me][i].store(seq, Ordering::Release);
    }
    crate::arch::apic::eoi();
}

/// Completed broadcast count (diagnostics + test assertions).
pub fn broadcast_count() -> u64 {
    BROADCASTS.load(Ordering::Relaxed)
}

/// One CPU's `seen` row (test/diagnostics view of the ack state).
pub fn seen_by(cpu_index: usize) -> [u64; SLOT_N] {
    let mut out = [0u64; SLOT_N];
    for (i, v) in out.iter_mut().enumerate() {
        *v = SEEN[cpu_index][i].load(Ordering::Relaxed);
    }
    out
}

/// Boot-time marker line (the machinery is lazy — say it exists).
pub fn init() {
    serial_println!(
        "[shootdown] vector {:#x} armed ({} slots x {} VAs)",
        SD_VECTOR,
        SLOT_N,
        SLOT_VAS
    );
}
