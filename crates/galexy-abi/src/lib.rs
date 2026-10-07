//! galexy-abi: the syscall ABI.
//!
//! The ONLY shared surface between the kernel and ring-3 programs. `no_std`,
//! zero dependencies, no platform assumptions in the data model (register
//! assignment lives with each arch's entry code, not here).
//!
//! Key decisions (locked in Milestone 12, before any userland existed):
//! - **Capabilities day one**: every syscall that touches a resource takes
//!   an opaque `Cap` handle instead of a magic number (no fds). The kernel
//!   validates rights on every call.
//! - **Numbered syscalls, machine-checked**: `SYSCALLS` lists the table in
//!   order; a host test asserts no duplicates and stable numbering, so
//!   renumbering early is safe and accidental reuse is impossible.
//! - **Errors as values**: syscalls return `SyscallResult` — a status code,
//!   not an exception mechanism. Kernel panics are unreachable from ring 3.

#![no_std]
#![deny(clippy::all)]
#![deny(missing_docs)]
#![deny(missing_debug_implementations)]

#[cfg(test)]
mod tests;

/// Marketing / banner version string for the OS (shell login screen, about).
pub const OS_VERSION: &str = "0.1.0";

/* ---------------- capabilities ---------------- */

/// An opaque capability handle. Ring 3 treats it as a black u64; the kernel
/// decodes `(index, rights)` and validates the rights on every use.
///
/// Layout (64 bits): lower 48 bits = kernel handle index, upper 16 bits =
/// caller-visible rights snapshot (informational — the authoritative rights
/// live kernel-side; userspace can only detect a stale handle faster).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(transparent)]
pub struct Cap(u64);

/// Capability rights (16-bit mask, upper half of the handle).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CapRights(u16);

impl CapRights {
    /// No rights at all (handle present, unusable).
    pub const NONE: Self = CapRights(0);
    /// May read the resource (`read`-style syscalls).
    pub const READ: Self = CapRights(1 << 0);
    /// May write the resource (`write`-style syscalls).
    pub const WRITE: Self = CapRights(1 << 1);
    /// May signal/notify the resource (IPC groundwork).
    pub const SIGNAL: Self = CapRights(1 << 2);
    /// May wait on the resource (IPC groundwork).
    pub const WAIT: Self = CapRights(1 << 3);
    /// May execute/spawn from the resource (loader groundwork).
    pub const EXEC: Self = CapRights(1 << 4);
    /// May shut the machine down or reset it.
    pub const POWER: Self = CapRights(1 << 5);
    /// The most rights a kernel-grade resource can hold.
    pub const ALL: Self = CapRights(0xFFFF);

    /// Builds a rights mask from raw bits.
    pub const fn from_bits(bits: u16) -> Self {
        CapRights(bits)
    }

    /// Raw bits (upper half of a `Cap` handle).
    pub const fn bits(self) -> u16 {
        self.0
    }

    /// Does `self` contain ALL of `needed`?
    pub const fn contains(self, needed: Self) -> bool {
        (self.0 & needed.0) == needed.0
    }

    /// Union of two masks.
    pub const fn union(self, other: Self) -> Self {
        CapRights(self.0 | other.0)
    }

    /// Rights present in both masks. A call is allowed only for this
    /// intersection of the kernel grant and the handle's snapshot.
    pub const fn intersection(self, other: Self) -> Self {
        CapRights(self.0 & other.0)
    }
}

impl Cap {
    /// Builds a handle from raw parts (kernel-space construction).
    pub const fn new(index: u64, rights: CapRights) -> Self {
        Cap((index & 0x0000_FFFF_FFFF_FFFF) | ((rights.bits() as u64) << 48))
    }

    /// The "unknown/absent" handle — never valid, never stored.
    pub const fn null() -> Self {
        Cap(0)
    }

    /// Kernel handle index (lower 48 bits).
    pub const fn index(self) -> u64 {
        self.0 & 0x0000_FFFF_FFFF_FFFF
    }

    /// Caller-visible rights snapshot (upper 16 bits).
    pub const fn rights(self) -> CapRights {
        CapRights::from_bits((self.0 >> 48) as u16)
    }

    /// Raw handle value (the only thing ring 3 should ever see).
    pub const fn bits(self) -> u64 {
        self.0
    }

    /// Rebuilds a handle from a raw u64 (invalidates nothing — the kernel
    /// validates the decoded parts on use; a stale handle is just 'stale').
    pub const fn from_bits(bits: u64) -> Self {
        Cap(bits)
    }
}

/// Reserved capability indexes — the first resources a booting system
/// always has. These index values are PERMANENT (capabilities day one
/// decision): changing them breaks every future user program.
pub mod reserved {
    use super::Cap;

    /// The text console: the first user-visible output resource. The shell
    /// itself goes through a `Cap` for the console (`write(sys_console, ...)`).
    pub const CONSOLE_INDEX: u64 = 1;
    /// The calling task itself (introspection, exit-equivalents).
    pub const SELF_INDEX: u64 = 2;

    /// The console capability, as handed to every booting task that has
    /// print rights.
    pub const fn console(rights: super::CapRights) -> Cap {
        Cap::new(CONSOLE_INDEX, rights)
    }

    /// The self capability.
    pub const fn self_cap() -> Cap {
        Cap::new(
            SELF_INDEX,
            super::CapRights::READ.union(super::CapRights::WRITE),
        )
    }

    /// The keyboard. Appended after file caps took the low indexes
    /// (`FILE_CAP_BASE` upward), so this lives in the high reserved band
    /// and is never a per-task file slot.
    pub const KEYBOARD_INDEX: u64 = 0x8000;
    /// The program loader. Same high band as the keyboard, next index.
    pub const LOADER_INDEX: u64 = 0x8001;
    /// Free frames and heap counters. `read` returns a fresh text snapshot.
    pub const STATS_INDEX: u64 = 0x8002;
    /// Cooperative-task counters. Same snapshot rule as [`STATS_INDEX`].
    pub const TASKS_INDEX: u64 = 0x8003;
    /// Running threads and their tick counts. Same snapshot rule as
    /// [`STATS_INDEX`].
    pub const THREADS_INDEX: u64 = 0x8004;

    /// The keyboard capability. `read` copies waiting keystrokes.
    pub const fn keyboard(rights: super::CapRights) -> Cap {
        Cap::new(KEYBOARD_INDEX, rights)
    }

    /// The loader capability. `spawn` starts a ramdisk program.
    pub const fn loader(rights: super::CapRights) -> Cap {
        Cap::new(LOADER_INDEX, rights)
    }

    /// The stats capability. `read` copies the current frame and heap report.
    pub const fn stats(rights: super::CapRights) -> Cap {
        Cap::new(STATS_INDEX, rights)
    }

    /// The tasks capability. `read` copies the cooperative-task report.
    pub const fn tasks(rights: super::CapRights) -> Cap {
        Cap::new(TASKS_INDEX, rights)
    }

    /// The threads capability. `read` copies the running-thread report.
    pub const fn threads(rights: super::CapRights) -> Cap {
        Cap::new(THREADS_INDEX, rights)
    }

    /// The power capability. `power` shuts down or resets the machine.
    pub const POWER_INDEX: u64 = 0x8005;

    /// The power capability.
    pub const fn power(rights: super::CapRights) -> Cap {
        Cap::new(POWER_INDEX, rights)
    }

    /// Ramdisk file names. `read` copies a fresh newline-separated list.
    /// Same snapshot rule as [`STATS_INDEX`].
    pub const FILES_INDEX: u64 = 0x8006;

    /// The files capability.
    pub const fn files(rights: super::CapRights) -> Cap {
        Cap::new(FILES_INDEX, rights)
    }
}

/// `power` operand: turn the machine off (ACPI S5).
pub const POWER_SHUTDOWN: u64 = 0;
/// `power` operand: reset the machine.
pub const POWER_REBOOT: u64 = 1;

/// `spawn` grant bit (`r10`): also give the new task the query caps.
///
/// The child always receives the console. Any other bit is rejected.
/// Keyboard, the loader, and power stay with the shell.
pub const SPAWN_GRANT_QUERY: u64 = 1;
/// `spawn` grant bit (`r10`): park the caller until the child exits
/// (not only until the ELF is loaded). Shell utilities use this so the
/// prompt returns after `ls` / `mkdir` finish.
pub const SPAWN_WAIT: u64 = 2;

/// `grant` rights (`RDX`): read the object.
pub const TOKEN_READ: u64 = 1;
/// `grant` rights: write the object's bytes.
pub const TOKEN_WRITE: u64 = 2;
/// `grant` rights: list the object's children (or see it in a listing).
pub const TOKEN_LIST: u64 = 4;
/// `grant` rights: create children under a directory.
pub const TOKEN_CREATE: u64 = 8;
/// `grant` rights: remove the object.
pub const TOKEN_REMOVE: u64 = 16;
/// Every galfs token right. Any other bit in `grant`'s `RDX` is `BadValue`.
pub const TOKEN_ALL: u64 = TOKEN_READ | TOKEN_WRITE | TOKEN_LIST | TOKEN_CREATE | TOKEN_REMOVE;

/// `seek` whence (`RDX`): set the cursor to `offset`.
pub const SEEK_SET: u64 = 0;
/// `seek` whence: move the cursor by `offset` from the current position.
pub const SEEK_CUR: u64 = 1;
/// `seek` whence: move the cursor by `offset` from the end of the file.
pub const SEEK_END: u64 = 2;

/// `user` op (`RDX`): write the calling task's actor name into a buffer.
pub const USER_WHOAMI: u64 = 0;
/// `user` op: write every actor name, one per line.
pub const USER_USERS: u64 = 1;
/// `user` op: create an actor and an empty Desktop (admin only).
pub const USER_ADD: u64 = 2;
/// `user` op: delete an empty actor (not admin; no live task on that root).
pub const USER_DEL: u64 = 3;
/// `user` op: switch this task via an access card (ALL on target root).
pub const USER_SU: u64 = 4;
/// `user` op: password login; `R8`/`R9` are the password bytes.
pub const USER_LOGIN: u64 = 5;
/// `user` op: set a password. `RDI`/`RSI` name the account (empty = self);
/// `R8`/`R9` are the new password.
pub const USER_PASSWD: u64 = 6;
/// `user` op: clear the caller's session (logged out / pre-login seat).
pub const USER_LOGOUT: u64 = 7;
/// `user` op: write the caller's galfs tokens (path + rights) into the buffer.
pub const USER_TOKENS: u64 = 8;
/// `user` op: write an actor's quota record into a buffer (see [`QUOTA_LEN`]).
/// `RDI`/`RSI` are the buffer; `R8`/`R9` name the actor (empty = self).
pub const USER_QUOTA: u64 = 9;
/// `user` op: set an actor's object/byte limits (admin only).
/// `RDI`/`RSI` name the actor; `R8 = max_objects`, `R9 = max_bytes`.
pub const USER_SETQUOTA: u64 = 10;

/// Bytes written by [`USER_QUOTA`]: four little-endian `u32` fields —
/// objects_used, objects_max, bytes_used, bytes_max.
pub const QUOTA_LEN: usize = 16;

/// Lowest capability index a per-task file open may return. `0` is null,
/// [`reserved::CONSOLE_INDEX`] is the console, [`reserved::SELF_INDEX`] is
/// the calling task. File indexes are per-task (not a global fd table):
/// task A's index 3 and task B's index 3 are different opens.
pub const FILE_CAP_BASE: u64 = 3;

/* ---------------- address-space contract ---------------- */

/// The fixed virtual load address for EVERY user program. Programs link
/// with their text at this base (`-Ttext`); the loader maps ELF segments
/// exactly at the phdrs' `p_vaddr`. Per-task address spaces (Step B) make
/// all programs sharing this base safe — every task sees its own image.
pub const USER_IMAGE_BASE: u64 = 0x0000_0C80_0000_0000;

/* ---------------- syscall table ---------------- */

/// The syscall table, in abi-number order.
///
/// BABY RULES (locked while there are no user programs to break):
/// 1. Numbers are assigned by position in this list — inserting a call means
///    appending (or bumping the ABI major version).
/// 2. Every entry documents its RAX number implicitly via `SYSCALLS`'s index
///    and its argument layout here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u64)]
pub enum Syscall {
    /// `exit(code)` — terminate the CALLING task immediately.
    ///
    /// Args: `RDI = exit code (u64)`. No return; the caller never resumes.
    Exit,
    /// `yield()` — voluntarily give up the rest of this scheduling quantum.
    ///
    /// Args: none. Returns: u64 status (0 = ok).
    Yield,
    /// `write(cap, addr, len)` — write bytes to a capability's output end.
    ///
    /// Args: `RDI = cap bits (u64)`, `RSI = user address`, `RDX = byte count`.
    /// Returns: `SyscallResult` (rax). Requires CapRights::WRITE.
    Write,
    /// `cap_info(cap)` — TEMPLATE/test syscall: return the handle's own
    /// bits back (proves the dispatch machinery; removed at ABI 1.0).
    ///
    /// Args: `RDI = cap bits`. Returns: `SyscallResult` (rax = cap bits).
    CapInfo,
    /// `open(name, len)` — open a ramdisk file by exact name.
    ///
    /// Args: `RDI = user address of the name`, `RSI = byte count`.
    /// Returns: `SyscallResult` (rax = new `Cap` bits, READ right).
    /// The cap is private to the calling task.
    Open,
    /// `read(cap, addr, len)` — copy bytes from a capability into a buffer.
    ///
    /// Args: `RDI = cap bits`, `RSI = user address`, `RDX = byte count`.
    /// Returns: `SyscallResult` (rax = bytes copied). A long request
    /// short-reads rather than failing. Requires CapRights::READ.
    ///
    /// On a file cap, `0` is end of file. On the keyboard cap, `0` means
    /// no keystroke is waiting (the queue does not end). On a query cap
    /// (stats, tasks, threads, files), each call returns a fresh snapshot, so
    /// `0` means the caller asked for no bytes. An empty files snapshot is
    /// still one newline, so a positive `len` does not come back as `0`.
    Read,
    /// `close(cap)` — drop a file capability opened by this task.
    ///
    /// Args: `RDI = cap bits`. Returns: `SyscallResult` (rax = 0).
    /// Reserved caps (console, self, keyboard, loader, stats, tasks,
    /// threads) are not files and fail `BadCap`.
    Close,
    /// `spawn(cap, name, len)` — start a ramdisk program.
    ///
    /// Args: `RDI = loader cap bits`, `RSI = user address of the name`,
    /// `RDX = byte count`. Optional: `R8 = user address of an argument`,
    /// `R9 = argument byte count` (at most 256; zero means none),
    /// `R10 = grant bits` ([`SPAWN_GRANT_QUERY`], [`SPAWN_WAIT`], or both).
    /// Returns: `SyscallResult` (rax = 0). Requires CapRights::EXEC on the
    /// loader cap. The load itself runs on the kernel's page table
    /// (main-loop drain). Without `SPAWN_WAIT` the caller is parked only
    /// until that load finishes and the child keeps running; with it the
    /// caller stays parked until the child exits.
    Spawn,
    /// `power(cap, op)` — shut down or reset the machine.
    ///
    /// Args: `RDI = power cap bits`, `RSI = op` ([`POWER_SHUTDOWN`] or
    /// [`POWER_REBOOT`]). Does not return when the platform honors it.
    /// Requires CapRights::POWER. A return means the machine stayed up.
    Power,
    /// `create(name, len, flags)` — create a scratch file or directory.
    ///
    /// Args: `RDI = user address of the path`, `RSI = byte count`,
    /// `RDX = flags` (`1` replaces an existing scratch file; any other
    /// value creates only when the name is new).
    /// Returns: `SyscallResult`. A file's rax is a READ|WRITE `Cap`. A
    /// directory (path ending in `/`) returns `0`.
    /// The bytes live in a fixed kernel table, not the ramdisk. A
    /// ramdisk name at `/`, or a name that already exists, is
    /// `Unsupported` (unless `RDX` is `1` and the name is a scratch
    /// file, which is emptied). A missing parent directory is
    /// `NotFound`. A full scratch table, or a full per-task file table,
    /// is `NoResource`.
    Create,
    /// `remove(name, len)` — delete a scratch file or an empty directory.
    ///
    /// Args: `RDI = user address of the path`, `RSI = byte count`.
    /// Returns: `SyscallResult` (rax = 0). The slot can be created again.
    /// A ramdisk name, or a directory that still has a child, is
    /// `Unsupported`. A missing path is `NotFound`. An open cap on a
    /// removed file becomes `BadCap`.
    Remove,
    /// `grant(path, len, rights, task, task_len)` — install a galfs token
    /// on a live user task.
    ///
    /// Args: `RDI = user address of the path`, `RSI = byte count`,
    /// `RDX = rights` ([`TOKEN_READ`] and friends; any other bit is
    /// `BadValue`), `R8 = user address of the target task name`,
    /// `R9 = target name byte count`. Returns: `SyscallResult` (rax = 0).
    /// The caller must already hold every right being granted on the
    /// resolved object (or an ancestor). A missing path or task is
    /// `NotFound`. A full token table on the target is `NoResource`.
    Grant,
    /// `revoke(path, len, rights, task, task_len)` — drop galfs token
    /// rights from a live user task.
    ///
    /// Same register layout as [`Syscall::Grant`]. Clears `rights` from
    /// the target's token that names the resolved object exactly. A
    /// zeroed token slot is freed. The caller must hold every bit being
    /// revoked. No matching token is `NotFound`.
    Revoke,
    /// `pipe(addr)` — create an anonymous pipe; write two caps into a
    /// 16-byte user buffer (`read` then `write`).
    ///
    /// Args: `RDI = user address of 16 bytes`. Returns: `SyscallResult`
    /// (rax = 0). The read cap has READ; the write cap has WRITE. A full
    /// pipe table or file table is `NoResource`.
    Pipe,
    /// `give(cap, task, task_len)` — move an open file/pipe cap to another
    /// live user task.
    ///
    /// Args: `RDI = cap bits`, `RSI = user address of the task name`,
    /// `RDX = name byte count`. Returns: `SyscallResult` (rax = the
    /// target's new `Cap` bits). The caller's slot is cleared. Reserved
    /// caps are `BadCap`. A missing task or a full target table is
    /// `NotFound` / `NoResource`.
    Give,
    /// `seek(cap, offset, whence)` — set the read cursor on an open file.
    ///
    /// Args: `RDI = cap bits`, `RSI = signed offset as u64 bits`,
    /// `RDX = whence` ([`SEEK_SET`], [`SEEK_CUR`], or [`SEEK_END`]).
    /// Returns: `SyscallResult` (rax = new offset). Archive and galfs
    /// files only; a pipe is `Unsupported`. Past-end seeks clamp to EOF.
    Seek,
    /// `user(addr, len, op)` — actor identity and account management.
    ///
    /// Args: `RDI`/`RSI` = name or buffer, `RDX` = op ([`USER_WHOAMI`],
    /// [`USER_USERS`], [`USER_ADD`], [`USER_DEL`], [`USER_SU`],
    /// [`USER_LOGIN`], [`USER_PASSWD`], [`USER_LOGOUT`], [`USER_TOKENS`],
    /// [`USER_QUOTA`], [`USER_SETQUOTA`]). Login/add/passwd take a
    /// password in `R8`/`R9`; quota get takes an optional actor name
    /// there; setquota takes max_objects/max_bytes. Whoami/users/tokens/
    /// quota write into the buffer. Passwords authenticate identity;
    /// tokens authorize object access (see `docs/AUTH.md`). Deleting
    /// admin, a non-empty tree, or an actor a live task still uses is
    /// `Unsupported` / `NotFound`.
    User,
    /// `rename(old, old_len, new, new_len)` — move a galfs dirent.
    ///
    /// Args: `RDI`/`RSI` = old path, `RDX`/`R8` = new path. Returns
    /// `SyscallResult` (rax = 0). Needs REMOVE on the source and CREATE
    /// on the destination parent. Same-actor or cross-directory; actor
    /// roots and ramdisk names are `Unsupported`. An existing destination
    /// is `Unsupported`. Moving a directory under itself is `BadValue`.
    Rename,
    /// `truncate(cap, size)` — set a galfs file's length.
    ///
    /// Args: `RDI = file cap bits`, `RSI = new size` (bytes). Returns
    /// `SyscallResult` (rax = 0). Requires WRITE. Shrinks free trailing
    /// blocks; grows with zeroed blocks up to the per-file max. Past-max
    /// or a non-galfs cap is `BadValue` / `Unsupported`. A full block
    /// pool is `NoResource`.
    Truncate,
    /// `stat(path, len, buf, buf_len)` — galfs metadata into a buffer.
    ///
    /// Args: `RDI`/`RSI` = path, `RDX` = user buffer address,
    /// `R8 = buffer length` (must be ≥ [`STAT_LEN`]). Returns
    /// `SyscallResult` (rax = [`STAT_LEN`]). Needs LIST on the object (or
    /// an ancestor). Layout: see [`STAT_LEN`] / `STAT_*` constants.
    Stat,
    /// `sync()` — flush the galfs dual-slot image when disk-backed.
    ///
    /// Args: none. Returns: `SyscallResult` (rax = 0). RAM-only is a
    /// successful no-op. A present but corrupt/locked volume is
    /// `Unsupported`. Mutates already sync; this is an explicit barrier.
    Sync,
    /// `share(path, len, rights, user, user_len)` — durable home share.
    ///
    /// Same register layout as [`Syscall::Grant`], but the target is an
    /// **actor name** (not a live task). The share is stored in the GALF
    /// image and re-applied at that actor's next login. Caller must hold
    /// every right being shared. A full share table is `NoResource`.
    Share,
    /// `unshare(path, len, rights, user, user_len)` — clear durable rights.
    ///
    /// Same layout as [`Syscall::Share`]. Removes `rights` from the
    /// matching share; an empty share slot is freed.
    Unshare,
}

/// The ABI's syscall list (index = number). Length is capped at 64 while
/// there is no ABI versioning story (lifting the cap is version-1 work).
pub const SYSCALLS: [Syscall; 23] = [
    Syscall::Exit,
    Syscall::Yield,
    Syscall::Write,
    Syscall::CapInfo,
    Syscall::Open,
    Syscall::Read,
    Syscall::Close,
    Syscall::Spawn,
    Syscall::Power,
    Syscall::Create,
    Syscall::Remove,
    Syscall::Grant,
    Syscall::Revoke,
    Syscall::Pipe,
    Syscall::Give,
    Syscall::Seek,
    Syscall::User,
    Syscall::Rename,
    Syscall::Truncate,
    Syscall::Stat,
    Syscall::Sync,
    Syscall::Share,
    Syscall::Unshare,
];

/// `stat` kind: regular file.
pub const STAT_FILE: u8 = 1;
/// `stat` kind: directory.
pub const STAT_DIR: u8 = 2;
/// Bytes written by [`Syscall::Stat`].
///
/// Layout (little-endian):
/// - `0`: kind ([`STAT_FILE`] / [`STAT_DIR`])
/// - `1`: token rights the caller holds (`TOKEN_*` mask)
/// - `2..4`: reserved
/// - `4..8`: size (`u32`) — file length; `0` for directories
/// - `8`: owner name length
/// - `9..41`: owner name bytes (padded)
/// - `41..48`: reserved
pub const STAT_LEN: usize = 48;

/// Maximum syscall number (upper bound for a u64 dispatch table).
pub const MAX_SYSCALL: u64 = SYSCALLS.len() as u64 - 1;

/* ---------------- results & errors ---------------- */

/// A syscall's return value: OK carries one u64, Err carries one code.
///
/// Mirrored into registers by each arch's entry/exit shim — RAX = code,
/// RDX = payload (single register, no struct packing at this ABI level).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SyscallResult {
    /// `true` = success (`rdx` = payload); `false` = failure (`rdx` = code).
    pub ok: bool,
    /// Payload (success) or error code (failure).
    pub value: u64,
}

impl SyscallResult {
    /// Success with a payload.
    pub const fn ok(value: u64) -> Self {
        SyscallResult { ok: true, value }
    }

    /// Failure with an error code.
    pub const fn err(err: SysError) -> Self {
        SyscallResult {
            ok: false,
            value: err as u64,
        }
    }

    /// Splits into a Rust Result (host-side ergonomics).
    pub const fn to_result(self) -> Result<u64, SysError> {
        if self.ok {
            Result::Ok(self.value)
        } else {
            Result::Err(SysError::from_code(self.value))
        }
    }
}

/// Error codes. Values are PERMANENT once ring 3 exists (they are the ABI).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u64)]
pub enum SysError {
    /// The capability handle is unknown to the kernel (revoked or never
    /// existed — treat as consumed, do not retry).
    BadCap = 1,
    /// The capability exists but lacks the right this call needs.
    AccessDenied = 2,
    /// A user-supplied buffer crosses unmapped or kernel-owned memory.
    BadBuffer = 3,
    /// Not implemented at this ABI level (capability-shaped stubs only).
    Unsupported = 4,
    /// Invalid argument value outside any handle/buffer concern.
    BadValue = 5,
    /// `open` found no ramdisk file with that exact name.
    NotFound = 6,
    /// A fixed kernel slot this call needs is already taken (the task's
    /// file table, the object table, the single queued spawn, or a live
    /// task that already uses the requested spawn name).
    NoResource = 7,
}

impl SysError {
    /// Round-trips a code from a `SyscallResult` value.
    pub const fn from_code(code: u64) -> Self {
        match code {
            1 => SysError::BadCap,
            2 => SysError::AccessDenied,
            3 => SysError::BadBuffer,
            4 => SysError::Unsupported,
            5 => SysError::BadValue,
            6 => SysError::NotFound,
            7 => SysError::NoResource,
            _ => SysError::Unsupported,
        }
    }
}
