//! Kernel spin locks.
//!
//! Every kernel `Mutex` is a `spin` mutex whose spin loop services pending
//! TLB-shootdown requests ([`crate::arch::mm::shootdown::service_pending`]).
//!
//! Why: syscalls and IRQ-gated sections run IF=0, and any of them may
//! allocate (`String`, `Vec`, `format!`). An allocation that misses the
//! heap's fast path grows the heap, and growth broadcasts a shootdown IPI
//! that waits for every other CPU's ack. A CPU spinning IF=0 on a lock the
//! grower holds cannot take that IPI — both CPUs then spin forever with
//! interrupts off (galexy.os#86: `passwd` froze the machine with the last
//! serial line being the audit record). Polling the mailbox from the lock's
//! relax step makes the waiter ack without the IPI, so the holder's
//! broadcast completes, the holder releases, and the waiter proceeds.
//!
//! The lock discipline in `docs/DESIGN.md` (IF=0 while held, THREADS before
//! CHANS, no broadcast under a lock) still stands; this is the safety net
//! for the one path (`GlobalAlloc`) that cannot see which locks its caller
//! holds.

use spin::RelaxStrategy;

/// Relax step for kernel spin loops: ack any pending shootdown, then pause.
pub struct ServiceShootdowns;

impl RelaxStrategy for ServiceShootdowns {
    #[inline]
    fn relax() {
        crate::arch::mm::shootdown::service_pending();
        core::hint::spin_loop();
    }
}

/// The kernel's spin mutex. Same API as `spin::Mutex`.
pub type Mutex<T> = spin::mutex::Mutex<T, ServiceShootdowns>;

/// Guard for [`Mutex`].
pub type MutexGuard<'a, T> = spin::mutex::MutexGuard<'a, T, ServiceShootdowns>;
