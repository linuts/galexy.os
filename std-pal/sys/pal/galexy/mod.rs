//! Galexy.OS platform layer (Milestone 70).
//!
//! `std` on `x86_64-unknown-galexy` talks to the kernel through exactly one
//! `syscall` instruction: [`syscall::raw_syscall`], an `#[inline(never)]
//! extern "C"` stub. Everything above it is plain Rust over the ABI in
//! `abi.rs` (generated from `crates/galexy-abi/src/lib.rs` by
//! `scripts/galexy-std-sysroot.py`).
//!
//! What the kernel offers maps onto `std` like this:
//!
//! | `std`                 | kernel                                        |
//! |-----------------------|-----------------------------------------------|
//! | `alloc::System`       | `Map` (dlmalloc over 4 KiB pages)             |
//! | `env::args`           | the spawn argument blob (NUL-separated)       |
//! | `io::stdout`/`stdin`  | console / keyboard, or the pipe spawn handed  |
//! | `time::Instant`       | `Clock` (monotonic milliseconds since boot)   |
//! | `thread::spawn`       | `ThreadSpawn` / `ThreadExit`; join = `Wait`   |
//! | `sync::*`             | `Futex` (std's generic futex primitives)      |
//! | `thread_local!`       | a per-thread table behind the FS base         |
//! | `fs::*`               | `Open`/`Create`/`Read`/`Write`/`Seek`/`Stat`… |
//! | `process::Command`    | `Spawn` + `Pipe` + `Wait`                     |
//!
//! There is no environment, no wall clock, no network, and no symlinks.

#![allow(missing_debug_implementations)]

pub mod abi;
pub mod startup;
pub mod syscall;

use crate::io;

// SAFETY: must be called only once during runtime initialization.
// `startup::_start` has already stashed the argument blob and probed the
// stdio slots before `main` runs; nothing else is needed here.
pub unsafe fn init(_argc: isize, _argv: *const *const u8, _sigpipe: u8) {}

// SAFETY: must be called only once during runtime cleanup.
pub unsafe fn cleanup() {}

pub fn unsupported<T>() -> io::Result<T> {
    Err(unsupported_err())
}

pub fn unsupported_err() -> io::Error {
    io::Error::UNSUPPORTED_PLATFORM
}

/// `process::abort` and the `panic = "abort"` tail. Exit code 134 mirrors
/// `SIGABRT` so a shell `$?` tells a panic from a clean exit.
pub fn abort_internal() -> ! {
    syscall::exit(134)
}

/// `process::exit`. The kernel truncates nothing; the low byte is what a
/// Unix-shaped caller expects to see in `$?`.
pub fn exit(code: i32) -> ! {
    syscall::exit(u64::from(code as u8))
}
