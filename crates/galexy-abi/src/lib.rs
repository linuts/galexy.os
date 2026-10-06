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
}

/// `power` operand: turn the machine off (ACPI S5).
pub const POWER_SHUTDOWN: u64 = 0;
/// `power` operand: reset the machine.
pub const POWER_REBOOT: u64 = 1;

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
    /// (stats, tasks, threads), each call returns a fresh snapshot, so
    /// `0` means the caller asked for no bytes.
    Read,
    /// `close(cap)` — drop a file capability opened by this task.
    ///
    /// Args: `RDI = cap bits`. Returns: `SyscallResult` (rax = 0).
    /// Reserved caps (console, self, keyboard, loader, stats, tasks,
    /// threads) are not files and fail `BadCap`.
    Close,
    /// `spawn(cap, name, len)` — start a ramdisk program and wait until it
    /// exits.
    ///
    /// Args: `RDI = loader cap bits`, `RSI = user address of the name`,
    /// `RDX = byte count`. Returns: `SyscallResult` (rax = 0) after the
    /// program has exited. Requires CapRights::EXEC on the loader cap.
    /// The load itself runs on the kernel's page table (main-loop drain);
    /// the caller is parked until then.
    Spawn,
    /// `power(cap, op)` — shut down or reset the machine.
    ///
    /// Args: `RDI = power cap bits`, `RSI = op` ([`POWER_SHUTDOWN`] or
    /// [`POWER_REBOOT`]). Does not return when the platform honors it.
    /// Requires CapRights::POWER. A return means the machine stayed up.
    Power,
}

/// The ABI's syscall list (index = number). Length is capped at 64 while
/// there is no ABI versioning story (fixing the cap is version-1 work).
pub const SYSCALLS: [Syscall; 9] = [
    Syscall::Exit,
    Syscall::Yield,
    Syscall::Write,
    Syscall::CapInfo,
    Syscall::Open,
    Syscall::Read,
    Syscall::Close,
    Syscall::Spawn,
    Syscall::Power,
];

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
    /// file table, or the single queued program spawn).
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
