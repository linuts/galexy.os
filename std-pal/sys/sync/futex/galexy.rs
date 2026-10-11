//! Futex over `Syscall::Futex`. The kernel keys waiters by
//! `(address space, address)`, compares the 32-bit word under its
//! scheduler lock, and parks the caller; wake-ups and timeouts come back
//! as `ok(0)` / `TimedOut`. `std`'s generic `Mutex`, `Condvar`, `RwLock`,
//! `Once`, and `Parker` are built on these three functions.

use crate::sync::atomic::Atomic;
use crate::sys::pal::abi::{SLEEP_MS_MAX, SysError};
use crate::sys::pal::syscall;
use crate::time::Duration;

/// An atomic for use as a futex that is at least 32-bits but may be larger
pub type Futex = Atomic<Primitive>;
/// Must be the underlying type of Futex
pub type Primitive = u32;

/// An atomic for use as a futex that is at least 8-bits but may be larger.
pub type SmallFutex = Atomic<SmallPrimitive>;
/// Must be the underlying type of SmallFutex
pub type SmallPrimitive = u32;

/// Waits while `*futex == expected`. Returns `false` only on timeout.
///
/// The kernel caps one wait at `SLEEP_MS_MAX`; a longer timeout is split
/// into rounds, each re-checking the word. A sub-millisecond timeout
/// rounds up to one millisecond so it cannot become "no timeout".
pub fn futex_wait(futex: &Atomic<u32>, expected: u32, timeout: Option<Duration>) -> bool {
    let Some(timeout) = timeout else {
        loop {
            match syscall::futex_wait(futex, expected, 0) {
                Err(SysError::Interrupted) => continue,
                _ => return true,
            }
        }
    };
    // Round up to whole milliseconds.
    let mut left_ms: u128 =
        timeout.as_millis() + u128::from(timeout.subsec_nanos() % 1_000_000 != 0);
    if left_ms == 0 {
        // A zero timeout still has to observe the word once.
        return futex.load(crate::sync::atomic::Ordering::Relaxed) != expected;
    }
    while left_ms > 0 {
        let round = left_ms.min(u128::from(SLEEP_MS_MAX)) as u64;
        match syscall::futex_wait(futex, expected, round) {
            Ok(()) => return true,
            Err(SysError::TimedOut) => left_ms -= u128::from(round),
            Err(SysError::Interrupted) => {}
            Err(_) => return true,
        }
    }
    false
}

/// Wakes one waiter. Returns `true` when a thread was woken.
#[inline]
pub fn futex_wake(futex: &Atomic<u32>) -> bool {
    matches!(syscall::futex_wake(futex, 1), Ok(n) if n > 0)
}

#[inline]
pub fn futex_wake_all(futex: &Atomic<u32>) {
    let _ = syscall::futex_wake(futex, u64::MAX);
}
