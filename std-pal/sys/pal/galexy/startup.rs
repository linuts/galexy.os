//! Program entry and the two things only the entry can learn: the spawn
//! argument blob and which file slots the launcher wired as stdio.
//!
//! The loader enters `_start(rdi = argument address, rsi = length)` with
//! `rsp` on a fresh stack. The blob is NUL-separated argv without a
//! program name; there is no environment vector.

use super::abi::{self, CapRights};
use super::syscall;
use crate::cell::UnsafeCell;
use crate::sync::atomic::Ordering::{Acquire, Release};
use crate::sync::atomic::{Atomic, AtomicU8};

struct ArgBlob {
    buf: UnsafeCell<[u8; abi::SPAWN_ARG_MAX]>,
    len: UnsafeCell<usize>,
}

// SAFETY: written once by `_start` before `main`, read-only afterwards.
unsafe impl Sync for ArgBlob {}

static ARG: ArgBlob =
    ArgBlob { buf: UnsafeCell::new([0; abi::SPAWN_ARG_MAX]), len: UnsafeCell::new(0) };

/// Where stdout goes: a file slot index, or [`NO_SLOT`] for the console.
static STDOUT_SLOT: Atomic<u8> = AtomicU8::new(NO_SLOT);
/// Where stdin comes from: a file slot index, or [`NO_SLOT`] for the keyboard.
static STDIN_SLOT: Atomic<u8> = AtomicU8::new(NO_SLOT);

pub const NO_SLOT: u8 = 0xFF;

/// The raw argument blob the launcher passed (possibly empty).
pub fn arg_blob() -> &'static [u8] {
    // SAFETY: `_start` finished the copy before `main`; nothing writes later.
    unsafe {
        let n = *ARG.len.get();
        crate::slice::from_raw_parts(ARG.buf.get().cast::<u8>(), n)
    }
}

pub fn stdout_slot() -> u8 {
    STDOUT_SLOT.load(Acquire)
}

pub fn stdin_slot() -> u8 {
    STDIN_SLOT.load(Acquire)
}

/// Probes the per-task file table for Caps a pipeline spawn installed.
///
/// Slot 1 writable: middle stage (stdin in slot 0, stdout in slot 1).
/// Slot 0 writable: producer (stdout only). Slot 0 readable and no writable
/// slot: consumer (stdin only). An empty transfer never blocks and never
/// moves data, so the probe is free of side effects.
fn probe_stdio() {
    let probe = [0u8; 1];
    let writable =
        |slot: u64| syscall::write(syscall::file_cap(slot, CapRights::WRITE), &probe[..0]).is_ok();
    if writable(1) {
        STDOUT_SLOT.store(1, Release);
        STDIN_SLOT.store(0, Release);
        return;
    }
    if writable(0) {
        STDOUT_SLOT.store(0, Release);
        return;
    }
    let mut sink = [0u8; 1];
    if syscall::read(syscall::file_cap(0, CapRights::READ), &mut sink[..0]).is_ok() {
        STDIN_SLOT.store(0, Release);
    }
}

#[cfg(not(test))]
mod entry {
    unsafe extern "C" {
        /// The `main` rustc synthesizes around the user's `fn main` (it
        /// calls `std::rt::lang_start`).
        fn main(argc: i32, argv: *const *const u8) -> i32;
    }

    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn _start(arg: *const u8, len: usize) -> ! {
        // SAFETY: the loader mapped `arg` for `len` bytes above the initial
        // stack pointer (or passed zeros), and this runs before `main`.
        unsafe {
            let n = len.min(super::abi::SPAWN_ARG_MAX);
            if n > 0 && !arg.is_null() {
                core::ptr::copy_nonoverlapping(arg, super::ARG.buf.get().cast::<u8>(), n);
            }
            *super::ARG.len.get() = n;
        }
        super::probe_stdio();
        // argc/argv are unused: `env::args` reads the blob directly.
        let code = unsafe { main(0, core::ptr::null()) };
        super::syscall::exit(u64::from(code as u8))
    }
}
