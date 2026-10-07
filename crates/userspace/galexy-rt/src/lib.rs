//! galexy-rt: the ring-3 runtime.
//!
//! Everything a user program links against. Never the kernel — the only
//! contract with it is the syscall ABI (`galexy-abi`). Syscall wrappers
//! carry the register contract from `docs/DESIGN.md` (arch/syscall):
//! RAX = number in / value out, RDX = len-arg in / ok-flag out,
//! RDI/RSI = args. RCX and R11 are clobbered by the instruction itself.

#![no_std]
#![deny(clippy::all)]
#![deny(missing_docs)]

use core::arch::asm;
use core::cell::UnsafeCell;
use core::panic::PanicInfo;

use galexy_abi::{Cap, CapRights, Syscall, SyscallResult};

/// The console capability as granted to user programs (WRITE right).
pub fn console_cap() -> Cap {
    galexy_abi::reserved::console(CapRights::WRITE)
}

/// The self capability (inspect). [`read`] returns `id=… name=… state=…`.
pub fn self_cap() -> Cap {
    galexy_abi::reserved::self_cap()
}

/// One syscall: number + 3 args, register-form result back.
pub fn syscall(number: u64, a0: u64, a1: u64, a2: u64) -> SyscallResult {
    let value: u64;
    let ok: u64;
    // SAFETY: the kernel's syscall entry is uniform; rcx/r11 are clobbered
    // by the instruction itself per the x86_64 syscall ABI.
    unsafe {
        asm!(
            "syscall",
            inlateout("rax") number => value,
            inlateout("rdx") a2 => ok,
            in("rdi") a0,
            in("rsi") a1,
            lateout("rcx") _,
            lateout("r11") _,
            options(nostack),
        );
    }
    SyscallResult { ok: ok != 0, value }
}

/// Writes bytes to the console through the console capability.
pub fn write_console(bytes: &[u8]) -> SyscallResult {
    let cap = console_cap();
    syscall(
        Syscall::Write as u64,
        cap.bits(),
        bytes.as_ptr() as u64,
        bytes.len() as u64,
    )
}

/// Creates a scratch file or directory. On success, a file's `value` is a
/// READ and WRITE capability. A directory path (ending in `/`) returns `0`.
///
/// The name is a path (`note`, `box/leaf`, `box/`). A ramdisk name at `/`
/// is rejected. The bytes live in a fixed kernel table.
pub fn create(name: &[u8]) -> SyscallResult {
    syscall(
        Syscall::Create as u64,
        name.as_ptr() as u64,
        name.len() as u64,
        0,
    )
}

/// Removes a scratch file or an empty directory. The slot can be created
/// again. A ramdisk name, or a directory that still has a child, fails.
pub fn remove(name: &[u8]) -> SyscallResult {
    syscall(
        Syscall::Remove as u64,
        name.as_ptr() as u64,
        name.len() as u64,
        0,
    )
}

/// Installs a galfs token on a live user task.
///
/// `rights` is a mask of [`galexy_abi::TOKEN_READ`] and friends. The
/// caller must already hold every bit being granted. `r8`/`r9` are the
/// target task name.
pub fn grant(path: &[u8], rights: u64, task: &[u8]) -> SyscallResult {
    let value: u64;
    let ok: u64;
    // SAFETY: same syscall entry as [`syscall`]. r8/r9 name the target;
    // rcx/r11 are clobbered by the instruction.
    unsafe {
        asm!(
            "syscall",
            inlateout("rax") Syscall::Grant as u64 => value,
            inlateout("rdx") rights => ok,
            in("rdi") path.as_ptr() as u64,
            in("rsi") path.len() as u64,
            in("r8") task.as_ptr() as u64,
            in("r9") task.len() as u64,
            lateout("rcx") _,
            lateout("r11") _,
            options(nostack),
        );
    }
    SyscallResult { ok: ok != 0, value }
}

/// Drops galfs token rights from a live user task. Same layout as [`grant`].
pub fn revoke(path: &[u8], rights: u64, task: &[u8]) -> SyscallResult {
    let value: u64;
    let ok: u64;
    // SAFETY: same syscall entry as [`grant`].
    unsafe {
        asm!(
            "syscall",
            inlateout("rax") Syscall::Revoke as u64 => value,
            inlateout("rdx") rights => ok,
            in("rdi") path.as_ptr() as u64,
            in("rsi") path.len() as u64,
            in("r8") task.as_ptr() as u64,
            in("r9") task.len() as u64,
            lateout("rcx") _,
            lateout("r11") _,
            options(nostack),
        );
    }
    SyscallResult { ok: ok != 0, value }
}

/// Durable home share for actor `user` (re-applied at login).
pub fn share(path: &[u8], rights: u64, user: &[u8]) -> SyscallResult {
    let value: u64;
    let ok: u64;
    // SAFETY: same register layout as [`grant`]; target is an actor name.
    unsafe {
        asm!(
            "syscall",
            inlateout("rax") Syscall::Share as u64 => value,
            inlateout("rdx") rights => ok,
            in("rdi") path.as_ptr() as u64,
            in("rsi") path.len() as u64,
            in("r8") user.as_ptr() as u64,
            in("r9") user.len() as u64,
            lateout("rcx") _,
            lateout("r11") _,
            options(nostack),
        );
    }
    SyscallResult { ok: ok != 0, value }
}

/// Clear durable share rights for actor `user`.
pub fn unshare(path: &[u8], rights: u64, user: &[u8]) -> SyscallResult {
    let value: u64;
    let ok: u64;
    // SAFETY: same entry as [`share`].
    unsafe {
        asm!(
            "syscall",
            inlateout("rax") Syscall::Unshare as u64 => value,
            inlateout("rdx") rights => ok,
            in("rdi") path.as_ptr() as u64,
            in("rsi") path.len() as u64,
            in("r8") user.as_ptr() as u64,
            in("r9") user.len() as u64,
            lateout("rcx") _,
            lateout("r11") _,
            options(nostack),
        );
    }
    SyscallResult { ok: ok != 0, value }
}

/// Creates a pipe. On success, `out` receives `[read_cap, write_cap]` bits.
pub fn pipe(out: &mut [u64; 2]) -> SyscallResult {
    syscall(Syscall::Pipe as u64, out.as_mut_ptr() as u64, 0, 0)
}

/// Moves an open file or pipe cap to another live user task.
///
/// On success, `value` is the target's new capability bits.
pub fn give(cap: Cap, task: &[u8]) -> SyscallResult {
    syscall(
        Syscall::Give as u64,
        cap.bits(),
        task.as_ptr() as u64,
        task.len() as u64,
    )
}

/// Sets the read cursor on an open file. `whence` is [`galexy_abi::SEEK_SET`],
/// [`galexy_abi::SEEK_CUR`], or [`galexy_abi::SEEK_END`].
pub fn seek(cap: Cap, offset: i64, whence: u64) -> SyscallResult {
    syscall(Syscall::Seek as u64, cap.bits(), offset as u64, whence)
}

/// Actor whoami/users: write into `buf`. `op` is [`galexy_abi::USER_WHOAMI`]
/// or [`galexy_abi::USER_USERS`]. On success, `value` is the byte count.
pub fn user(buf: &mut [u8], op: u64) -> SyscallResult {
    syscall(
        Syscall::User as u64,
        buf.as_mut_ptr() as u64,
        buf.len() as u64,
        op,
    )
}

/// Actor add/del/su. `op` is [`galexy_abi::USER_ADD`], [`galexy_abi::USER_DEL`],
/// or [`galexy_abi::USER_SU`]. Prefer [`user_name_pass`] for add.
pub fn user_name(name: &[u8], op: u64) -> SyscallResult {
    user_name_pass(name, &[], op)
}

/// Like [`user_name`], with a password in `r8`/`r9` for add/login/passwd.
pub fn user_name_pass(name: &[u8], password: &[u8], op: u64) -> SyscallResult {
    let value: u64;
    let ok: u64;
    // SAFETY: same syscall entry as [`syscall`]. r8/r9 are the password.
    unsafe {
        asm!(
            "syscall",
            inlateout("rax") Syscall::User as u64 => value,
            inlateout("rdx") op => ok,
            in("rdi") name.as_ptr() as u64,
            in("rsi") name.len() as u64,
            in("r8") password.as_ptr() as u64,
            in("r9") password.len() as u64,
            lateout("rcx") _,
            lateout("r11") _,
            options(nostack),
        );
    }
    SyscallResult { ok: ok != 0, value }
}

/// Password login as `name`.
pub fn user_login(name: &[u8], password: &[u8]) -> SyscallResult {
    user_name_pass(name, password, galexy_abi::USER_LOGIN)
}

/// Clear the current session (logged out).
pub fn user_logout() -> SyscallResult {
    user_name_pass(&[], &[], galexy_abi::USER_LOGOUT)
}

/// Set password for `name` (empty name = self).
pub fn user_passwd(name: &[u8], password: &[u8]) -> SyscallResult {
    user_name_pass(name, password, galexy_abi::USER_PASSWD)
}

/// Reads an actor quota record into `buf` (at least [`galexy_abi::QUOTA_LEN`]).
/// Empty `name` means the caller's actor.
pub fn user_quota(buf: &mut [u8], name: &[u8]) -> SyscallResult {
    let value: u64;
    let ok: u64;
    // SAFETY: user buffer + optional name; same entry as other `user` ops.
    unsafe {
        asm!(
            "syscall",
            inlateout("rax") Syscall::User as u64 => value,
            inlateout("rdx") galexy_abi::USER_QUOTA => ok,
            in("rdi") buf.as_mut_ptr() as u64,
            in("rsi") buf.len() as u64,
            in("r8") name.as_ptr() as u64,
            in("r9") name.len() as u64,
            lateout("rcx") _,
            lateout("r11") _,
            options(nostack),
        );
    }
    SyscallResult { ok: ok != 0, value }
}

/// Admin: set object/byte limits for actor `name`.
pub fn user_setquota(name: &[u8], max_objects: u16, max_bytes: u32) -> SyscallResult {
    let value: u64;
    let ok: u64;
    // SAFETY: name is a user buffer; limits ride in r8/r9.
    unsafe {
        asm!(
            "syscall",
            inlateout("rax") Syscall::User as u64 => value,
            inlateout("rdx") galexy_abi::USER_SETQUOTA => ok,
            in("rdi") name.as_ptr() as u64,
            in("rsi") name.len() as u64,
            in("r8") max_objects as u64,
            in("r9") max_bytes as u64,
            lateout("rcx") _,
            lateout("r11") _,
            options(nostack),
        );
    }
    SyscallResult { ok: ok != 0, value }
}

/// Like [`create`], and if the path is an existing scratch file its bytes
/// are emptied first.
pub fn create_replace(name: &[u8]) -> SyscallResult {
    syscall(
        Syscall::Create as u64,
        name.as_ptr() as u64,
        name.len() as u64,
        1,
    )
}

/// Moves a galfs dirent from `old` to `new` without copying bytes.
pub fn rename(old: &[u8], new: &[u8]) -> SyscallResult {
    let value: u64;
    let ok: u64;
    // SAFETY: same syscall entry as [`grant`]; rdx/r8 carry the new path.
    unsafe {
        asm!(
            "syscall",
            inlateout("rax") Syscall::Rename as u64 => value,
            inlateout("rdx") new.as_ptr() as u64 => ok,
            in("rdi") old.as_ptr() as u64,
            in("rsi") old.len() as u64,
            in("r8") new.len() as u64,
            lateout("rcx") _,
            lateout("r11") _,
            options(nostack),
        );
    }
    SyscallResult { ok: ok != 0, value }
}

/// Sets the length of an open galfs file. Requires WRITE on `cap`.
pub fn truncate(cap: Cap, size: u64) -> SyscallResult {
    syscall(Syscall::Truncate as u64, cap.bits(), size, 0)
}

/// Flushes the galfs dual-slot image when disk-backed.
pub fn sync() -> SyscallResult {
    syscall(Syscall::Sync as u64, 0, 0, 0)
}

/// Lists the caller's galfs tokens into `buf`.
pub fn user_tokens(buf: &mut [u8]) -> SyscallResult {
    user(buf, galexy_abi::USER_TOKENS)
}

/// Writes galfs metadata for `path` into `buf` (at least [`galexy_abi::STAT_LEN`]).
pub fn stat(path: &[u8], buf: &mut [u8]) -> SyscallResult {
    let value: u64;
    let ok: u64;
    // SAFETY: same syscall entry as [`rename`]; rdx/r8 are the out buffer.
    unsafe {
        asm!(
            "syscall",
            inlateout("rax") Syscall::Stat as u64 => value,
            inlateout("rdx") buf.as_mut_ptr() as u64 => ok,
            in("rdi") path.as_ptr() as u64,
            in("rsi") path.len() as u64,
            in("r8") buf.len() as u64,
            lateout("rcx") _,
            lateout("r11") _,
            options(nostack),
        );
    }
    SyscallResult { ok: ok != 0, value }
}

/// Writes bytes to a capability (a scratch file, or the console).
pub fn write(cap: Cap, bytes: &[u8]) -> SyscallResult {
    syscall(
        Syscall::Write as u64,
        cap.bits(),
        bytes.as_ptr() as u64,
        bytes.len() as u64,
    )
}

/// Opens a ramdisk file by exact name (`banner.txt`, `hello`).
///
/// On success, `value` is the new capability's bits (READ right).
pub fn open(name: &[u8]) -> SyscallResult {
    syscall(
        Syscall::Open as u64,
        name.as_ptr() as u64,
        name.len() as u64,
        0,
    )
}

/// Reads the next bytes of an open file into `buf`.
///
/// On success, `value` is the number of bytes copied. `0` is end of file.
pub fn read(cap: Cap, buf: &mut [u8]) -> SyscallResult {
    syscall(
        Syscall::Read as u64,
        cap.bits(),
        buf.as_mut_ptr() as u64,
        buf.len() as u64,
    )
}

/// Drops a file or process capability.
pub fn close(cap: Cap) -> SyscallResult {
    syscall(Syscall::Close as u64, cap.bits(), 0, 0)
}

/// Blocks until the process Cap's task exits; returns the exit code.
///
/// Requires [`CapRights::PROC_WAIT`]. The Cap is stale after a successful
/// wait.
pub fn wait(cap: Cap) -> SyscallResult {
    syscall(Syscall::Wait as u64, cap.bits(), 0, 0)
}

/// Stops the task named by a process Cap.
///
/// Requires [`CapRights::PROC_KILL`].
pub fn kill(cap: Cap) -> SyscallResult {
    syscall(Syscall::Kill as u64, cap.bits(), 0, 0)
}

/// The keyboard capability (READ). `read` of zero bytes means no key is waiting.
pub fn keyboard_cap() -> Cap {
    galexy_abi::reserved::keyboard(CapRights::READ)
}

/// The loader capability (EXEC). [`spawn`] returns once the program is running.
pub fn loader_cap() -> Cap {
    galexy_abi::reserved::loader(CapRights::EXEC)
}

/// The stats capability (READ). [`read`] returns a fresh frame and heap report.
pub fn stats_cap() -> Cap {
    galexy_abi::reserved::stats(CapRights::READ)
}

/// The tasks capability (READ). [`read`] returns a fresh cooperative-task report.
pub fn tasks_cap() -> Cap {
    galexy_abi::reserved::tasks(CapRights::READ)
}

/// The threads capability (READ). [`read`] returns a fresh running-thread report.
pub fn threads_cap() -> Cap {
    galexy_abi::reserved::threads(CapRights::READ)
}

/// The files capability (READ). [`read`] returns a fresh ramdisk name list.
pub fn files_cap() -> Cap {
    galexy_abi::reserved::files(CapRights::READ)
}

/// The power capability. [`shutdown`] and [`reboot`] do not return when the
/// machine honors them.
pub fn power_cap() -> Cap {
    galexy_abi::reserved::power(CapRights::POWER)
}

/// Turns the machine off. Returns if it stayed up.
pub fn shutdown() -> SyscallResult {
    syscall(
        Syscall::Power as u64,
        power_cap().bits(),
        galexy_abi::POWER_SHUTDOWN,
        0,
    )
}

/// Resets the machine. Returns if it stayed up.
pub fn reboot() -> SyscallResult {
    syscall(
        Syscall::Power as u64,
        power_cap().bits(),
        galexy_abi::POWER_REBOOT,
        0,
    )
}

/// Starts the ramdisk program `name` and returns once it is running.
///
/// The child receives no argument and only the console grant.
pub fn spawn(name: &[u8]) -> SyscallResult {
    spawn_with(name, &[], 0)
}

/// Starts `name` with `arg` and `grants` (`SPAWN_GRANT_QUERY`,
/// `SPAWN_WAIT`, `SPAWN_INHERIT`, or a combination).
///
/// Without `SPAWN_WAIT`, returns a process Cap once the program is loaded.
/// With `SPAWN_WAIT`, parks until the child exits and returns its exit
/// code (the Cap remains installed). Utilities that Cap-wait themselves
/// pass `SPAWN_INHERIT` and call [`wait`] on the Cap. `r8`/`r9`/`r10` are
/// set explicitly so a leftover register is not an argument.
pub fn spawn_with(name: &[u8], arg: &[u8], grants: u64) -> SyscallResult {
    let value: u64;
    let ok: u64;
    let cap = loader_cap();
    // SAFETY: same syscall entry as [`syscall`]. r8/r9/r10 are the spawn
    // argument and grant bits; rcx/r11 are clobbered by the instruction.
    unsafe {
        asm!(
            "syscall",
            inlateout("rax") Syscall::Spawn as u64 => value,
            inlateout("rdx") name.len() as u64 => ok,
            in("rdi") cap.bits(),
            in("rsi") name.as_ptr() as u64,
            in("r8") arg.as_ptr() as u64,
            in("r9") arg.len() as u64,
            in("r10") grants,
            lateout("rcx") _,
            lateout("r11") _,
            options(nostack),
        );
    }
    SyscallResult { ok: ok != 0, value }
}

struct StartupArg {
    buf: UnsafeCell<[u8; 256]>,
    len: UnsafeCell<usize>,
}

// One task owns this image. `_start` writes it before `main`.
unsafe impl Sync for StartupArg {}

static STARTUP_ARG: StartupArg = StartupArg {
    buf: UnsafeCell::new([0; 256]),
    len: UnsafeCell::new(0),
};

/// Copies the kernel-placed argument into this program's own buffer.
///
/// # Safety
///
/// Called once from `_start`, before any other use of [`arg`]. `ptr` is
/// the address the loader passed in `rdi`, and `len` is `rsi`.
pub unsafe fn init_arg(ptr: *const u8, len: usize) {
    let n = len.min(256);
    // SAFETY: the loader mapped `ptr` for `len` bytes above the initial
    // stack pointer, and this runs before `main`.
    unsafe {
        if n > 0 && !ptr.is_null() {
            core::ptr::copy_nonoverlapping(ptr, STARTUP_ARG.buf.get().cast::<u8>(), n);
        }
        *STARTUP_ARG.len.get() = n;
    }
}

/// The argument the launcher passed, or an empty slice.
pub fn arg() -> &'static [u8] {
    // SAFETY: `_start` finishes the copy before `main` reads it. This task
    // does not write the buffer again.
    unsafe {
        let n = *STARTUP_ARG.len.get();
        core::slice::from_raw_parts(STARTUP_ARG.buf.get().cast::<u8>(), n)
    }
}

/// Gives up the rest of the scheduling quantum.
pub fn yield_now() -> SyscallResult {
    syscall(Syscall::Yield as u64, 0, 0, 0)
}

/// Exits the calling task with `code` (never returns).
pub fn exit(code: u64) -> ! {
    let _ = syscall(Syscall::Exit as u64, code, 0, 0);
    // `exit` never returns; reaching here = a kernel bug. Park with
    // interrupts off so nothing wedges further.
    loop {
        // SAFETY: cli+hlt on the park path is the intended behavior.
        unsafe { asm!("cli", "hlt", options(nomem, nostack)) };
    }
}

/// Program entry: defines `_start`, runs the user's `main`, exits with its
/// return value.
#[macro_export]
macro_rules! entry {
    ($main:path) => {
        #[unsafe(no_mangle)]
        pub unsafe extern "C" fn _start(arg: *const u8, arg_len: usize) -> ! {
            // SAFETY: the loader set rdi/rsi to the argument, or to zeros.
            unsafe { $crate::init_arg(arg, arg_len) };
            let main: fn() -> i32 = $main;
            $crate::exit(main() as u64)
        }
    };
}

/// User panic: report through the console (best effort), die with code 1.
#[panic_handler]
fn panic(_info: &PanicInfo) -> ! {
    write_console(b"[user task panicked]\n");
    exit(1)
}
