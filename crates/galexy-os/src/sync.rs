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

/// Per-CPU depth of locks that must not be held across a disk wait.
///
/// The timer takes `THREADS`. A peer that already holds `THREADS` may be
/// waiting on a galfs lock. Enabling interrupts, or taking `THREADS`, while
/// this CPU still holds either one never returns. [`IoMutex`] is those locks.
const IO_CPUS: usize = crate::arch::cpu::MAX_CPUS;
static IO_DEPTH: [core::sync::atomic::AtomicUsize; IO_CPUS] =
    [const { core::sync::atomic::AtomicUsize::new(0) }; IO_CPUS];

fn cpu_slot() -> Option<usize> {
    match crate::arch::cpu::try_current_index() {
        Some(cpu) if cpu < IO_CPUS => Some(cpu),
        _ => None,
    }
}

pub(crate) fn io_depth_inc(cpu: usize) {
    if cpu < IO_CPUS {
        IO_DEPTH[cpu].fetch_add(1, core::sync::atomic::Ordering::Release);
    }
}

pub(crate) fn io_depth_dec(cpu: usize) {
    if cpu < IO_CPUS {
        IO_DEPTH[cpu].fetch_sub(1, core::sync::atomic::Ordering::Release);
    }
}

/// This CPU holds `THREADS` or a galfs lock the disk wait must have dropped.
pub(crate) fn io_lock_held() -> bool {
    cpu_slot().is_some_and(|cpu| IO_DEPTH[cpu].load(core::sync::atomic::Ordering::Acquire) > 0)
}

/// A mutex counted by [`io_lock_held`].
pub struct IoMutex<T> {
    inner: Mutex<T>,
}

impl<T> IoMutex<T> {
    pub const fn new(value: T) -> Self {
        Self {
            inner: Mutex::new(value),
        }
    }

    pub fn lock(&self) -> IoGuard<'_, T> {
        let inner = self.inner.lock();
        let cpu = cpu_slot();
        if let Some(cpu) = cpu {
            io_depth_inc(cpu);
        }
        IoGuard {
            inner: core::mem::ManuallyDrop::new(inner),
            cpu,
        }
    }
}

pub struct IoGuard<'a, T> {
    inner: core::mem::ManuallyDrop<MutexGuard<'a, T>>,
    cpu: Option<usize>,
}

impl<T> Drop for IoGuard<'_, T> {
    fn drop(&mut self) {
        // Unlock first. Decrementing before the unlock would let a disk wait
        // observe a zero depth while this CPU still held the mutex.
        unsafe { core::mem::ManuallyDrop::drop(&mut self.inner) };
        if let Some(cpu) = self.cpu {
            io_depth_dec(cpu);
        }
    }
}

impl<T> core::ops::Deref for IoGuard<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        &self.inner
    }
}

impl<T> core::ops::DerefMut for IoGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        &mut self.inner
    }
}
