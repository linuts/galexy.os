//! Boot demo activity: silent preemptive threads.
//!
//! The threads spin (preemption is real, timer-driven) but print nothing —
//! the "quiet OS" concept: their CPU time shows up as tick counts in the
//! status bar and the shell's `threads` command instead of scrolling noise.

use super::spawn_thread;

/// Spawns the silent demo thread pair.
pub fn spawn_all() {
    spawn_thread("thread-a", thread_a);
    spawn_thread("thread-b", thread_b);
}

extern "C" fn thread_a() {
    loop {
        core::hint::spin_loop();
    }
}

extern "C" fn thread_b() {
    loop {
        core::hint::spin_loop();
    }
}
