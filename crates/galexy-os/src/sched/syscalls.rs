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
use x86_64::structures::paging::PageTableFlags;
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
            stamp(
                frame,
                syscall_write(Cap::from_bits(frame.rdi), frame.rsi, frame.rdx),
            );
            Outcome::Resume
        }
        n if n == Syscall::CapInfo as u64 => {
            stamp(frame, syscall_cap_info(Cap::from_bits(frame.rdi)));
            Outcome::Resume
        }
        n if n == Syscall::Open as u64 => {
            stamp(frame, syscall_open(frame.rdi, frame.rsi));
            Outcome::Resume
        }
        n if n == Syscall::Read as u64 => {
            stamp(
                frame,
                syscall_read(Cap::from_bits(frame.rdi), frame.rsi, frame.rdx),
            );
            Outcome::Resume
        }
        n if n == Syscall::Close as u64 => {
            stamp(frame, syscall_close(Cap::from_bits(frame.rdi)));
            Outcome::Resume
        }
        n if n == Syscall::Spawn as u64 => {
            let result = syscall_spawn(Cap::from_bits(frame.rdi), frame.rsi, frame.rdx);
            let park = result.ok;
            stamp(frame, result);
            if park {
                Outcome::Handoff
            } else {
                Outcome::Resume
            }
        }
        // Unknown numbers inside the table (none today) still answer.
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
    // Echoes the handle's bits. File caps have a real per-task table
    // (`open`/`read`/`close`); this call still does not consult it.
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

    // User-accessible pages only. A kernel address is present in the task
    // tree (shared kernel half) but lacks USER_ACCESSIBLE — copying it
    // would hand the task kernel bytes.
    if user_buffer(addr, len, false).is_err() {
        return SyscallResult::err(SysError::BadBuffer);
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
    // Printable ASCII, newline, backspace (0x08), and form feed (0x0c,
    // clear). Tab/CR/ESC stay future screen work.
    let printable = staged[..len as usize]
        .iter()
        .all(|b| b.is_ascii_graphic() || *b == b' ' || *b == b'\n' || *b == 0x08 || *b == 0x0c);
    if !printable {
        return SyscallResult::err(SysError::BadValue);
    }
    let text = core::str::from_utf8(&staged[..len as usize]).unwrap_or("");
    // Console policy (drivers::console): screen + serial — visible
    // interactively and observable headless.
    crate::drivers::console::out_str(text);
    SyscallResult::ok(len)
}

fn syscall_open(addr: u64, len: u64) -> SyscallResult {
    if len == 0 || len > MAX_NAME {
        return SyscallResult::err(SysError::BadValue);
    }
    if user_buffer(addr, len, false).is_err() {
        return SyscallResult::err(SysError::BadBuffer);
    }
    let mut raw = [0u8; MAX_NAME as usize];
    // SAFETY: `user_buffer` accepted every byte of [addr, addr+len).
    unsafe {
        core::ptr::copy_nonoverlapping(
            VirtAddr::new(addr).as_ptr::<u8>(),
            raw.as_mut_ptr(),
            len as usize,
        );
    }
    let name = core::str::from_utf8(&raw[..len as usize]).unwrap_or("");
    if !file_name_ok(name) {
        return SyscallResult::err(SysError::BadValue);
    }
    match crate::sched::task_open(name) {
        Ok(cap) => SyscallResult::ok(cap.bits()),
        Err(err) => SyscallResult::err(err),
    }
}

fn syscall_read(cap: Cap, addr: u64, len: u64) -> SyscallResult {
    if cap.index() == galexy_abi::reserved::KEYBOARD_INDEX {
        return syscall_read_keyboard(cap, addr, len);
    }
    // Short read: a request larger than the staging cap returns a prefix.
    let len = len.min(MAX_READ);
    if len > 0 && user_buffer(addr, len, true).is_err() {
        return SyscallResult::err(SysError::BadBuffer);
    }
    let mut staged = [0u8; MAX_READ as usize];
    let n = match crate::sched::task_read(cap, &mut staged[..len as usize]) {
        Ok(n) => n,
        Err(err) => return SyscallResult::err(err),
    };
    if n > 0 {
        // SAFETY: the destination was accepted as present, user, writable.
        unsafe {
            core::ptr::copy_nonoverlapping(
                staged.as_ptr(),
                VirtAddr::new(addr).as_mut_ptr::<u8>(),
                n,
            );
        }
    }
    SyscallResult::ok(n as u64)
}

fn syscall_read_keyboard(cap: Cap, addr: u64, len: u64) -> SyscallResult {
    if !cap.rights().contains(CapRights::READ) {
        return SyscallResult::err(SysError::AccessDenied);
    }
    let len = len.min(MAX_READ);
    if len == 0 {
        return SyscallResult::ok(0);
    }
    if user_buffer(addr, len, true).is_err() {
        return SyscallResult::err(SysError::BadBuffer);
    }
    let mut staged = [0u8; MAX_READ as usize];
    let mut filled = 0usize;
    while filled < len as usize {
        let Some(c) = crate::drivers::keyboard::pop_key() else {
            break;
        };
        let mut tmp = [0u8; 4];
        let encoded = c.encode_utf8(&mut tmp);
        if filled + encoded.len() > len as usize {
            crate::drivers::keyboard::unget_key(c);
            break;
        }
        staged[filled..filled + encoded.len()].copy_from_slice(encoded.as_bytes());
        filled += encoded.len();
    }
    if filled > 0 {
        // SAFETY: the destination was accepted as present, user, writable.
        unsafe {
            core::ptr::copy_nonoverlapping(
                staged.as_ptr(),
                VirtAddr::new(addr).as_mut_ptr::<u8>(),
                filled,
            );
        }
    }
    SyscallResult::ok(filled as u64)
}

fn syscall_spawn(cap: Cap, addr: u64, len: u64) -> SyscallResult {
    if cap.index() != galexy_abi::reserved::LOADER_INDEX {
        return SyscallResult::err(SysError::BadCap);
    }
    if !cap.rights().contains(CapRights::EXEC) {
        return SyscallResult::err(SysError::AccessDenied);
    }
    if len == 0 || len > MAX_NAME {
        return SyscallResult::err(SysError::BadValue);
    }
    if user_buffer(addr, len, false).is_err() {
        return SyscallResult::err(SysError::BadBuffer);
    }
    let mut raw = [0u8; MAX_NAME as usize];
    // SAFETY: `user_buffer` accepted every byte of [addr, addr+len).
    unsafe {
        core::ptr::copy_nonoverlapping(
            VirtAddr::new(addr).as_ptr::<u8>(),
            raw.as_mut_ptr(),
            len as usize,
        );
    }
    let name = core::str::from_utf8(&raw[..len as usize]).unwrap_or("");
    if !file_name_ok(name) {
        return SyscallResult::err(SysError::BadValue);
    }
    if crate::sched::ramdisk::find(name).is_none() {
        return SyscallResult::err(SysError::NotFound);
    }
    match crate::sched::task_spawn(name) {
        Ok(()) => SyscallResult::ok(0),
        Err(err) => SyscallResult::err(err),
    }
}

fn syscall_close(cap: Cap) -> SyscallResult {
    match crate::sched::task_close(cap) {
        Ok(()) => SyscallResult::ok(0),
        Err(err) => SyscallResult::err(err),
    }
}

/// Exact ramdisk names: `banner.txt`, `hello`. No directories, no spaces.
fn file_name_ok(name: &str) -> bool {
    !name.is_empty()
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'_' || b == b'-')
}

/// Every page of `[addr, addr+len)` is present and user-accessible in the
/// active tree. `writable` also requires the leaf to be writable, so `read`
/// cannot store into the task's code page (that would be a ring-0 fault).
fn user_buffer(addr: u64, len: u64, writable: bool) -> Result<(), SysError> {
    let Some(last_byte) = addr.checked_add(len - 1) else {
        return Err(SysError::BadBuffer);
    };
    let Ok(start) = VirtAddr::try_new(addr) else {
        return Err(SysError::BadBuffer);
    };
    let Ok(end) = VirtAddr::try_new(last_byte) else {
        return Err(SysError::BadBuffer);
    };
    let mut need = PageTableFlags::PRESENT | PageTableFlags::USER_ACCESSIBLE;
    if writable {
        need |= PageTableFlags::WRITABLE;
    }
    let first_page = start.as_u64() >> 12;
    let last_page = end.as_u64() >> 12;
    for page_no in first_page..=last_page {
        let page = VirtAddr::new(page_no << 12);
        match crate::arch::mm::active_leaf_flags(page) {
            Some(flags) if flags.contains(need) => {}
            _ => return Err(SysError::BadBuffer),
        }
    }
    Ok(())
}

/// `write` staging cap (single page minus stack headroom).
const MAX_WRITE: u64 = 1024;

/// `read` staging cap. Longer user requests short-read to this size.
const MAX_READ: u64 = 1024;

/// `open` name cap. Ramdisk entries are short (`hello`, `banner.txt`).
const MAX_NAME: u64 = 64;

/// Rights a `write` call must see on the capability (kernel-side authority;
/// the opaque model means userspace never "sets" them).
pub const WRITE_RIGHTS: CapRights = CapRights::WRITE;
