//! The one `syscall` instruction in `std`, and typed wrappers over it.
//!
//! Register contract (`docs/DESIGN.md`, arch/syscall): RAX = number in /
//! value out, RDI/RSI/RDX = args, R8/R9/R10 = extra args, RDX = ok flag
//! out. RCX and R11 are clobbered by the instruction itself.

use core::arch::asm;

use super::abi::{Cap, CapRights, SysError, Syscall, SyscallResult};
use crate::io;
use crate::sync::atomic::Atomic;

/// The stub. `#[inline(never)] extern "C"` so a code generator without
/// inline assembly (cg_clif on the OS itself) links an ordinary function
/// and never meets the `asm!`. `SyscallResult` is `#[repr(C)]`, so the
/// SysV ABI returns it in RAX:RDX.
#[inline(never)]
pub extern "C" fn raw_syscall(
    number: u64,
    a0: u64,
    a1: u64,
    a2: u64,
    r8: u64,
    r9: u64,
    r10: u64,
) -> SyscallResult {
    let value: u64;
    let ok: u64;
    // SAFETY: the kernel's syscall entry is uniform; rcx/r11 are clobbered
    // by the instruction itself per the x86_64 syscall ABI. The kernel
    // preserves every other register.
    unsafe {
        asm!(
            "syscall",
            inlateout("rax") number => value,
            inlateout("rdx") a2 => ok,
            in("rdi") a0,
            in("rsi") a1,
            in("r8") r8,
            in("r9") r9,
            in("r10") r10,
            lateout("rcx") _,
            lateout("r11") _,
            options(nostack),
        );
    }
    SyscallResult { ok: ok != 0, value }
}

/// Reads the FS base register (the per-thread TLS table pointer).
///
/// Same `#[inline(never)] extern "C"` discipline as [`raw_syscall`].
/// CR4.FSGSBASE is set at kernel bring-up, so ring 3 may use the
/// instruction directly; the kernel saves and restores the value across
/// thread switches and never writes FS itself.
#[inline(never)]
pub extern "C" fn read_fs_base() -> u64 {
    let value: u64;
    // SAFETY: CR4.FSGSBASE is asserted at boot (`arch/cpu.rs`).
    unsafe { asm!("rdfsbase {}", out(reg) value, options(nomem, nostack)) };
    value
}

/// Writes the FS base register. See [`read_fs_base`].
#[inline(never)]
pub extern "C" fn write_fs_base(value: u64) {
    // SAFETY: as above; FS is unused by compiled code on this target
    // (`has-thread-local: false`), so the base is ours to repurpose.
    unsafe { asm!("wrfsbase {}", in(reg) value, options(nomem, nostack)) };
}

#[inline]
fn finish(got: SyscallResult) -> Result<u64, SysError> {
    if got.ok { Ok(got.value) } else { Err(SysError::from_code(got.value)) }
}

/// Number + three arguments.
pub fn syscall(number: Syscall, a0: u64, a1: u64, a2: u64) -> Result<u64, SysError> {
    finish(raw_syscall(number as u64, a0, a1, a2, 0, 0, 0))
}

/// Number + five arguments (`r8`/`r9`).
pub fn syscall6(
    number: Syscall,
    a0: u64,
    a1: u64,
    a2: u64,
    r8: u64,
    r9: u64,
) -> Result<u64, SysError> {
    finish(raw_syscall(number as u64, a0, a1, a2, r8, r9, 0))
}

/// `SysError` → `io::Error`. The raw OS error is the ABI code, so
/// `err.raw_os_error()` round-trips to `SysError::from_code`.
pub fn io_error(err: SysError) -> io::Error {
    io::Error::from_raw_os_error(err as i32)
}

/// Terminates the whole process. Never returns; a return would be a kernel
/// bug, so park on a privileged instruction the kernel turns into a kill.
pub fn exit(code: u64) -> ! {
    let _ = syscall(Syscall::Exit, code, 0, 0);
    park_forever()
}

/// Terminates the calling thread only (the leader's `ThreadExit` is `Exit`).
pub fn thread_exit(code: u64) -> ! {
    let _ = syscall(Syscall::ThreadExit, code, 0, 0);
    park_forever()
}

fn park_forever() -> ! {
    loop {
        // SAFETY: `cli` from ring 3 is a #GP the kernel answers by killing
        // the task; `hlt` the same. Either way this thread is gone.
        unsafe { asm!("cli", "hlt", options(nomem, nostack)) };
    }
}

pub fn yield_now() {
    let _ = syscall(Syscall::Yield, 0, 0, 0);
}

/// Parks for about `ms` milliseconds. The kernel clamps one call to
/// [`super::abi::SLEEP_MS_MAX`]; callers loop for longer waits.
pub fn sleep_ms(ms: u64) {
    let _ = syscall(Syscall::Sleep, ms, 0, 0);
}

/// Monotonic milliseconds since boot.
pub fn clock_ms() -> u64 {
    syscall(Syscall::Clock, 0, 0, 0).unwrap_or(0)
}

/// Grows the heap by `pages` 4 KiB frames; returns their base address.
pub fn map_pages(pages: u64) -> Result<u64, SysError> {
    syscall(Syscall::Map, pages, 0, 0)
}

pub fn console_cap() -> Cap {
    super::abi::reserved::console(CapRights::WRITE)
}

pub fn keyboard_cap() -> Cap {
    super::abi::reserved::keyboard(CapRights::READ)
}

pub fn files_cap() -> Cap {
    super::abi::reserved::files(CapRights::READ)
}

pub fn loader_cap() -> Cap {
    super::abi::reserved::loader(CapRights::EXEC)
}

/// File Cap for per-task slot `slot` (0-based from [`super::abi::FILE_CAP_BASE`]).
pub fn file_cap(slot: u64, rights: CapRights) -> Cap {
    Cap::new(super::abi::FILE_CAP_BASE + slot, rights)
}

/// Kernel stages at most this many bytes per `read` / `write`.
const IO_CHUNK: usize = 1024;

pub fn write(cap: Cap, bytes: &[u8]) -> Result<usize, SysError> {
    // An empty write still reaches the kernel. `probe_stdio` uses it to
    // tell a writable file slot from an empty one; returning Ok locally
    // would make every program treat slot 1 as stdout.
    // A non-empty write is short: `Write::write_all` loops. One syscall
    // cannot carry a 20 MiB buffer (the kernel stages 1024 bytes).
    let n = bytes.len().min(IO_CHUNK);
    syscall(Syscall::Write, cap.bits(), bytes.as_ptr().addr() as u64, n as u64).map(|n| n as usize)
}

pub fn read(cap: Cap, buf: &mut [u8]) -> Result<usize, SysError> {
    if buf.is_empty() {
        return syscall(Syscall::Read, cap.bits(), buf.as_mut_ptr().addr() as u64, 0)
            .map(|n| n as usize);
    }
    let n = buf.len().min(IO_CHUNK);
    syscall(Syscall::Read, cap.bits(), buf.as_mut_ptr().addr() as u64, n as u64).map(|n| n as usize)
}

pub fn close(cap: Cap) -> Result<(), SysError> {
    syscall(Syscall::Close, cap.bits(), 0, 0).map(|_| ())
}

/// Opens an existing ramdisk entry (READ) or galfs file (READ|WRITE).
pub fn open(path: &[u8]) -> Result<Cap, SysError> {
    syscall(Syscall::Open, path.as_ptr().addr() as u64, path.len() as u64, 0).map(Cap::from_bits)
}

/// Creates a galfs file (READ|WRITE Cap) or, with a trailing `/`, a
/// directory (`Cap::null()`). `replace` empties an existing file instead
/// of failing.
pub fn create(path: &[u8], replace: bool) -> Result<Cap, SysError> {
    syscall(Syscall::Create, path.as_ptr().addr() as u64, path.len() as u64, u64::from(replace))
        .map(Cap::from_bits)
}

pub fn remove(path: &[u8]) -> Result<(), SysError> {
    syscall(Syscall::Remove, path.as_ptr().addr() as u64, path.len() as u64, 0).map(|_| ())
}

pub fn rename(old: &[u8], new: &[u8]) -> Result<(), SysError> {
    syscall6(
        Syscall::Rename,
        old.as_ptr().addr() as u64,
        old.len() as u64,
        new.as_ptr().addr() as u64,
        new.len() as u64,
        0,
    )
    .map(|_| ())
}

pub fn seek(cap: Cap, offset: i64, whence: u64) -> Result<u64, SysError> {
    syscall(Syscall::Seek, cap.bits(), offset as u64, whence)
}

pub fn truncate(cap: Cap, size: u64) -> Result<(), SysError> {
    syscall(Syscall::Truncate, cap.bits(), size, 0).map(|_| ())
}

/// `stat` into a [`super::abi::STAT_LEN`]-byte buffer.
pub fn stat(path: &[u8], buf: &mut [u8; super::abi::STAT_LEN]) -> Result<(), SysError> {
    syscall6(
        Syscall::Stat,
        path.as_ptr().addr() as u64,
        path.len() as u64,
        buf.as_mut_ptr().addr() as u64,
        buf.len() as u64,
        0,
    )
    .map(|_| ())
}

pub fn sync() -> Result<(), SysError> {
    syscall(Syscall::Sync, 0, 0, 0).map(|_| ())
}

/// Creates a pipe: `(read end, write end)`.
pub fn pipe() -> Result<(Cap, Cap), SysError> {
    let mut out = [0u64; 2];
    syscall(Syscall::Pipe, out.as_mut_ptr().addr() as u64, 0, 0)?;
    Ok((Cap::from_bits(out[0]), Cap::from_bits(out[1])))
}

/// `spawn(name, arg, grants)`: without `SPAWN_WAIT`, a process Cap.
pub fn spawn(name: &[u8], arg: &[u8], grants: u64) -> Result<Cap, SysError> {
    finish(raw_syscall(
        Syscall::Spawn as u64,
        loader_cap().bits(),
        name.as_ptr().addr() as u64,
        name.len() as u64,
        arg.as_ptr().addr() as u64,
        arg.len() as u64,
        grants,
    ))
    .map(Cap::from_bits)
}

/// Parks until the process Cap's task exits; returns its exit code.
pub fn wait(cap: Cap) -> Result<u64, SysError> {
    syscall(Syscall::Wait, cap.bits(), 0, 0)
}

/// Like [`wait`], but a live child is `Err(NoResource)` and nothing parks.
pub fn wait_poll(cap: Cap) -> Result<u64, SysError> {
    syscall(Syscall::Wait, cap.bits(), super::abi::WAIT_POLL, 0)
}

pub fn kill(cap: Cap) -> Result<(), SysError> {
    syscall(Syscall::Kill, cap.bits(), 0, 0).map(|_| ())
}

/// Starts a thread at `entry(arg)` with a `stack_pages` user stack; parks
/// until it exists. Returns the join Cap.
pub fn thread_spawn(
    entry: unsafe extern "C" fn(u64) -> !,
    arg: u64,
    stack_pages: u64,
) -> Result<Cap, SysError> {
    syscall(Syscall::ThreadSpawn, entry as usize as u64, arg, stack_pages).map(Cap::from_bits)
}

/// `Ok(())` when woken or when `*word != expected`; `Err(TimedOut)` when
/// `timeout_ms` (non-zero) fired first.
pub fn futex_wait(word: &Atomic<u32>, expected: u32, timeout_ms: u64) -> Result<(), SysError> {
    syscall6(
        Syscall::Futex,
        super::abi::FUTEX_WAIT,
        word.as_ptr().addr() as u64,
        u64::from(expected),
        timeout_ms,
        0,
    )
    .map(|_| ())
}

/// Wakes up to `count` waiters on `word`; returns how many.
pub fn futex_wake(word: &Atomic<u32>, count: u64) -> Result<u64, SysError> {
    syscall(Syscall::Futex, super::abi::FUTEX_WAKE, word.as_ptr().addr() as u64, count)
}

/// Reads the self snapshot (`id=… name=… state=…`) and returns the name.
pub fn self_name() -> Option<crate::string::String> {
    let mut buf = [0u8; 160];
    let n = read(super::abi::reserved::self_cap(), &mut buf).ok()?;
    let text = core::str::from_utf8(&buf[..n]).ok()?;
    let rest = text.split_once("name=")?.1;
    let name = rest.split(' ').next()?.trim_end();
    if name.is_empty() { None } else { Some(name.into()) }
}
