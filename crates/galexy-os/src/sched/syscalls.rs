//! Syscall dispatch table (scheduler-adjacent policy — see DESIGN.md
//! boundary rule 7): which syscall number does what. The MECHANISM (MSR
//! setup, naked entry, frame building) lives in `arch/`; the ABI in
//! `galexy-abi`; this file is the policy that binds numbers to behavior.
//!
//! Register/return contract (the arch shim guarantees this):
//! - args arrive in the frame: `a0 = frame.rdi`, `a1 = frame.rsi`,
//!   `a2 = frame.rdx`. `spawn` also reads `r8`/`r9` (argument) and `r10`
//!   (grant bits). `grant` reads `r8`/`r9` (target task name).
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
        n if n == Syscall::Write as u64 => match syscall_write_ex(
            Cap::from_bits(frame.rdi),
            frame.rsi,
            frame.rdx,
        ) {
            IoResult::Done(r) => {
                stamp(frame, r);
                Outcome::Resume
            }
            IoResult::Park => {
                stamp(frame, SyscallResult::ok(0));
                Outcome::Handoff
            }
        },
        n if n == Syscall::CapInfo as u64 => {
            stamp(frame, syscall_cap_info(Cap::from_bits(frame.rdi)));
            Outcome::Resume
        }
        n if n == Syscall::Open as u64 => {
            stamp(frame, syscall_open(frame.rdi, frame.rsi));
            Outcome::Resume
        }
        n if n == Syscall::Read as u64 => match syscall_read_ex(
            Cap::from_bits(frame.rdi),
            frame.rsi,
            frame.rdx,
        ) {
            IoResult::Done(r) => {
                stamp(frame, r);
                Outcome::Resume
            }
            IoResult::Park => {
                stamp(frame, SyscallResult::ok(0));
                Outcome::Handoff
            }
        },
        n if n == Syscall::Close as u64 => {
            stamp(frame, syscall_close(Cap::from_bits(frame.rdi)));
            Outcome::Resume
        }
        n if n == Syscall::Spawn as u64 => {
            let result = syscall_spawn(frame);
            let park = result.ok;
            stamp(frame, result);
            if park {
                Outcome::Handoff
            } else {
                Outcome::Resume
            }
        }
        n if n == Syscall::Power as u64 => {
            stamp(frame, syscall_power(Cap::from_bits(frame.rdi), frame.rsi));
            Outcome::Resume
        }
        n if n == Syscall::Create as u64 => {
            stamp(frame, syscall_create(frame.rdi, frame.rsi, frame.rdx));
            Outcome::Resume
        }
        n if n == Syscall::Remove as u64 => {
            stamp(frame, syscall_remove(frame.rdi, frame.rsi));
            Outcome::Resume
        }
        n if n == Syscall::Grant as u64 => {
            stamp(frame, syscall_grant(frame));
            Outcome::Resume
        }
        n if n == Syscall::Revoke as u64 => {
            stamp(frame, syscall_revoke(frame));
            Outcome::Resume
        }
        n if n == Syscall::Pipe as u64 => {
            stamp(frame, syscall_pipe(frame.rdi));
            Outcome::Resume
        }
        n if n == Syscall::Give as u64 => {
            stamp(
                frame,
                syscall_give(Cap::from_bits(frame.rdi), frame.rsi, frame.rdx),
            );
            Outcome::Resume
        }
        n if n == Syscall::Seek as u64 => {
            stamp(
                frame,
                syscall_seek(Cap::from_bits(frame.rdi), frame.rsi, frame.rdx),
            );
            Outcome::Resume
        }
        n if n == Syscall::User as u64 => {
            stamp(frame, syscall_user(frame));
            Outcome::Resume
        }
        n if n == Syscall::Rename as u64 => {
            stamp(
                frame,
                syscall_rename(frame.rdi, frame.rsi, frame.rdx, frame.r8),
            );
            Outcome::Resume
        }
        n if n == Syscall::Truncate as u64 => {
            stamp(
                frame,
                syscall_truncate(Cap::from_bits(frame.rdi), frame.rsi),
            );
            Outcome::Resume
        }
        n if n == Syscall::Stat as u64 => {
            stamp(
                frame,
                syscall_stat(frame.rdi, frame.rsi, frame.rdx, frame.r8),
            );
            Outcome::Resume
        }
        n if n == Syscall::Sync as u64 => {
            stamp(frame, syscall_sync());
            Outcome::Resume
        }
        n if n == Syscall::Share as u64 => {
            stamp(frame, syscall_share(frame));
            Outcome::Resume
        }
        n if n == Syscall::Unshare as u64 => {
            stamp(frame, syscall_unshare(frame));
            Outcome::Resume
        }
        n if n == Syscall::Wait as u64 => match crate::sched::task_wait(Cap::from_bits(frame.rdi))
        {
            Ok(None) => {
                // Parked; exit code is stamped when the child exits.
                stamp(frame, SyscallResult::ok(0));
                Outcome::Handoff
            }
            Ok(Some(code)) => {
                stamp(frame, SyscallResult::ok(code));
                Outcome::Resume
            }
            Err(err) => {
                stamp(frame, SyscallResult::err(err));
                Outcome::Resume
            }
        },
        n if n == Syscall::Kill as u64 => {
            stamp(frame, syscall_kill(Cap::from_bits(frame.rdi)));
            Outcome::Resume
        }
        n if n == Syscall::Sleep as u64 => match crate::sched::task_sleep(frame.rdi) {
            Ok(()) => {
                stamp(frame, SyscallResult::ok(0));
                Outcome::Handoff
            }
            Err(err) => {
                stamp(frame, SyscallResult::err(err));
                Outcome::Resume
            }
        },
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

enum IoResult {
    Done(SyscallResult),
    Park,
}

fn syscall_write_ex(cap: Cap, addr: u64, len: u64) -> IoResult {
    if cap.index() != galexy_abi::reserved::CONSOLE_INDEX {
        return syscall_write_file_ex(cap, addr, len);
    }
    IoResult::Done(syscall_write_console(cap, addr, len))
}

fn syscall_write_console(cap: Cap, addr: u64, len: u64) -> SyscallResult {
    if !crate::sched::task_granted(crate::sched::Grant::Console) {
        return SyscallResult::err(SysError::AccessDenied);
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
    // Printable ASCII, newline, backspace (0x08), form feed (0x0c),
    // tab (0x09), CR (0x0d), ESC (0x1b, for CSI), and BEL (0x07 → beep).
    // Other controls stay BadValue.
    let printable = staged[..len as usize].iter().all(|b| {
        b.is_ascii_graphic()
            || *b == b' '
            || *b == b'\n'
            || *b == 0x07
            || *b == 0x08
            || *b == 0x09
            || *b == 0x0c
            || *b == 0x0d
            || *b == 0x1b
    });
    if !printable {
        return SyscallResult::err(SysError::BadValue);
    }
    let allowed = crate::sched::console_take_budget(len as usize);
    if allowed == 0 {
        return SyscallResult::ok(0);
    }
    let text = core::str::from_utf8(&staged[..allowed]).unwrap_or("");
    // The task's own console. COM1 mirrors it only while that TTY is visible.
    crate::drivers::console::out_str_tty(crate::sched::current_tty(), text);
    SyscallResult::ok(allowed as u64)
}

/// Copies `src` onto a file/pipe cap. Pipes may park when full (M57).
fn syscall_write_file_ex(cap: Cap, addr: u64, len: u64) -> IoResult {
    if len > MAX_WRITE {
        return IoResult::Done(SyscallResult::err(SysError::BadValue));
    }
    if len > 0 && user_buffer(addr, len, false).is_err() {
        return IoResult::Done(SyscallResult::err(SysError::BadBuffer));
    }
    let mut staged = [0u8; MAX_WRITE as usize];
    if len > 0 {
        // SAFETY: `user_buffer` accepted every byte of [addr, addr+len).
        unsafe {
            core::ptr::copy_nonoverlapping(
                VirtAddr::new(addr).as_ptr::<u8>(),
                staged.as_mut_ptr(),
                len as usize,
            );
        }
    }
    match crate::sched::task_write_ex(cap, &staged[..len as usize]) {
        Ok(crate::sched::IoOp::Ready(n)) => IoResult::Done(SyscallResult::ok(n as u64)),
        Ok(crate::sched::IoOp::ParkPipe { id, read: false }) => {
            match crate::sched::task_park_pipe(id, false, cap.bits(), addr, len as u32) {
                Ok(()) => IoResult::Park,
                Err(err) => IoResult::Done(SyscallResult::err(err)),
            }
        }
        Ok(crate::sched::IoOp::ParkPipe { .. }) => {
            IoResult::Done(SyscallResult::err(SysError::Unsupported))
        }
        Err(err) => IoResult::Done(SyscallResult::err(err)),
    }
}

fn syscall_create(addr: u64, len: u64, flags: u64) -> SyscallResult {
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
    if !path_ok(name) {
        return SyscallResult::err(SysError::BadValue);
    }
    // Only flag 1 is defined (replace an existing scratch file). Any other
    // value, including whatever an older caller left in RDX, creates only
    // when the name is new.
    match crate::sched::task_create(name, flags == 1) {
        Ok(cap) => SyscallResult::ok(cap.bits()),
        Err(err) => SyscallResult::err(err),
    }
}

fn syscall_remove(addr: u64, len: u64) -> SyscallResult {
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
    if !path_ok(name) {
        return SyscallResult::err(SysError::BadValue);
    }
    match crate::sched::task_remove(name) {
        Ok(()) => SyscallResult::ok(0),
        Err(err) => SyscallResult::err(err),
    }
}

fn copy_user_path(addr: u64, len: u64) -> Result<[u8; MAX_NAME as usize], SysError> {
    if len == 0 || len > MAX_NAME {
        return Err(SysError::BadValue);
    }
    if user_buffer(addr, len, false).is_err() {
        return Err(SysError::BadBuffer);
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
    Ok(raw)
}

fn syscall_rename(old_addr: u64, old_len: u64, new_addr: u64, new_len: u64) -> SyscallResult {
    let old_raw = match copy_user_path(old_addr, old_len) {
        Ok(r) => r,
        Err(e) => return SyscallResult::err(e),
    };
    let new_raw = match copy_user_path(new_addr, new_len) {
        Ok(r) => r,
        Err(e) => return SyscallResult::err(e),
    };
    let old = core::str::from_utf8(&old_raw[..old_len as usize]).unwrap_or("");
    let new = core::str::from_utf8(&new_raw[..new_len as usize]).unwrap_or("");
    if !path_ok(old) || !path_ok(new) {
        return SyscallResult::err(SysError::BadValue);
    }
    match crate::sched::task_rename(old, new) {
        Ok(()) => SyscallResult::ok(0),
        Err(err) => SyscallResult::err(err),
    }
}

fn syscall_truncate(cap: Cap, size: u64) -> SyscallResult {
    match crate::sched::task_truncate(cap, size) {
        Ok(()) => SyscallResult::ok(0),
        Err(err) => SyscallResult::err(err),
    }
}

fn syscall_stat(path_addr: u64, path_len: u64, buf_addr: u64, buf_len: u64) -> SyscallResult {
    let raw = match copy_user_path(path_addr, path_len) {
        Ok(r) => r,
        Err(e) => return SyscallResult::err(e),
    };
    let name = core::str::from_utf8(&raw[..path_len as usize]).unwrap_or("");
    if !path_ok(name) {
        return SyscallResult::err(SysError::BadValue);
    }
    if buf_len < galexy_abi::STAT_LEN as u64 {
        return SyscallResult::err(SysError::BadBuffer);
    }
    if user_buffer(buf_addr, galexy_abi::STAT_LEN as u64, true).is_err() {
        return SyscallResult::err(SysError::BadBuffer);
    }
    let mut staged = [0u8; galexy_abi::STAT_LEN];
    match crate::sched::task_stat(name, &mut staged) {
        Ok(n) => {
            // SAFETY: `user_buffer` accepted the destination.
            unsafe {
                core::ptr::copy_nonoverlapping(
                    staged.as_ptr(),
                    VirtAddr::new(buf_addr).as_mut_ptr::<u8>(),
                    n,
                );
            }
            SyscallResult::ok(n as u64)
        }
        Err(err) => SyscallResult::err(err),
    }
}

fn syscall_sync() -> SyscallResult {
    match crate::sched::task_sync() {
        Ok(()) => SyscallResult::ok(0),
        Err(err) => SyscallResult::err(err),
    }
}

fn syscall_grant(frame: &Context) -> SyscallResult {
    let path_addr = frame.rdi;
    let path_len = frame.rsi;
    let rights = frame.rdx;
    let task_addr = frame.r8;
    let task_len = frame.r9;
    if path_len == 0 || path_len > MAX_NAME {
        return SyscallResult::err(SysError::BadValue);
    }
    if task_len == 0 || task_len > MAX_NAME {
        return SyscallResult::err(SysError::BadValue);
    }
    if rights == 0 || rights & !galexy_abi::TOKEN_ALL != 0 {
        return SyscallResult::err(SysError::BadValue);
    }
    if user_buffer(path_addr, path_len, false).is_err() {
        return SyscallResult::err(SysError::BadBuffer);
    }
    if user_buffer(task_addr, task_len, false).is_err() {
        return SyscallResult::err(SysError::BadBuffer);
    }
    let mut path_raw = [0u8; MAX_NAME as usize];
    let mut task_raw = [0u8; MAX_NAME as usize];
    // SAFETY: `user_buffer` accepted every byte of both ranges.
    unsafe {
        core::ptr::copy_nonoverlapping(
            VirtAddr::new(path_addr).as_ptr::<u8>(),
            path_raw.as_mut_ptr(),
            path_len as usize,
        );
        core::ptr::copy_nonoverlapping(
            VirtAddr::new(task_addr).as_ptr::<u8>(),
            task_raw.as_mut_ptr(),
            task_len as usize,
        );
    }
    let path = core::str::from_utf8(&path_raw[..path_len as usize]).unwrap_or("");
    let task = core::str::from_utf8(&task_raw[..task_len as usize]).unwrap_or("");
    if !path_ok(path) || !file_name_ok(task) {
        return SyscallResult::err(SysError::BadValue);
    }
    match crate::sched::task_grant(path, rights as u8, task) {
        Ok(()) => SyscallResult::ok(0),
        Err(err) => SyscallResult::err(err),
    }
}

fn syscall_revoke(frame: &Context) -> SyscallResult {
    let path_addr = frame.rdi;
    let path_len = frame.rsi;
    let rights = frame.rdx;
    let task_addr = frame.r8;
    let task_len = frame.r9;
    if path_len == 0 || path_len > MAX_NAME {
        return SyscallResult::err(SysError::BadValue);
    }
    if task_len == 0 || task_len > MAX_NAME {
        return SyscallResult::err(SysError::BadValue);
    }
    if rights == 0 || rights & !galexy_abi::TOKEN_ALL != 0 {
        return SyscallResult::err(SysError::BadValue);
    }
    if user_buffer(path_addr, path_len, false).is_err() {
        return SyscallResult::err(SysError::BadBuffer);
    }
    if user_buffer(task_addr, task_len, false).is_err() {
        return SyscallResult::err(SysError::BadBuffer);
    }
    let mut path_raw = [0u8; MAX_NAME as usize];
    let mut task_raw = [0u8; MAX_NAME as usize];
    // SAFETY: `user_buffer` accepted every byte of both ranges.
    unsafe {
        core::ptr::copy_nonoverlapping(
            VirtAddr::new(path_addr).as_ptr::<u8>(),
            path_raw.as_mut_ptr(),
            path_len as usize,
        );
        core::ptr::copy_nonoverlapping(
            VirtAddr::new(task_addr).as_ptr::<u8>(),
            task_raw.as_mut_ptr(),
            task_len as usize,
        );
    }
    let path = core::str::from_utf8(&path_raw[..path_len as usize]).unwrap_or("");
    let task = core::str::from_utf8(&task_raw[..task_len as usize]).unwrap_or("");
    if !path_ok(path) || !file_name_ok(task) {
        return SyscallResult::err(SysError::BadValue);
    }
    match crate::sched::task_revoke(path, rights as u8, task) {
        Ok(()) => SyscallResult::ok(0),
        Err(err) => SyscallResult::err(err),
    }
}

fn syscall_share(frame: &Context) -> SyscallResult {
    syscall_share_op(frame, true)
}

fn syscall_unshare(frame: &Context) -> SyscallResult {
    syscall_share_op(frame, false)
}

fn syscall_share_op(frame: &Context, add: bool) -> SyscallResult {
    let path_addr = frame.rdi;
    let path_len = frame.rsi;
    let rights = frame.rdx;
    let user_addr = frame.r8;
    let user_len = frame.r9;
    if path_len == 0 || path_len > MAX_NAME {
        return SyscallResult::err(SysError::BadValue);
    }
    if user_len == 0 || user_len > MAX_NAME {
        return SyscallResult::err(SysError::BadValue);
    }
    if rights == 0 || rights & !galexy_abi::TOKEN_ALL != 0 {
        return SyscallResult::err(SysError::BadValue);
    }
    if user_buffer(path_addr, path_len, false).is_err() {
        return SyscallResult::err(SysError::BadBuffer);
    }
    if user_buffer(user_addr, user_len, false).is_err() {
        return SyscallResult::err(SysError::BadBuffer);
    }
    let mut path_raw = [0u8; MAX_NAME as usize];
    let mut user_raw = [0u8; MAX_NAME as usize];
    // SAFETY: `user_buffer` accepted every byte of both ranges.
    unsafe {
        core::ptr::copy_nonoverlapping(
            VirtAddr::new(path_addr).as_ptr::<u8>(),
            path_raw.as_mut_ptr(),
            path_len as usize,
        );
        core::ptr::copy_nonoverlapping(
            VirtAddr::new(user_addr).as_ptr::<u8>(),
            user_raw.as_mut_ptr(),
            user_len as usize,
        );
    }
    let path = core::str::from_utf8(&path_raw[..path_len as usize]).unwrap_or("");
    let user = core::str::from_utf8(&user_raw[..user_len as usize]).unwrap_or("");
    if !path_ok(path) || !file_name_ok(user) {
        return SyscallResult::err(SysError::BadValue);
    }
    let result = if add {
        crate::sched::task_share(path, rights as u8, user)
    } else {
        crate::sched::task_unshare(path, rights as u8, user)
    };
    match result {
        Ok(()) => SyscallResult::ok(0),
        Err(err) => SyscallResult::err(err),
    }
}

fn syscall_pipe(addr: u64) -> SyscallResult {
    if user_buffer(addr, 16, true).is_err() {
        return SyscallResult::err(SysError::BadBuffer);
    }
    match crate::sched::task_pipe() {
        Ok((read_cap, write_cap)) => {
            let bits = [read_cap.bits(), write_cap.bits()];
            // SAFETY: `user_buffer` accepted 16 writable user bytes.
            unsafe {
                core::ptr::copy_nonoverlapping(
                    bits.as_ptr() as *const u8,
                    VirtAddr::new(addr).as_mut_ptr::<u8>(),
                    16,
                );
            }
            SyscallResult::ok(0)
        }
        Err(err) => SyscallResult::err(err),
    }
}

fn syscall_give(cap: Cap, addr: u64, len: u64) -> SyscallResult {
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
    match crate::sched::task_give(cap, name) {
        Ok(new_cap) => SyscallResult::ok(new_cap.bits()),
        Err(err) => SyscallResult::err(err),
    }
}

fn syscall_seek(cap: Cap, offset_bits: u64, whence: u64) -> SyscallResult {
    let offset = offset_bits as i64;
    match crate::sched::task_seek(cap, offset, whence) {
        Ok(pos) => SyscallResult::ok(pos),
        Err(err) => SyscallResult::err(err),
    }
}

fn syscall_user(frame: &Context) -> SyscallResult {
    let addr = frame.rdi;
    let len = frame.rsi;
    let op = frame.rdx;
    match op {
        galexy_abi::USER_WHOAMI
        | galexy_abi::USER_USERS
        | galexy_abi::USER_TOKENS
        | galexy_abi::USER_QUOTA => {
            if len == 0 || len > MAX_READ {
                return SyscallResult::err(SysError::BadValue);
            }
            if user_buffer(addr, len, true).is_err() {
                return SyscallResult::err(SysError::BadBuffer);
            }
            let mut staged = [0u8; MAX_READ as usize];
            let result = if op == galexy_abi::USER_WHOAMI {
                crate::sched::task_whoami(&mut staged[..len as usize])
            } else if op == galexy_abi::USER_USERS {
                crate::sched::task_users(&mut staged[..len as usize])
            } else if op == galexy_abi::USER_TOKENS {
                crate::sched::task_tokens(&mut staged[..len as usize])
            } else {
                let mut name_raw = [0u8; MAX_NAME as usize];
                let name = if frame.r9 == 0 {
                    None
                } else {
                    let Some(n) = copy_user_str(frame.r8, frame.r9, &mut name_raw, true) else {
                        return SyscallResult::err(SysError::BadValue);
                    };
                    Some(core::str::from_utf8(&name_raw[..n]).unwrap_or(""))
                };
                crate::sched::task_quota(&mut staged[..len as usize], name)
            };
            match result {
                Ok(n) => {
                    // SAFETY: buffer accepted as writable user memory.
                    unsafe {
                        core::ptr::copy_nonoverlapping(
                            staged.as_ptr(),
                            VirtAddr::new(addr).as_mut_ptr::<u8>(),
                            n,
                        );
                    }
                    SyscallResult::ok(n as u64)
                }
                Err(err) => SyscallResult::err(err),
            }
        }
        galexy_abi::USER_SETQUOTA => {
            let mut raw = [0u8; MAX_NAME as usize];
            let Some(n) = copy_user_str(addr, len, &mut raw, true) else {
                return SyscallResult::err(SysError::BadValue);
            };
            let name = core::str::from_utf8(&raw[..n]).unwrap_or("");
            if frame.r8 > u16::MAX as u64 || frame.r9 > u32::MAX as u64 {
                return SyscallResult::err(SysError::BadValue);
            }
            match crate::sched::task_setquota(name, frame.r8 as u16, frame.r9 as u32) {
                Ok(()) => SyscallResult::ok(0),
                Err(err) => SyscallResult::err(err),
            }
        }
        galexy_abi::USER_LOGOUT => match crate::sched::task_logout() {
            Ok(()) => SyscallResult::ok(0),
            Err(err) => SyscallResult::err(err),
        },
        galexy_abi::USER_DEL | galexy_abi::USER_SU => {
            let mut raw = [0u8; MAX_NAME as usize];
            let Some(n) = copy_user_str(addr, len, &mut raw, true) else {
                return SyscallResult::err(SysError::BadValue);
            };
            let name = core::str::from_utf8(&raw[..n]).unwrap_or("");
            let result = if op == galexy_abi::USER_DEL {
                crate::sched::task_userdel(name)
            } else {
                crate::sched::task_su(name)
            };
            match result {
                Ok(()) => SyscallResult::ok(0),
                Err(err) => SyscallResult::err(err),
            }
        }
        galexy_abi::USER_ADD | galexy_abi::USER_LOGIN => {
            let mut raw = [0u8; MAX_NAME as usize];
            let Some(n) = copy_user_str(addr, len, &mut raw, true) else {
                return SyscallResult::err(SysError::BadValue);
            };
            let mut pass = [0u8; 64];
            let Some(p) = copy_user_str(frame.r8, frame.r9, &mut pass, false) else {
                galexy_crypto::wipe_bytes(&mut pass);
                return SyscallResult::err(SysError::BadValue);
            };
            let name = core::str::from_utf8(&raw[..n]).unwrap_or("");
            let password = &pass[..p];
            let result = if op == galexy_abi::USER_ADD {
                crate::sched::task_useradd(name, password)
            } else {
                crate::sched::task_login(name, password)
            };
            galexy_crypto::wipe_bytes(&mut pass);
            match result {
                Ok(()) => SyscallResult::ok(0),
                Err(err) => SyscallResult::err(err),
            }
        }
        galexy_abi::USER_PASSWD => {
            let mut raw = [0u8; MAX_NAME as usize];
            let name = if len == 0 {
                None
            } else {
                let Some(n) = copy_user_str(addr, len, &mut raw, true) else {
                    return SyscallResult::err(SysError::BadValue);
                };
                Some(core::str::from_utf8(&raw[..n]).unwrap_or(""))
            };
            let mut pass = [0u8; 64];
            let Some(p) = copy_user_str(frame.r8, frame.r9, &mut pass, false) else {
                galexy_crypto::wipe_bytes(&mut pass);
                return SyscallResult::err(SysError::BadValue);
            };
            let result = crate::sched::task_passwd(name, &pass[..p]);
            galexy_crypto::wipe_bytes(&mut pass);
            match result {
                Ok(()) => SyscallResult::ok(0),
                Err(err) => SyscallResult::err(err),
            }
        }
        _ => SyscallResult::err(SysError::BadValue),
    }
}

/// Copies a user string into `out`. When `named`, applies [`file_name_ok`].
fn copy_user_str(addr: u64, len: u64, out: &mut [u8], named: bool) -> Option<usize> {
    if len == 0 || len as usize > out.len() {
        return None;
    }
    if user_buffer(addr, len, false).is_err() {
        return None;
    }
    // SAFETY: `user_buffer` accepted every byte.
    unsafe {
        core::ptr::copy_nonoverlapping(
            VirtAddr::new(addr).as_ptr::<u8>(),
            out.as_mut_ptr(),
            len as usize,
        );
    }
    let bytes = &out[..len as usize];
    if named {
        let name = core::str::from_utf8(bytes).ok()?;
        if !file_name_ok(name) {
            return None;
        }
    } else if !bytes
        .iter()
        .all(|b| b.is_ascii_graphic() || *b == b' ')
    {
        return None;
    }
    Some(len as usize)
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
    if !path_ok(name) {
        return SyscallResult::err(SysError::BadValue);
    }
    match crate::sched::task_open(name) {
        Ok(cap) => SyscallResult::ok(cap.bits()),
        Err(err) => SyscallResult::err(err),
    }
}

fn syscall_read_ex(cap: Cap, addr: u64, len: u64) -> IoResult {
    if cap.index() == galexy_abi::reserved::KEYBOARD_INDEX {
        return syscall_read_keyboard_ex(cap, addr, len);
    }
    if let Some(kind) = query_kind(cap.index()) {
        return IoResult::Done(syscall_read_query(cap, kind, addr, len));
    }
    if cap.index() == galexy_abi::reserved::SELF_INDEX {
        return IoResult::Done(syscall_read_self(cap, addr, len));
    }
    if (galexy_abi::PROC_CAP_BASE
        ..galexy_abi::PROC_CAP_BASE + galexy_abi::MAX_PROC_CAPS)
        .contains(&cap.index())
    {
        return IoResult::Done(syscall_read_proc(cap, addr, len));
    }
    // Short read: a request larger than the staging cap returns a prefix.
    let len = len.min(MAX_READ);
    if len > 0 && user_buffer(addr, len, true).is_err() {
        return IoResult::Done(SyscallResult::err(SysError::BadBuffer));
    }
    let mut staged = [0u8; MAX_READ as usize];
    match crate::sched::task_read_ex(cap, &mut staged[..len as usize]) {
        Ok(crate::sched::IoOp::Ready(n)) => {
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
            IoResult::Done(SyscallResult::ok(n as u64))
        }
        Ok(crate::sched::IoOp::ParkPipe { id, read: true }) => {
            match crate::sched::task_park_pipe(id, true, cap.bits(), addr, len as u32) {
                Ok(()) => IoResult::Park,
                Err(err) => IoResult::Done(SyscallResult::err(err)),
            }
        }
        Ok(crate::sched::IoOp::ParkPipe { .. }) => {
            IoResult::Done(SyscallResult::err(SysError::Unsupported))
        }
        Err(err) => IoResult::Done(SyscallResult::err(err)),
    }
}

fn syscall_read_self(cap: Cap, addr: u64, len: u64) -> SyscallResult {
    if !cap.rights().contains(CapRights::PROC_INSPECT) {
        return SyscallResult::err(SysError::AccessDenied);
    }
    copy_inspect(addr, len, |dst| crate::sched::task_self_inspect(dst))
}

fn syscall_read_proc(cap: Cap, addr: u64, len: u64) -> SyscallResult {
    copy_inspect(addr, len, |dst| crate::sched::task_proc_inspect(cap, dst))
}

fn copy_inspect(
    addr: u64,
    len: u64,
    fill: impl FnOnce(&mut [u8]) -> Result<usize, SysError>,
) -> SyscallResult {
    let len = len.min(MAX_READ);
    if len == 0 {
        return SyscallResult::ok(0);
    }
    if user_buffer(addr, len, true).is_err() {
        return SyscallResult::err(SysError::BadBuffer);
    }
    let mut staged = [0u8; MAX_READ as usize];
    let n = match fill(&mut staged[..len as usize]) {
        Ok(n) => n,
        Err(err) => return SyscallResult::err(err),
    };
    if n > 0 {
        // SAFETY: destination accepted as present, user, writable.
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

/// Which query snapshot a high-band index names, if it names one.
fn query_kind(index: u64) -> Option<Query> {
    match index {
        galexy_abi::reserved::STATS_INDEX => Some(Query::Stats),
        galexy_abi::reserved::TASKS_INDEX => Some(Query::Tasks),
        galexy_abi::reserved::THREADS_INDEX => Some(Query::Threads),
        galexy_abi::reserved::FILES_INDEX => Some(Query::Files),
        _ => None,
    }
}

enum Query {
    Stats,
    Tasks,
    Threads,
    Files,
}

fn syscall_read_query(cap: Cap, kind: Query, addr: u64, len: u64) -> SyscallResult {
    if !crate::sched::task_granted(crate::sched::Grant::Query) {
        return SyscallResult::err(SysError::AccessDenied);
    }
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
    // Snapshot into a stack buffer only. SYSCALL entry clears IF (SFMASK),
    // so this path must never allocate — `TextBuf` + fixed `staged` only.
    let mut staged = [0u8; MAX_READ as usize];
    let mut out = TextBuf {
        dst: &mut staged[..len as usize],
        n: 0,
    };
    match kind {
        Query::Stats => render_stats(&mut out),
        Query::Tasks => render_tasks(&mut out),
        Query::Threads => render_threads(&mut out),
        Query::Files => render_files(&mut out),
    }
    let n = out.n;
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

/// Stack-only sink for query text (no heap). Regression: never replace with
/// `String` / `Vec` — `read` on stats/tasks/threads/files runs with IF=0.
struct TextBuf<'a> {
    dst: &'a mut [u8],
    n: usize,
}

impl TextBuf<'_> {
    fn push(&mut self, bytes: &[u8]) {
        let room = self.dst.len().saturating_sub(self.n);
        let take = bytes.len().min(room);
        self.dst[self.n..self.n + take].copy_from_slice(&bytes[..take]);
        self.n += take;
    }

    fn push_u64(&mut self, value: u64) {
        if value == 0 {
            self.push(b"0");
            return;
        }
        let mut tmp = [0u8; 20];
        let mut i = tmp.len();
        let mut n = value;
        while n > 0 {
            i -= 1;
            tmp[i] = b'0' + (n % 10) as u8;
            n /= 10;
        }
        self.push(&tmp[i..]);
    }

    /// `{:#x}` form: `0x` plus lowercase digits, no leading zeros (`0x0` for 0).
    fn push_hex(&mut self, value: u64) {
        self.push(b"0x");
        if value == 0 {
            self.push(b"0");
            return;
        }
        let mut tmp = [0u8; 16];
        let mut i = tmp.len();
        let mut n = value;
        while n > 0 {
            i -= 1;
            let digit = (n & 0xf) as u8;
            tmp[i] = if digit < 10 {
                b'0' + digit
            } else {
                b'a' + (digit - 10)
            };
            n >>= 4;
        }
        self.push(&tmp[i..]);
    }
}

fn render_stats(out: &mut TextBuf<'_>) {
    let (heap_start, heap_size) = crate::arch::mm::heap::stats();
    let ticks = crate::arch::timer_ticks();
    out.push(b"uptime: ");
    out.push_u64(ticks / 1000);
    out.push(b".");
    // Tenths of a second from the ~1 kHz tick (not wall-clock).
    out.push_u64((ticks / 100) % 10);
    out.push(b"s\nframes free: ");
    out.push_u64(crate::arch::mm::free_frames() as u64);
    out.push(b"\nheap: ");
    out.push_u64(crate::arch::mm::heap::used_bytes() as u64);
    out.push(b" used, ");
    out.push_u64(crate::arch::mm::heap::free_bytes() as u64);
    out.push(b" free of ");
    out.push_u64(heap_size / 1024);
    out.push(b" KiB\nheap at ");
    out.push_hex(heap_start);
    out.push(b"\ngalfs: ");
    out.push_u64(crate::sched::galfs::blocks_used() as u64);
    out.push(b" / ");
    out.push_u64(crate::sched::galfs::BLOCK_SLOTS as u64);
    out.push(b" blocks\n");
}

fn render_tasks(out: &mut TextBuf<'_>) {
    out.push(b"cooperative tasks: ");
    out.push_u64(crate::sched::active_tasks() as u64);
    out.push(b" active, ");
    out.push_u64(crate::sched::spawned_total() as u64);
    out.push(b" spawned since boot\npreemption: timer @ ~1kHz, round-robin incl. main loop\n");
    // Process labels: debug id is for listings only (not a handle).
    crate::sched::for_user_tasks(|id, name, state| {
        out.push(b"id=");
        out.push_u64(id);
        out.push(b" name=");
        out.push(name.as_bytes());
        out.push(b" state=");
        out.push(state.as_bytes());
        out.push(b"\n");
    });
}

fn render_files(out: &mut TextBuf<'_>) {
    crate::sched::ramdisk::for_each_name(|name| {
        out.push(name.as_bytes());
        out.push(b"\n");
    });
    crate::sched::for_each_scratch_path(|path| {
        out.push(path);
        out.push(b"\n");
    });
    // A positive read of an empty archive is still a snapshot, not "you
    // asked for zero bytes".
    if out.n == 0 {
        out.push(b"\n");
    }
}

fn render_threads(out: &mut TextBuf<'_>) {
    crate::sched::for_running_threads(|id, name, ticks| {
        out.push(b"id=");
        out.push_u64(id);
        out.push(b" ");
        out.push(name.as_bytes());
        out.push(b": ");
        out.push_u64(ticks);
        out.push(b" ticks\n");
    });
    out.push(b"main loop: ");
    out.push_u64(crate::sched::main_ticks());
    out.push(b" ticks\n");
}

fn syscall_read_keyboard_ex(cap: Cap, addr: u64, len: u64) -> IoResult {
    if !crate::sched::task_granted(crate::sched::Grant::Keyboard) {
        return IoResult::Done(SyscallResult::err(SysError::AccessDenied));
    }
    if !cap.rights().contains(CapRights::READ) {
        return IoResult::Done(SyscallResult::err(SysError::AccessDenied));
    }
    let len = len.min(MAX_READ);
    if len == 0 {
        return IoResult::Done(SyscallResult::ok(0));
    }
    if user_buffer(addr, len, true).is_err() {
        return IoResult::Done(SyscallResult::err(SysError::BadBuffer));
    }
    let tty = crate::sched::current_tty();
    let mut staged = [0u8; MAX_READ as usize];
    let mut filled = 0usize;
    while filled < len as usize {
        let Some(c) = crate::drivers::keyboard::pop_key_tty(tty) else {
            break;
        };
        let mut tmp = [0u8; 4];
        let encoded = c.encode_utf8(&mut tmp);
        if filled + encoded.len() > len as usize {
            crate::drivers::keyboard::unget_key_tty(tty, c);
            break;
        }
        staged[filled..filled + encoded.len()].copy_from_slice(encoded.as_bytes());
        filled += encoded.len();
    }
    if filled == 0 {
        // Block until a key arrives (Milestone 57).
        return match crate::sched::task_park_keyboard(cap.bits(), addr, len as u32) {
            Ok(()) => IoResult::Park,
            Err(err) => IoResult::Done(SyscallResult::err(err)),
        };
    }
    // SAFETY: the destination was accepted as present, user, writable.
    unsafe {
        core::ptr::copy_nonoverlapping(
            staged.as_ptr(),
            VirtAddr::new(addr).as_mut_ptr::<u8>(),
            filled,
        );
    }
    IoResult::Done(SyscallResult::ok(filled as u64))
}

fn syscall_spawn(frame: &Context) -> SyscallResult {
    let cap = Cap::from_bits(frame.rdi);
    let addr = frame.rsi;
    let len = frame.rdx;
    if cap.index() != galexy_abi::reserved::LOADER_INDEX {
        return SyscallResult::err(SysError::BadCap);
    }
    if !cap.rights().contains(CapRights::EXEC) {
        return SyscallResult::err(SysError::AccessDenied);
    }
    if !crate::sched::task_granted(crate::sched::Grant::Loader) {
        return SyscallResult::err(SysError::AccessDenied);
    }
    if len == 0 || len > MAX_NAME {
        return SyscallResult::err(SysError::BadValue);
    }
    if frame.r10
        & !(galexy_abi::SPAWN_GRANT_QUERY
            | galexy_abi::SPAWN_WAIT
            | galexy_abi::SPAWN_INHERIT)
        != 0
    {
        return SyscallResult::err(SysError::BadValue);
    }
    let arg_len = frame.r9;
    if arg_len > crate::sched::ARG_MAX as u64 {
        return SyscallResult::err(SysError::BadValue);
    }
    if user_buffer(addr, len, false).is_err() {
        return SyscallResult::err(SysError::BadBuffer);
    }
    if arg_len > 0 && user_buffer(frame.r8, arg_len, false).is_err() {
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
    let mut arg = [0u8; crate::sched::ARG_MAX];
    if arg_len > 0 {
        // SAFETY: `user_buffer` accepted every byte of the argument.
        unsafe {
            core::ptr::copy_nonoverlapping(
                VirtAddr::new(frame.r8).as_ptr::<u8>(),
                arg.as_mut_ptr(),
                arg_len as usize,
            );
        }
    }
    let name = core::str::from_utf8(&raw[..len as usize]).unwrap_or("");
    if !file_name_ok(name) {
        return SyscallResult::err(SysError::BadValue);
    }
    // Milestone 54: F-key seat names share the ramdisk `shell` ELF.
    let elf_name = if crate::sched::is_console_shell_name(name) {
        "shell"
    } else {
        name
    };
    let Some(bytes) = crate::sched::ramdisk::find(elf_name) else {
        return SyscallResult::err(SysError::NotFound);
    };
    if !crate::sched::loader::looks_like_elf(bytes) {
        return SyscallResult::err(SysError::Unsupported);
    }
    let query = frame.r10 & galexy_abi::SPAWN_GRANT_QUERY != 0;
    let wait_exit = frame.r10 & galexy_abi::SPAWN_WAIT != 0;
    let inherit = frame.r10 & galexy_abi::SPAWN_INHERIT != 0;
    match crate::sched::task_spawn(name, &arg[..arg_len as usize], query, wait_exit, inherit)
    {
        Ok(()) => SyscallResult::ok(0),
        Err(err) => SyscallResult::err(err),
    }
}

fn syscall_kill(cap: Cap) -> SyscallResult {
    match crate::sched::task_kill(cap) {
        Ok(()) => SyscallResult::ok(0),
        Err(err) => SyscallResult::err(err),
    }
}

fn syscall_power(cap: Cap, op: u64) -> SyscallResult {
    if cap.index() != galexy_abi::reserved::POWER_INDEX {
        return SyscallResult::err(SysError::BadCap);
    }
    if !cap.rights().contains(CapRights::POWER) {
        return SyscallResult::err(SysError::AccessDenied);
    }
    if !crate::sched::task_granted(crate::sched::Grant::Power) {
        return SyscallResult::err(SysError::AccessDenied);
    }
    // Durability before the machine goes away (write-back may still be dirty).
    crate::sched::galfs::sync();
    match op {
        galexy_abi::POWER_SHUTDOWN => crate::arch::power::shutdown(),
        galexy_abi::POWER_REBOOT => crate::arch::power::reboot(),
        _ => return SyscallResult::err(SysError::BadValue),
    }
    // The platform ignored the request. The machine is still up.
    SyscallResult::err(SysError::Unsupported)
}

fn syscall_close(cap: Cap) -> SyscallResult {
    match crate::sched::task_close(cap) {
        Ok(()) => SyscallResult::ok(0),
        Err(err) => SyscallResult::err(err),
    }
}

/// Exact ramdisk program names: `banner.txt`, `hello`. No slashes.
fn file_name_ok(name: &str) -> bool {
    !name.is_empty()
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'_' || b == b'-')
}

/// A galfs path: optional leading `/`, optional `owner@` on the first
/// component, then `/`-separated names, optional trailing slash.
fn path_ok(name: &str) -> bool {
    !name.is_empty() && crate::sched::galfs::parse_path(name).is_ok()
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
