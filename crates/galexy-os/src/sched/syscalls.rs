//! Syscall dispatch table (scheduler-adjacent policy — see DESIGN.md
//! boundary rule 7): which syscall number does what. The MECHANISM (MSR
//! setup, naked entry, frame building) lives in `arch/`; the ABI in
//! `galexy-abi`; this file is the policy that binds numbers to behavior.
//!
//! Register/return contract (the arch shim guarantees this):
//! - args arrive in the frame: `a0 = frame.rdi`, `a1 = frame.rsi`,
//!   `a2 = frame.rdx`
//! - the result is stamped back into the frame: `RAX = value`,
//!   `RDX = 1 (ok) / 0 (err)` — the register form of
//!   `galexy_abi::SyscallResult`.
//!
//! Every capability-taking call validates the handle kernel-side.

use galexy_abi::{Cap, CapRights, SysError, Syscall, SyscallResult, MAX_SYSCALL};

use crate::sched::context::Context;

/// What the dispatcher wants done with the (already result-stamped) frame.
pub enum Outcome {
    /// Pop the frame and resume the calling task (result visible in RAX/RDX).
    Resume,
    /// Hand the CPU away now (yield: task stays schedulable). The arch shim
    /// passes the frame to the scheduler handoff.
    Handoff,
    /// Hand the CPU away with the task tombstoned (exit: never resumes).
    Exit,
}

/// Binds one syscall number → behavior + handoff decision.
pub fn service(frame: &mut Context, sysno: u64) -> Outcome {
    // Ring-3 callers only; `arch` validates "a user task is current" before
    // reaching this point (kernel-origin syscalls die loudly there).
    debug_assert_eq!(frame.cpl(), 3, "syscall service: not a ring-3 frame");
    if sysno > MAX_SYSCALL {
        stamp(frame, SyscallResult::err(SysError::Unsupported));
        return Outcome::Resume;
    }
    match sysno {
        n if n == Syscall::Exit as u64 => {
            // Exit code (a0) is informational this early in the OS; the
            // task is gone either way.
            Outcome::Exit
        }
        n if n == Syscall::Yield as u64 => {
            stamp(frame, SyscallResult::ok(0));
            Outcome::Handoff
        }
        n if n == Syscall::Write as u64 => {
            stamp(frame, syscall_write(Cap::from_bits(frame.rdi), frame.rsi, frame.rdx));
            Outcome::Resume
        }
        n if n == Syscall::CapInfo as u64 => {
            stamp(frame, syscall_cap_info(Cap::from_bits(frame.rdi)));
            Outcome::Resume
        }
        // The rest of the frozen ABI table: not wired at this step.
        _ => {
            stamp(frame, SyscallResult::err(SysError::Unsupported));
            Outcome::Resume
        }
    }
}

/// Writes the register-form result back into the frame (visible to user
/// code on resume — including after a yield round-trips the scheduler).
fn stamp(frame: &mut Context, result: SyscallResult) {
    frame.rax = result.value;
    frame.rdx = if result.ok { 1 } else { 0 };
}

fn syscall_cap_info(cap: Cap) -> SyscallResult {
    // TEMPLATE behavior: echoes the handle's bits back. Kernel-side cap
    // table + revocation semantics land with the resource work.
    SyscallResult::ok(cap.bits())
}

fn syscall_write(cap: Cap, _addr: u64, _len: u64) -> SyscallResult {
    // Cap authority exists (kernel-side checks); the buffer copy + screen
    // printing land with the first printing program (next commit).
    if cap.index() != galexy_abi::reserved::CONSOLE_INDEX {
        return SyscallResult::err(SysError::BadCap);
    }
    SyscallResult::err(SysError::Unsupported)
}

/// Rights a `write` call must see on the capability (kernel-side authority;
/// the opaque model means userspace never "sets" them).
pub const WRITE_RIGHTS: CapRights = CapRights::WRITE;
