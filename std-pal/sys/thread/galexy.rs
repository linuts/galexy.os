//! Threads over `ThreadSpawn` / `ThreadExit`; join is `Wait` on the
//! process Cap the spawn returns.
//!
//! A thread shares the leader's address space, heap, files, and process
//! Caps. Its user stack comes from the kernel (`stack_pages` 4 KiB pages,
//! at most `THREAD_STACK_PAGES_MAX`); the kernel enters `thread_start(arg)`
//! and the thread ends itself with `ThreadExit`. When the leader exits,
//! the kernel ends every sibling.

use crate::io;
use crate::num::NonZero;
use crate::sys::pal::abi::{Cap, SLEEP_MS_MAX, SysError, THREAD_STACK_PAGES_MAX};
use crate::sys::pal::syscall;
use crate::thread::ThreadInit;
use crate::time::Duration;

pub struct Thread {
    join: Cap,
}

/// 64 KiB: `THREAD_STACK_PAGES_DEFAULT` pages. Milestone 71 lifts the cap.
pub const DEFAULT_MIN_STACK_SIZE: usize = 64 * 1024;

impl Thread {
    // unsafe: see thread::Builder::spawn_unchecked for safety requirements
    pub unsafe fn new(stack: usize, init: Box<ThreadInit>) -> io::Result<Thread> {
        // A request past the kernel maximum is clamped, not refused: the
        // stack budget is a Milestone 71 capacity item, and a program
        // asking for 8 MiB still runs on 256 KiB far more often than not.
        let pages = (stack.div_ceil(4096) as u64).clamp(1, THREAD_STACK_PAGES_MAX);
        let data = Box::into_raw(init);
        match syscall::thread_spawn(thread_start, data.expose_provenance() as u64, pages) {
            Ok(join) => Ok(Thread { join }),
            Err(e) => {
                // SAFETY: the kernel never saw `data`; reclaim it.
                drop(unsafe { Box::from_raw(data) });
                Err(syscall::io_error(e))
            }
        }
    }

    pub fn join(self) {
        loop {
            match syscall::wait(self.join) {
                Err(SysError::Interrupted) => continue,
                Ok(_) => return,
                Err(e) => rtabort!("failed to join thread: {e:?}"),
            }
        }
    }
}

#[inline(never)]
fn rust_main_thread_not_inlined(init: Box<ThreadInit>) {
    let rust_start = init.init();
    rust_start();
}

unsafe extern "C" fn thread_start(data: u64) -> ! {
    // SAFETY: recreating the box `Thread::new` leaked.
    let init = unsafe { Box::from_raw(crate::ptr::with_exposed_provenance_mut(data as usize)) };

    // Run the main thread with an inline(never) barrier to prevent
    // dealloc calls from being reordered to after the TLS has been destroyed.
    // See https://github.com/rust-lang/rust/pull/144465#pullrequestreview-3289729950
    // for more context.
    rust_main_thread_not_inlined(init);

    // Run `thread_local!` destructors and the runtime's thread cleanup.
    unsafe { crate::sys::thread_local::key::destroy_tls() };

    syscall::thread_exit(0)
}

pub fn available_parallelism() -> io::Result<NonZero<usize>> {
    // The kernel schedules threads across every CPU it brought up, but
    // does not report how many that is.
    Err(io::Error::UNKNOWN_THREAD_COUNT)
}

pub fn yield_now() {
    syscall::yield_now();
}

/// Sleeps in rounds of at most `SLEEP_MS_MAX`, rounding a sub-millisecond
/// request up to one millisecond so it still parks.
pub fn sleep(dur: Duration) {
    let mut left: u128 = dur.as_millis() + u128::from(dur.subsec_nanos() % 1_000_000 != 0);
    while left > 0 {
        let round = left.min(u128::from(SLEEP_MS_MAX)) as u64;
        syscall::sleep_ms(round);
        left -= u128::from(round);
    }
}
