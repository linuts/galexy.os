//! Syscall dispatch table (scheduler-adjacent policy — see DESIGN.md
//! boundary rule 7): which syscall number does what. The MECHANISM (MSR
//! setup, naked entry, frame building) lives in `arch/`; the ABI in
//! `galexy-abi`; this file is the policy that binds numbers to behavior.
//!
//! Register/return contract (the arch shim guarantees this):
//! - args arrive in the frame: `a0 = frame.rdi`, `a1 = frame.rsi`,
//!   `a2 = frame.rdx`. `spawn` also reads `r8`/`r9` (argument) and `r10`
//!   (grant bits).
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
    if cap.index() != galexy_abi::reserved::CONSOLE_INDEX {
        return syscall_write_file(cap, addr, len);
    }
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
    // tab (0x09), CR (0x0d), and ESC (0x1b, for CSI). Other controls
    // stay BadValue.
    let printable = staged[..len as usize].iter().all(|b| {
        b.is_ascii_graphic()
            || *b == b' '
            || *b == b'\n'
            || *b == 0x08
            || *b == 0x09
            || *b == 0x0c
            || *b == 0x0d
            || *b == 0x1b
    });
    if !printable {
        return SyscallResult::err(SysError::BadValue);
    }
    let text = core::str::from_utf8(&staged[..len as usize]).unwrap_or("");
    // Console policy (drivers::console): screen + serial — visible
    // interactively and observable headless.
    crate::drivers::console::out_str(text);
    SyscallResult::ok(len)
}

/// Copies `src` onto a scratch-file cap. Any bytes are legal; the console
/// charset does not apply. An archive open is `Unsupported`.
fn syscall_write_file(cap: Cap, addr: u64, len: u64) -> SyscallResult {
    if len > MAX_WRITE {
        return SyscallResult::err(SysError::BadValue);
    }
    if len > 0 && user_buffer(addr, len, false).is_err() {
        return SyscallResult::err(SysError::BadBuffer);
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
    match crate::sched::task_write(cap, &staged[..len as usize]) {
        Ok(n) => SyscallResult::ok(n as u64),
        Err(err) => SyscallResult::err(err),
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

fn syscall_read(cap: Cap, addr: u64, len: u64) -> SyscallResult {
    if cap.index() == galexy_abi::reserved::KEYBOARD_INDEX {
        return syscall_read_keyboard(cap, addr, len);
    }
    if let Some(kind) = query_kind(cap.index()) {
        return syscall_read_query(cap, kind, addr, len);
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

/// A stack buffer for query text. The syscall runs with interrupts off, so
/// this must not allocate.
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
    out.push(b"frames free: ");
    out.push_u64(crate::arch::mm::free_frames() as u64);
    out.push(b"\nheap: ");
    out.push_u64(crate::arch::mm::heap::used_bytes() as u64);
    out.push(b" used, ");
    out.push_u64(crate::arch::mm::heap::free_bytes() as u64);
    out.push(b" free of ");
    out.push_u64(heap_size / 1024);
    out.push(b" KiB\nheap at ");
    out.push_hex(heap_start);
    out.push(b"\n");
}

fn render_tasks(out: &mut TextBuf<'_>) {
    out.push(b"cooperative tasks: ");
    out.push_u64(crate::sched::active_tasks() as u64);
    out.push(b" active, ");
    out.push_u64(crate::sched::spawned_total() as u64);
    out.push(b" spawned since boot\npreemption: timer @ ~1kHz, round-robin incl. main loop\n");
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
    crate::sched::for_running_threads(|name, ticks| {
        out.push(name.as_bytes());
        out.push(b": ");
        out.push_u64(ticks);
        out.push(b" ticks\n");
    });
    out.push(b"main loop: ");
    out.push_u64(crate::sched::main_ticks());
    out.push(b" ticks\n");
}

fn syscall_read_keyboard(cap: Cap, addr: u64, len: u64) -> SyscallResult {
    if !crate::sched::task_granted(crate::sched::Grant::Keyboard) {
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
    if frame.r10 & !galexy_abi::SPAWN_GRANT_QUERY != 0 {
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
    let Some(bytes) = crate::sched::ramdisk::find(name) else {
        return SyscallResult::err(SysError::NotFound);
    };
    if !crate::sched::loader::looks_like_elf(bytes) {
        return SyscallResult::err(SysError::Unsupported);
    }
    let query = frame.r10 & galexy_abi::SPAWN_GRANT_QUERY != 0;
    match crate::sched::task_spawn(name, &arg[..arg_len as usize], query) {
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

/// A scratch path: components separated by `/`, optional trailing slash.
fn path_ok(name: &str) -> bool {
    if name.is_empty() {
        return false;
    }
    let body = name.strip_suffix('/').unwrap_or(name);
    if body.is_empty() || body.ends_with('/') {
        return false;
    }
    let mut comps = 0usize;
    for comp in body.split('/') {
        if comp.is_empty() || comp == "." || comp == ".." {
            return false;
        }
        let ok = comp
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'_' || b == b'-');
        if !ok {
            return false;
        }
        comps += 1;
        if comps > 8 {
            return false;
        }
    }
    comps > 0
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
