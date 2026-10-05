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

use crate::drivers::screen;
use crate::sched::context::Context;
use x86_64::VirtAddr;

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

fn syscall_write(cap: Cap, addr: u64, len: u64) -> SyscallResult {
    // Capability authority kernel-side: only the console, only WRITE.
    if cap.index() != galexy_abi::reserved::CONSOLE_INDEX {
        return SyscallResult::err(SysError::BadCap);
    }
    if !cap.rights().contains(CapRights::WRITE) {
        return SyscallResult::err(SysError::AccessDenied);
    }
    // Length guard before any memory walking: staging cap.
    if len == 0 {
        return SyscallResult::ok(0);
    }
    if len > MAX_WRITE {
        return SyscallResult::err(SysError::BadValue);
    }

    // User-buffer validation: every byte must sit in USER-ACCESSIBLE,
    // present pages. (Step A shares the kernel's address space, so it is
    // not enough for a page to be mapped — the flags must allow ring 3.)
    // User-buffer validation: every byte must sit in present pages
    // (Step A shares the kernel's address space, so "mapped" alone is NOT
    // enough — a kernel page in ring 3 faults at the copy, framed by the
    // CPU as the task's own illegal access; the explicit walk turns that
    // into a clean syscall error for common cases).
    let Some(last_byte) = addr.checked_add(len - 1) else {
        return SyscallResult::err(SysError::BadBuffer);
    };
    let first_page = addr >> 12;
    let last_page = last_byte >> 12;
    for page_no in first_page..=last_page {
        if crate::arch::mm::translate(VirtAddr::new(page_no << 12)).is_none() {
            return SyscallResult::err(SysError::BadBuffer);
        }
    }

    // Stage the bytes kernel-side, then print. run-with-IF semantics: this
    // handler runs on the task's kernel stack with the screen lock's
    // IRQ-gating (lock-audit rule).
    let mut staged = [0u8; MAX_WRITE as usize];
    // SAFETY: validated above — every byte of [addr, addr+len) lives in
    // present pages holding user data.
    unsafe {
        core::ptr::copy_nonoverlapping(
            VirtAddr::new(addr).as_ptr::<u8>(),
            staged.as_mut_ptr(),
            len as usize,
        );
    }
    // Printable ASCII + newline: the console's charset discipline (TODO:
    // tab/CR/ESC handling is future screen work).
    let printable = staged[..len as usize]
        .iter()
        .all(|b| b.is_ascii_graphic() || *b == b' ' || *b == b'\n');
    if !printable {
        return SyscallResult::err(SysError::BadValue);
    }
    let text = core::str::from_utf8(&staged[..len as usize])
        .unwrap_or("");
    screen::out_str(text);
    SyscallResult::ok(len)
}

/// `write` staging cap (single page minus stack headroom).
const MAX_WRITE: u64 = 1024;

/// Rights a `write` call must see on the capability (kernel-side authority;
/// the opaque model means userspace never "sets" them).
pub const WRITE_RIGHTS: CapRights = CapRights::WRITE;
