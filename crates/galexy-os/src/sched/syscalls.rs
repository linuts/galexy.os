//! Syscall dispatch table (scheduler-adjacent policy — see DESIGN.md
//! boundary rule 7): which syscall number does what. The MECHANISM (MSR
//! setup, naked entry, frame building) lives in `arch/` and lands with
//! roadmap Step A.
//!
//! The table is empty at this milestone on purpose: `galexy-abi` froze the
//! numbers + capability layout ahead of any ring-3 code, so Step A only has
//! to wire the entry shim to this dispatch — no ABI decisions mid-ASM.

use galexy_abi::{Cap, SyscallResult, SysError};
use galexy_abi::{CapRights, Syscall};

/// Dispatches one syscall by its RAX number.
///
/// Args come in as `a0..=a2` (RDI, RSI, RDX — the arch shim's job is to
/// deliver them from user registers). Every capability-taking call
/// validates the handle kernel-side; `CapRights` checks fail as
/// `SysError::AccessDenied`, never as kernel panics.
pub fn dispatch(sysno: u64, a0: u64, a1: u64, a2: u64) -> SyscallResult {
    let _ = a1;
    let _ = a2;
    match sysno {
        n if n == Syscall::Exit as u64 => dispatch_exit(a0),
        n if n == Syscall::Yield as u64 => dispatch_yield(),
        n if n == Syscall::Write as u64 => dispatch_write(Cap::from_bits(a0), a1, a2),
        n if n == Syscall::CapInfo as u64 => dispatch_cap_info(Cap::from_bits(a0)),
        _ => SyscallResult::err(SysError::Unsupported),
    }
}

fn dispatch_exit(_exit_code: u64) -> SyscallResult {
    SyscallResult::err(SysError::Unsupported)
}

fn dispatch_yield() -> SyscallResult {
    SyscallResult::err(SysError::Unsupported)
}

fn dispatch_write(_cap: Cap, _addr: u64, _len: u64) -> SyscallResult {
    SyscallResult::err(SysError::Unsupported)
}

fn dispatch_cap_info(cap: Cap) -> SyscallResult {
    // TEMPLATE behavior until real kernel-side caps exist: echoes the
    // handle's bits back. Revocation semantics land with Step A.
    SyscallResult::ok(cap.bits())
}

/// Rights a `write` call must see on the capability (also ABI-facing when
/// capability tables land).
pub const WRITE_RIGHTS: CapRights = CapRights::WRITE;
