//! Thread table, reaping, and the per-CPU pin.
//!
//! Constructing a thread (kernel or ring 3) lives here. Queued `spawn`,
//! file syscalls, and parked I/O live in the sibling modules. Names the
//! rest of the kernel already calls through `sched::` are re-exported
//! from `sched/mod.rs`.

use super::iowait::*;
use super::spawn::*;
use super::task::*;

use super::{channel, context, galfs, pipe};
use crate::arch::mm;
use crate::serial_println;
use crate::sync::Mutex;
use alloc::boxed::Box;
use alloc::vec;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicU8, AtomicUsize, Ordering};
use galexy_abi::{Cap, CapRights, SysError};
use x86_64::instructions::interrupts;
use x86_64::structures::paging::{Mapper, Page, PageTableFlags, PhysFrame, Size4KiB};
use x86_64::{PhysAddr, VirtAddr};

/* ---------------- preemptive threads ---------------- */

/// Per-thread kernel stack size.
pub(crate) const THREAD_STACK_SIZE: usize = 32 * 1024;

/// 16-byte-aligned buffer (FXSAVE requires it).
#[repr(align(16))]
pub(in crate::sched) struct FxArea(
    // Storage targeted by raw pointer in FXSAVE/FXRSTOR — never read by name.
    #[allow(dead_code)] [u8; context::FX_AREA_SIZE],
);

impl FxArea {
    pub(in crate::sched) const fn new() -> Self {
        FxArea([0; context::FX_AREA_SIZE])
    }
}

/// Thread lifecycle states (AtomicU8 values).
///
/// Tombstone design: slots are NEVER removed from [`THREADS`] — removal
/// would shift indexes used by the timer switch (`current`, `last_served`)
/// and corrupt rotation state mid-flight. A `Freed` slot is handed out
/// again once no CPU is current on it and its switch-out tail has
/// published [`CTX_STABLE`]. The index stays put; only the record changes.
pub(in crate::sched) const STATE_RUNNING: u8 = 0;
pub(in crate::sched) const STATE_EXITED: u8 = 1; // returned from its entry; reaped by the main loop
pub(in crate::sched) const STATE_FREED: u8 = 2; // stack + fx freed; rotation-skipped until reused
pub(in crate::sched) const STATE_WAITING: u8 = 3; // parked: spawn/wait/sleep/I/O until an event

/// Bytes kept for a thread's name. Spawn already rejects a longer name.
pub(in crate::sched) const NAME_CAP: usize = 64;

/// Process Caps per task (matches [`galexy_abi::MAX_PROC_CAPS`]).
pub(in crate::sched) const MAX_PROC_CAPS: usize = galexy_abi::MAX_PROC_CAPS as usize;
const _: () = assert!(MAX_PROC_CAPS == 16);

/// One process Cap entry: child thread slot + generation + rights.
#[derive(Clone, Copy)]
pub(in crate::sched) struct ProcHandle {
    /// 1-based index into [`THREADS`].
    pub(in crate::sched) child_slot: u8,
    /// Must match the child's `cap_gen` or the Cap is stale.
    pub(in crate::sched) gen: u32,
    pub(in crate::sched) rights: CapRights,
}

/// Monotonic debug id (listings / serial only — never an open-by-id key).
pub(in crate::sched) static NEXT_DEBUG_ID: AtomicU64 = AtomicU64::new(1);
/// Bumped on login, `su`, and logout. Audits quote it; it is not a Cap.
pub(in crate::sched) static NEXT_SESSION: AtomicU64 = AtomicU64::new(1);
/// Test kernels shrink the idle window. `0` means [`IDLE_LOGOUT_MS`].
pub(in crate::sched) static IDLE_LIMIT_OVERRIDE: AtomicU64 = AtomicU64::new(0);
/// Logged-in console with no keystrokes for this long is logged out.
/// Same clock as lockout (`timer_ticks`, ~1 ms).
pub const IDLE_LOGOUT_MS: u64 = 60_000;

/// Init's control channel. `0xFF` until init's first `channel` call.
pub(in crate::sched) static INIT_CTRL: AtomicU8 = AtomicU8::new(0xFF);
/// `wait_proc_index` when the park is not a consuming Cap-wait.
pub(in crate::sched) const WAIT_PROC_NONE: u8 = 0xFF;

/// Magic word painted at the very bottom of each thread's stack (lowest
/// address). A stack that overflows far enough to corrupt the heap walks
/// downward through this word first — reaping detects the clobber.
pub(in crate::sched) const STACK_CANARY: u64 = 0x0CA7_AB1E_500D_F00D;

/// Open ramdisk files one task may hold at once. The table is carved into
/// the `Thread` at spawn so `open` never allocates (the syscall runs IF=0;
/// a heap grow there would broadcast a shootdown that targets must ack).
pub(in crate::sched) const MAX_OPEN_FILES: usize = 8;

/// Where an open file's bytes live. The per-task slot only keeps the cursor.
#[derive(Clone, Copy)]
pub(crate) enum FileBody {
    /// Immutable archive bytes. The slice lives in the ramdisk.
    Archive(&'static [u8]),
    /// Index into [`galfs`] object table. The bytes are writable.
    Galfs(u16),
    /// Anonymous pipe end.
    Pipe { id: u8, end: pipe::PipeEnd },
    /// Capability-channel endpoint (`0` or `1`).
    Channel { id: u8, end: u8 },
}

/// One open file. Archive bytes live in the bootloader's ramdisk; galfs
/// bytes live in the global table. Only the cursor is per-open.
#[derive(Clone, Copy)]
pub(crate) struct OpenFile {
    pub(in crate::sched) body: FileBody,
    pub(in crate::sched) offset: usize,
    /// Authoritative rights. The handle's upper half is a snapshot; a call
    /// is allowed only for the intersection of the two.
    pub(in crate::sched) rights: CapRights,
}

/// Rights the launcher recorded on a task. A fabricated cap index is not
/// enough: the matching bit has to be set here.
#[derive(Clone, Copy)]
pub(crate) struct Grants {
    pub(in crate::sched) console: bool,
    pub(in crate::sched) keyboard: bool,
    pub(in crate::sched) loader: bool,
    /// `stats`, `tasks`, `threads`, and `ls`.
    pub(in crate::sched) query: bool,
    pub(in crate::sched) power: bool,
}

/// One reserved service a syscall may require.
pub(crate) enum Grant {
    Console,
    Keyboard,
    Loader,
    Query,
    Power,
}

impl Grants {
    pub(crate) const fn none() -> Self {
        Self {
            console: false,
            keyboard: false,
            loader: false,
            query: false,
            power: false,
        }
    }

    /// A program the user started. It can print, and nothing else.
    pub(crate) const fn console() -> Self {
        Self {
            console: true,
            keyboard: false,
            loader: false,
            query: false,
            power: false,
        }
    }

    /// Console, plus the query caps (`ls`, `rm`).
    pub(crate) const fn console_query() -> Self {
        Self {
            console: true,
            keyboard: false,
            loader: false,
            query: true,
            power: false,
        }
    }

    /// Console and keyboard for a program the shell Cap-waits (`nano`).
    ///
    /// `query` is included when the spawn asked for the files snapshot.
    /// The loader and power stay with the parent.
    pub(crate) const fn console_keyboard(query: bool) -> Self {
        Self {
            console: true,
            keyboard: true,
            loader: false,
            query,
            power: false,
        }
    }

    /// The interactive shell: console, keyboard, loader, queries, power.
    pub(crate) const fn launcher() -> Self {
        Self {
            console: true,
            keyboard: true,
            loader: true,
            query: true,
            power: true,
        }
    }

    /// Userspace init (Milestone 53): loader + console + query + power.
    /// No keyboard — init does not prompt for passwords.
    pub(crate) const fn init() -> Self {
        Self {
            console: true,
            keyboard: false,
            loader: true,
            query: true,
            power: true,
        }
    }

    /// Logged-in non-admin seat: console, keyboard, loader, queries.
    pub(crate) const fn session() -> Self {
        Self {
            console: true,
            keyboard: true,
            loader: true,
            query: true,
            power: false,
        }
    }

    /// Pre-login seat: console + keyboard only (`login` / `help`).
    pub(crate) const fn pre_login() -> Self {
        Self {
            console: true,
            keyboard: true,
            loader: false,
            query: false,
            power: false,
        }
    }

    pub(in crate::sched) fn allows(self, grant: Grant) -> bool {
        match grant {
            Grant::Console => self.console,
            Grant::Keyboard => self.keyboard,
            Grant::Loader => self.loader,
            Grant::Query => self.query,
            Grant::Power => self.power,
        }
    }
}

pub(in crate::sched) struct Thread {
    /// Display name. Copied at spawn so the caller's buffer can go away.
    pub(in crate::sched) name_bytes: [u8; NAME_CAP],
    pub(in crate::sched) name_len: u8,
    /// Lifecycle state (see STATE_* consts).
    pub(in crate::sched) state: AtomicU8,
    /// Saved context pointer; valid while the thread is NOT running.
    pub(in crate::sched) ctx: AtomicU64,
    /// Timer ticks charged to this thread (CPU-time attribution).
    pub(in crate::sched) ticks: AtomicU64,
    /// FXSAVE area — freed by the reaper once the thread exits.
    pub(in crate::sched) fx: *mut FxArea,
    /// `true` for ring-3 tasks: the fabricated context runs in user mode
    /// and preemption pushes onto `kstack` via TSS.RSP0.
    pub(in crate::sched) is_user: bool,
    /// Kernel threads: the 32 KiB mode+context stack (heap-backed), canary
    /// painted at the bottom. User tasks: EMPTY (their context lives on
    /// mapped user-space pages).
    pub(in crate::sched) stack: Vec<u8>,
    /// User tasks only: the kernel-mode stack for ring 3→0 transitions
    /// (TSS.RSP0 target). Empty for kernel threads.
    pub(in crate::sched) kstack: Vec<u8>,
    /// User tasks only: aligned top of `kstack` (the RSP0 value).
    pub(in crate::sched) kstack_top: u64,
    /// The task's page-table root (physical address). `0` = the kernel's
    /// table (Step A: every task shares it; Step B: only kernel threads —
    /// user tasks get a FreshL4 at spawn).
    pub(in crate::sched) cr3: AtomicU64,
    /// User tasks only: the P4 entry index of their region in their own
    /// tree (the reaper's tree walk needs it).
    pub(in crate::sched) user_p4: u16,
    /// The CPU that owns (runs + reaps) this thread — "pinned at spawn"
    /// (SMP M18); work stealing (M19) may flip it to an idle CPU.
    pub(in crate::sched) owner: u8,
    /// The timer tick of this thread's last steal (anti-ping-pong cooldown
    /// for the idle-CPU steal path). 0 = never stolen (eligible).
    pub(in crate::sched) stolen_at: AtomicU64,
    /// File capabilities belonging to this task. Empty for kernel threads.
    /// Indexes are [`galexy_abi::FILE_CAP_BASE`] + slot. Cleared on reap.
    pub(in crate::sched) files: [Option<OpenFile>; MAX_OPEN_FILES],
    /// Process Caps (children). Indexes are [`PROC_CAP_BASE`] + slot.
    pub(in crate::sched) procs: [Option<ProcHandle>; MAX_PROC_CAPS],
    /// Idle stealing skips this thread. The interactive shell is resident
    /// on the BSP: the keyboard and the framebuffer have one consumer.
    pub(in crate::sched) no_steal: bool,
    /// When `STATE_WAITING`: 1-based child slot to wake on (0 = wait for
    /// pending spawn load only, or a sleep / I/O wait). Cap-wait uses this
    /// instead of a name.
    pub(in crate::sched) wait_child_slot: AtomicU8,
    /// Process-cap table index held across a [`task_wait`] park.
    /// `0xFF` means this wait did not take a cap (`SPAWN_WAIT` keeps it).
    /// Exit delivery clears that slot; an init control wake leaves it.
    pub(in crate::sched) wait_proc_index: AtomicU8,
    /// Child `cap_gen` captured when [`task_wait`] parked.
    pub(in crate::sched) wait_child_gen: AtomicU32,
    /// When waiting on a child: true = wake on exit; false = wake on load.
    pub(in crate::sched) wait_for_exit: AtomicBool,
    /// Absolute `timer_ticks` deadline for [`Syscall::Sleep`]. `0` means
    /// this wait is not a sleep (spawn/Cap-wait/I/O).
    pub(in crate::sched) sleep_deadline: AtomicU64,
    /// I/O wait kind while `STATE_WAITING` (Milestone 57): `0` none,
    /// `1` keyboard, `2` pipe read, `3` pipe write.
    pub(in crate::sched) io_kind: AtomicU8,
    /// Pipe id when `io_kind` is pipe read/write.
    pub(in crate::sched) io_pipe: AtomicU8,
    /// User buffer address for a parked I/O syscall.
    pub(in crate::sched) io_addr: AtomicU64,
    /// User buffer length for a parked I/O syscall.
    pub(in crate::sched) io_len: AtomicU32,
    /// Cap bits for the parked I/O syscall (file/keyboard).
    pub(in crate::sched) io_cap: AtomicU64,
    /// Exit status stamped on [`STATE_EXITED`] (read by Cap-wait).
    pub(in crate::sched) exit_code: AtomicU64,
    /// Bumped when the slot is reaped/reused so old process Caps fail.
    pub(in crate::sched) cap_gen: AtomicU32,
    /// Monotonic debug id for `tasks` / serial (not a handle).
    pub(in crate::sched) debug_id: u64,
    /// 1-based parent slot; `0` = kernel-spawned root.
    pub(in crate::sched) parent_slot: u8,
    /// Milestone 53: orphan-root / first ring-3 supervisor. At most one.
    pub(in crate::sched) is_init: bool,
    /// Set when a Cap-wait (or `SPAWN_WAIT`) has collected the exit code.
    pub(in crate::sched) exit_waited: AtomicBool,
    /// Reserved services this task may call. Set at spawn, never grown.
    pub(in crate::sched) grants: Grants,
    /// Console this task writes, and whose keyboard queue it reads.
    /// Inherited from the task that spawned it. F1 is 0.
    pub(in crate::sched) tty: u8,
    /// Actor root this task walks from when a path has no `owner@`.
    pub(in crate::sched) fs_root: u16,
    /// Tokens that authorize galfs paths. Utilities inherit; bare spawns do not.
    pub(in crate::sched) fs_tokens: [galfs::Token; galfs::TOKEN_SLOTS],
    /// Set when the task was created as admin. Survives [`task_su`] so the
    /// seat can return to admin after switching to another actor.
    pub(in crate::sched) born_admin: bool,
    /// Default admin password is still in force. Mutating galfs syscalls fail
    /// until `passwd`. Shell-only gates cannot skip this.
    pub(in crate::sched) must_change: bool,
    /// Login/logout generation for audits (not a handle).
    pub(in crate::sched) session_gen: u64,
    /// `timer_ticks` of the last key delivered to this TTY, or 0 if none.
    pub(in crate::sched) last_input_tick: u64,
    /// Timer tick when [`console_budget_used`] was last reset.
    pub(in crate::sched) console_budget_tick: u64,
    /// Console bytes written during [`console_budget_tick`].
    pub(in crate::sched) console_budget_used: u32,
    /// Heap pages this task has `map`ped (Milestone 66). Reap frees them
    /// with the rest of the user tree.
    pub(in crate::sched) heap_pages: u16,
    /// Second user pointer for a parked channel `recv` (cap-out buffer).
    pub(in crate::sched) io_extra: AtomicU64,
}

impl Thread {
    pub(in crate::sched) fn name(&self) -> &str {
        let n = self.name_len as usize;
        core::str::from_utf8(&self.name_bytes[..n]).unwrap_or("")
    }
}

/// Copies `name` into a fixed buffer, stopping on a char boundary at 64.
pub(in crate::sched) fn pack_name(name: &str) -> ([u8; NAME_CAP], u8) {
    let mut bytes = [0u8; NAME_CAP];
    let mut n = 0;
    for ch in name.chars() {
        let len = ch.len_utf8();
        if n + len > NAME_CAP {
            break;
        }
        ch.encode_utf8(&mut bytes[n..]);
        n += len;
    }
    (bytes, n as u8)
}

// SAFETY: `fx` is an exclusively-owned allocation, dereferenced only by the
// single-core timer switch under the IRQ gate; `stack` likewise is only
// freed from main-loop context.
unsafe impl Send for Thread {}

/// All preemptive threads, in round-robin order. Touched by the main loop
/// and the timer handler — access is IRQ-gated (see lock audit). Slots stay
/// in the vec (indexes must not shift); a freed record can be overwritten.
pub(in crate::sched) static THREADS: Mutex<Vec<Thread>> = Mutex::new(Vec::new());

/// Per-CPU rotation state (SMP, M18): each CPU keeps its OWN round-robin —
/// the global rotation statics would double-enter a task the moment two
/// naked timer ticks coincided. INDEX = the CPU's logical index
/// (`cpu::current_index()`); a CPU NEVER locks another's slot. Recycling a
/// freed thread only loads every CPU's `current` atomic, so a slot some
/// CPU is still inside is not overwritten.
///
/// Slot 0 = "this CPU's main":
/// - on the BSP that is the shell main loop,
/// - on an AP the idle/reap loop.
pub(in crate::sched) struct CpuSched {
    /// 0 = this CPU's main is current; otherwise a thread index + 1
    /// (only threads OWNED by this CPU may ever be current elsewhere).
    pub(in crate::sched) current: AtomicUsize,
    /// This CPU's round-robin cursor (usize::MAX = none yet).
    pub(in crate::sched) last_served: AtomicUsize,
    /// This CPU's main saved context pointer (0 = not yet saved).
    pub(in crate::sched) main_ctx: AtomicU64,
    /// This CPU's main FXSAVE area.
    pub(in crate::sched) main_fx: Mutex<FxArea>,
    /// CPU ticks charged to THIS CPU's main.
    pub(in crate::sched) main_ticks: AtomicU64,
}

impl CpuSched {
    pub(in crate::sched) const fn new() -> Self {
        Self {
            current: AtomicUsize::new(0),
            last_served: AtomicUsize::new(usize::MAX),
            main_ctx: AtomicU64::new(0),
            main_fx: Mutex::new(FxArea::new()),
            main_ticks: AtomicU64::new(0),
        }
    }
}

pub(in crate::sched) static CPU_SCHED: [CpuSched; crate::arch::cpu::MAX_CPUS] =
    [const { CpuSched::new() }; crate::arch::cpu::MAX_CPUS];

/// This CPU's rotation state. Per-CPU ownership (fenced by IRQ gating in
/// every user); NEVER locks CPU_SCHED[i] from a foreign CPU.
pub(in crate::sched) fn cpu_sched() -> &'static CpuSched {
    &CPU_SCHED[crate::arch::cpu::current_index()]
}
/// The BSP's main ticks (shell-side accounting; the status bar and tests
/// use this — AP idles are off-graph).
pub fn main_ticks() -> u64 {
    CPU_SCHED[0].main_ticks.load(Ordering::Relaxed)
}
/// Where the next spawned thread/task lands: round-robin across the CPUs
/// the MADT brought online ("pinned at spawn"; no migration, no stealing).
pub(in crate::sched) static NEXT_CPU: AtomicUsize = AtomicUsize::new(0);
/// Completed work-steals (diagnostics + test assertions for the steal
/// proof).
pub(in crate::sched) static STEALS: AtomicU64 = AtomicU64::new(0);
/// A freshly stolen thread cannot be stolen again for this many timer
/// ticks (~0.1 s machine time): idle CPUs must not ping-pong a hot task
/// between them.
pub(in crate::sched) const STEAL_COOLDOWN_TICKS: u64 = 100;

/// Live slots. A freed record is reusable, so this caps threads that still
/// occupy a slot (running, waiting, exited, or not yet safe to recycle).
pub(in crate::sched) const MAX_THREADS: usize = 64;
/// Per-slot "saved context is idle" flag. Index = thread slot − 1.
///
/// `true`: no CPU is still unwinding a frame on this thread's stack.
/// Cleared when a CPU commits to entering the thread; set from the naked
/// switch tail AFTER `mov rsp` (see gs:[40] / `departed_slot`).
///
/// Load-bearing for work stealing. `current != slot` is stored before the
/// lock drops, and the victim's tail still runs on the thread stack after
/// that. One guest tick does not cover a host-starved victim vCPU — the
/// stealer must wait until this flag says the tail has actually finished.
pub(crate) static CTX_STABLE: [AtomicBool; MAX_THREADS] =
    [const { AtomicBool::new(true) }; MAX_THREADS];

const _: () = assert!(core::mem::size_of::<AtomicBool>() == 1);

/// A freed slot may be overwritten when its stacks are already gone, no
/// CPU still names it as `current`, and the switch-out tail has published
/// [`CTX_STABLE`]. `current` is an atomic load only — this does not lock
/// another CPU's rotation state.
pub(in crate::sched) fn slot_reusable(threads: &[Thread], index: usize) -> bool {
    if threads[index].state.load(Ordering::Acquire) != STATE_FREED {
        return false;
    }
    if !CTX_STABLE[index].load(Ordering::Acquire) {
        return false;
    }
    let slot = index + 1;
    !CPU_SCHED
        .iter()
        .any(|cpu| cpu.current.load(Ordering::Acquire) == slot)
}

/// Registers a thread. A freed slot is overwritten in place; otherwise the
/// vec grows. Indexes of live threads do not move. Returns the 1-based slot.
///
/// The vec growth allocates. That allocation can shoot down TLBs, and the
/// other CPU's timer takes [`THREADS`] with interrupts off. Holding the
/// lock across the growth deadlocks that ack (`echo | grep` in the
/// userland typing test). Capacity is reserved with the lock dropped.
pub(in crate::sched) fn push_thread(thread: Thread) -> u8 {
    let mut incoming = Some(thread);
    loop {
        if let Some(slot) = try_push_thread(&mut incoming) {
            return slot;
        }
        reserve_thread_slot();
    }
}

/// Inserts the pending thread when a freed slot or spare capacity exists.
/// `None` means the vec must grow first; `incoming` is left in place.
pub(in crate::sched) fn try_push_thread(incoming: &mut Option<Thread>) -> Option<u8> {
    interrupts::without_interrupts(|| {
        let mut threads = THREADS.lock();
        let reusable = (0..threads.len()).find(|&i| slot_reusable(&threads, i));
        let spare = threads.capacity() > threads.len();
        if reusable.is_none() && !spare {
            assert!(
                threads.len() < MAX_THREADS,
                "sched: thread table full ({MAX_THREADS} live slots)"
            );
            return None;
        }
        let thread = incoming.take().expect("push_thread: missing thread");
        if let Some(index) = reusable {
            // False until this thread's owner publishes a switch-out. Stored
            // before the record becomes RUNNING, so a steal scan cannot take
            // the slot on its first run.
            CTX_STABLE[index].store(false, Ordering::Release);
            threads[index] = thread;
            return Some((index + 1) as u8);
        }
        CTX_STABLE[threads.len()].store(false, Ordering::Release);
        threads.push(thread);
        Some(threads.len() as u8)
    })
}

/// Grows the thread vec by a few slots. The heap allocation runs without
/// [`THREADS`] held, so a shootdown can be acked by the other CPU.
pub(in crate::sched) fn reserve_thread_slot() {
    let want = interrupts::without_interrupts(|| {
        let threads = THREADS.lock();
        threads.len().saturating_add(4).max(4)
    });
    let mut buf = Vec::with_capacity(want);
    interrupts::without_interrupts(|| {
        let mut threads = THREADS.lock();
        if threads.capacity() > threads.len() || buf.capacity() < threads.len() + 1 {
            return;
        }
        let mut old = core::mem::take(&mut *threads);
        buf.append(&mut old);
        *threads = buf;
    });
}

/// Incoming `slot` (1-based; 0 = main) is about to be entered, so its saved
/// context is not stealable until this CPU switches off it.
pub(in crate::sched) fn claim_incoming(slot: usize) {
    if slot != 0 {
        CTX_STABLE[slot - 1].store(false, Ordering::Release);
    }
}

/// Number of completed work-steals since boot.
pub fn steal_count() -> u64 {
    STEALS.load(Ordering::Relaxed)
}

pub(in crate::sched) fn next_cpu() -> u8 {
    let online = crate::arch::cpu::online();
    (NEXT_CPU.fetch_add(1, Ordering::Relaxed) % online) as u8
}

/// Installs the CR3 for the task being entered (switch-in hook, callers are
/// under the IRQ gate). `cr3_addr == 0` = kernel table.
///
/// Safe-by-construction: every task table shares the kernel half of the
/// boot table (the FreshL4 contract), so the kernel structures the switch
/// machinery touches (locks, fx areas, stacks) remain mapped across the
/// swap.
pub(in crate::sched) fn enter_task_cr3(cr3_addr: u64) {
    let frame = if cr3_addr == 0 {
        mm::kernel_cr3()
    } else {
        // SAFETY: the address came from a real FreshL4 allocation (aligned
        // 4 KiB frame base).
        PhysFrame::from_start_address(PhysAddr::new(cr3_addr))
            .expect("sched: corrupt task CR3 address")
    };
    mm::install_cr3(frame);
}

/// Marks the CURRENT thread as exited. Called by the trampoline when a
/// thread's entry returns — the thread keeps executing until the next timer
/// tick (the rotation then skips it forever; the main loop's [`reap`]
/// frees its stack + fx area).
///
/// Must run on a thread's own stack — calling from the main loop is a bug
/// (panics; main has no Thread slot to tombstone).
pub fn thread_exit() {
    let slot = cpu_sched().current.load(Ordering::Relaxed);
    assert!(slot != 0, "thread_exit: called from the main loop");
    interrupts::without_interrupts(|| {
        let threads = THREADS.lock();
        threads[slot - 1]
            .state
            .store(STATE_EXITED, Ordering::Release);
        serial_println!("[sched] thread '{}' exited", threads[slot - 1].name());
    });
}

/// Frees resources of every EXITED thread OWNED BY THIS CPU (stack Vec +
/// FXSAVE area) and leaves `Freed` tombstones in place. Per-CPU reap (SMP
/// M18): cross-CPU reaping would free a stack while a zombie still parks on
/// it on its owner; ownership makes "the thread stopped" a same-CPU fact.
/// Called from main-loop/idle context (BSP shell + AP idle); IRQ-gated per
/// the lock-audit rule.
///
/// Safety net on the way: if a thread's stack canary was clobbered (stack
/// overflow deep enough to leave its Vec), reaping panics loudly instead of
/// silently returning corrupted heap blocks to the allocator.
pub fn reap() {
    interrupts::without_interrupts(|| {
        let mut threads = THREADS.lock();
        let my_cpu = crate::arch::cpu::current_index() as u8;
        let mut freed = 0usize;
        let n = threads.len();
        for i in 0..n {
            if threads[i].owner != my_cpu {
                continue; // another CPU's thread — its reaper owns it
            }
            if threads[i]
                .state
                .compare_exchange(
                    STATE_EXITED,
                    STATE_FREED,
                    Ordering::AcqRel,
                    Ordering::Relaxed,
                )
                .is_err()
            {
                continue; // running or already freed
            }
            // Zombie until Cap-wait (or no Cap holder remains). Kernel roots
            // (`parent_slot == 0`) and already-waited tasks reap immediately.
            let child_slot = (i + 1) as u8;
            let waited = threads[i].exit_waited.load(Ordering::Acquire);
            let kernel_root = threads[i].parent_slot == 0;
            let gen = threads[i].cap_gen.load(Ordering::Acquire);
            let held = !waited
                && !kernel_root
                && threads.iter().enumerate().any(|(hi, holder)| {
                    hi != i
                        && holder.procs.iter().any(|p| {
                            p.map(|h| h.child_slot == child_slot && h.gen == gen)
                                .unwrap_or(false)
                        })
                });
            if held {
                threads[i].state.store(STATE_EXITED, Ordering::Release);
                continue;
            }
            // Canary check BEFORE the stack is freed: a deep overflow writes
            // the magic word last (stack grows downward, canary is at the
            // very bottom). User tasks: heap check applies to `kstack`
            // instead (their `stack` is empty; the user stack has no heap
            // canary — it's isolated pages).
            let canary_stack: *const u8 = if threads[i].is_user {
                threads[i].kstack.as_ptr()
            } else {
                threads[i].stack.as_ptr()
            };
            // SAFETY: `canary_stack` is the bottom of the thread's heap
            // stack, still owned by this slot, 8 bytes long.
            let canary = unsafe { (canary_stack as *const u64).read_unaligned() };
            if canary != STACK_CANARY {
                panic!(
                    "reap: stack canary corrupted for thread '{}' (stack overflow)",
                    threads[i].name()
                );
            }
            // Milestone 53: move wait/control Caps for live children to
            // init; reparent. Without init, children become kernel roots
            // and Caps die with this task (legacy test path).
            let dead_slot = (i + 1) as u8;
            transfer_orphans_to_init(&mut threads, dead_slot, i);
            // File/process caps die with the task. Pipe and channel ends
            // are closed so a peer is not stuck and the tables return to
            // empty. Bump gen so foreign Caps fail.
            for slot in threads[i].files.iter_mut() {
                if let Some(file) = slot.take() {
                    release_file(file);
                }
            }
            threads[i].procs = [None; MAX_PROC_CAPS];
            threads[i].cap_gen.fetch_add(1, Ordering::AcqRel);
            threads[i].exit_waited.store(false, Ordering::Relaxed);
            threads[i].parent_slot = 0;
            // SAFETY: the fx area was leaked at spawn; its slot is a
            // tombstone now — no code will dereference it again.
            unsafe { drop(Box::from_raw(threads[i].fx)) };
            threads[i].fx = core::ptr::null_mut();
            // The saved context lives ON this stack; null it so any stray
            // reader fails loudly instead of jumping into freed memory.
            threads[i].ctx.store(0, Ordering::Relaxed);
            // User tasks: their ENTIRE tree is reclaimed by a walk under
            // the task's own P4 entry (page-table frames AND data frames —
            // the unmap-per-page pass is gone; the tree is not CR3-active
            // here: tombstoned ⇒ the handoff/switch already moved CR3).
            let task_cr3 = threads[i].cr3.swap(0, Ordering::AcqRel);
            let user_p4 = threads[i].user_p4;
            let mut name_raw = [0u8; NAME_CAP];
            let name_len = threads[i].name_len as usize;
            name_raw[..name_len].copy_from_slice(&threads[i].name_bytes[..name_len]);
            let name = core::str::from_utf8(&name_raw[..name_len]).unwrap_or("");
            if task_cr3 != 0 {
                // SAFETY: the address came from a real FreshL4 allocation.
                let root = PhysFrame::from_start_address(PhysAddr::new(task_cr3))
                    .expect("reap: corrupt task CR3");
                let count = mm::free_user_tree(root, user_p4);
                serial_println!("[sched] freed task '{}' tree: {} frame(s)", name, count);
            }
            // Wipe heap stacks before return so a later alloc cannot read
            // leftover syscall frames / secrets (user tree frames are wiped
            // in `deallocate_frame` during `free_user_tree`).
            let mut stack = core::mem::take(&mut threads[i].stack);
            stack.fill(0);
            drop(stack);
            let mut kstack = core::mem::take(&mut threads[i].kstack);
            kstack.fill(0);
            drop(kstack);
            freed += 1;
        }
        drop(threads);
        if freed > 0 {
            serial_println!("[sched] reaped {} thread stack(s)", freed);
            // Peers parked on a pipe or channel this task still held.
            for id in 0..pipe::PIPE_SLOTS {
                wake_pipe_waiters(id as u8);
            }
            for id in 0..channel::CHAN_SLOTS {
                wake_channel_waiters(id as u8);
            }
        }
    });
}

/// User-task registration seam for the loader (crate-internal): the
/// loader computes everything; the scheduler owns Thread construction
/// (fx area, canary, state).
pub(crate) struct TaskInit<'a> {
    pub(crate) name: &'a str,
    /// Fabricated initial context pointer (on the task's user stack).
    pub(crate) ctx: u64,
    /// Kernel-mode stack (heap-backed); the canary is painted here.
    pub(crate) kstack: Vec<u8>,
    /// Aligned top of `kstack` (the RSP0 value).
    pub(crate) kstack_top: u64,
    /// The task's page-table root (physical address; nonzero).
    pub(crate) cr3: u64,
    /// The task's P4 entry index inside its tree.
    pub(crate) user_p4: u16,
    /// `Some` pins the owner CPU. `None` round-robins via [`next_cpu`].
    pub(crate) owner: Option<u8>,
    /// Idle stealing must leave this task on its spawn CPU.
    pub(crate) no_steal: bool,
    /// Reserved services this program may call.
    pub(crate) grants: Grants,
    /// Console the new task writes. A child inherits its parent's.
    pub(crate) tty: u8,
    /// galfs credentials. Shells get admin's root token.
    pub(crate) fs: galfs::FsCred,
    /// 1-based parent slot; `0` = kernel.
    pub(crate) parent_slot: u8,
    /// Milestone 53 orphan root.
    pub(crate) is_init: bool,
    /// Stay `WAITING` until [`release_deferred`]. Spawn uses this so file
    /// Caps move in before the child's first instruction.
    pub(crate) defer_run: bool,
}

pub(crate) fn register_user_task(init: TaskInit<'_>) -> u8 {
    interrupts::without_interrupts(|| {
        // Canary at the very bottom of the kernel-mode stack.
        let mut kstack = init.kstack;
        kstack[..8].copy_from_slice(&STACK_CANARY.to_le_bytes());
        let fx = Box::into_raw(Box::new(FxArea::new()));
        let owner = init.owner.unwrap_or_else(next_cpu);
        let (name_bytes, name_len) = pack_name(init.name);
        let slot = push_thread(Thread {
            name_bytes,
            name_len,
            state: AtomicU8::new(if init.defer_run {
                STATE_WAITING
            } else {
                STATE_RUNNING
            }),
            ctx: AtomicU64::new(init.ctx),
            ticks: AtomicU64::new(0),
            fx,
            is_user: true,
            stack: Vec::new(),
            kstack,
            kstack_top: init.kstack_top,
            cr3: AtomicU64::new(init.cr3),
            user_p4: init.user_p4,
            owner,
            stolen_at: AtomicU64::new(0),
            files: [None; MAX_OPEN_FILES],
            procs: [None; MAX_PROC_CAPS],
            no_steal: init.no_steal,
            wait_child_slot: AtomicU8::new(0),
            wait_proc_index: AtomicU8::new(WAIT_PROC_NONE),
            wait_child_gen: AtomicU32::new(0),
            wait_for_exit: AtomicBool::new(false),
            sleep_deadline: AtomicU64::new(0),
            io_kind: AtomicU8::new(0),
            io_pipe: AtomicU8::new(0),
            io_addr: AtomicU64::new(0),
            io_len: AtomicU32::new(0),
            io_cap: AtomicU64::new(0),
            exit_code: AtomicU64::new(0),
            cap_gen: AtomicU32::new(1),
            debug_id: NEXT_DEBUG_ID.fetch_add(1, Ordering::Relaxed),
            parent_slot: init.parent_slot,
            is_init: init.is_init,
            exit_waited: AtomicBool::new(false),
            grants: init.grants,
            tty: init.tty,
            fs_root: init.fs.root,
            fs_tokens: init.fs.tokens,
            born_admin: galfs::is_admin_root(init.fs.root),
            must_change: false,
            session_gen: 0,
            last_input_tick: 0,
            console_budget_tick: 0,
            console_budget_used: 0,
            heap_pages: 0,
            io_extra: AtomicU64::new(0),
        });
        // The record is RUNNING before the poke. An idle owner otherwise
        // stays in `hlt` until its tickless deadline (up to a second).
        // A deferred child stays WAITING until `release_deferred` moves
        // Caps in; poking now would run it with an empty file table.
        if !init.defer_run {
            poke_owner(owner);
        }
        slot
    })
}

/// Spawns a preemptive kernel thread running `entry` (which parks if it
/// returns). Allocates + maps the thread stack; IRQ-gated while registering.
/// Spawns the thread; returns its owner CPU (the pin decision).
pub fn spawn_thread(name: &str, entry: extern "C" fn()) -> u8 {
    interrupts::without_interrupts(|| {
        // Zero pages straight into the heap (no big stack temp).
        let mut stack = vec![0u8; THREAD_STACK_SIZE];
        // Canary at the very bottom of the stack (lowest address) — the
        // first word a deep downward overflow would clobber.
        let canary_bytes = STACK_CANARY.to_le_bytes();
        stack[..8].copy_from_slice(&canary_bytes);
        // Round the stack top down to 16 bytes (SSE alignment).
        let top = (stack.as_ptr() as u64 + stack.len() as u64) & !0xF;
        let (cs, ss) = context::kernel_cs_ss();
        // SAFETY: `top` is the 16-byte-aligned top of the fresh heap stack.
        let ctx = unsafe { context::init_stack(top, entry, cs, ss) };
        let fx = Box::into_raw(Box::new(FxArea::new()));
        let owner = next_cpu();
        let (name_bytes, name_len) = pack_name(name);
        let _slot = push_thread(Thread {
            name_bytes,
            name_len,
            state: AtomicU8::new(STATE_RUNNING),
            ctx: AtomicU64::new(ctx),
            ticks: AtomicU64::new(0),
            fx,
            is_user: false,
            stack,
            kstack: Vec::new(),
            kstack_top: 0,
            cr3: AtomicU64::new(0),
            user_p4: 0,
            owner,
            stolen_at: AtomicU64::new(0),
            files: [None; MAX_OPEN_FILES],
            procs: [None; MAX_PROC_CAPS],
            no_steal: false,
            wait_child_slot: AtomicU8::new(0),
            wait_proc_index: AtomicU8::new(WAIT_PROC_NONE),
            wait_child_gen: AtomicU32::new(0),
            wait_for_exit: AtomicBool::new(false),
            sleep_deadline: AtomicU64::new(0),
            io_kind: AtomicU8::new(0),
            io_pipe: AtomicU8::new(0),
            io_addr: AtomicU64::new(0),
            io_len: AtomicU32::new(0),
            io_cap: AtomicU64::new(0),
            exit_code: AtomicU64::new(0),
            cap_gen: AtomicU32::new(1),
            debug_id: NEXT_DEBUG_ID.fetch_add(1, Ordering::Relaxed),
            parent_slot: 0,
            is_init: false,
            exit_waited: AtomicBool::new(false),
            grants: Grants::none(),
            tty: 0,
            fs_root: galfs::NO_OBJECT,
            fs_tokens: [galfs::Token::empty(); galfs::TOKEN_SLOTS],
            born_admin: false,
            must_change: false,
            session_gen: 0,
            last_input_tick: 0,
            console_budget_tick: 0,
            console_budget_used: 0,
            heap_pages: 0,
            io_extra: AtomicU64::new(0),
        });
        serial_println!("[sched] thread '{}' ready (owner cpu {})", name, owner);
        let _ = _slot;
        poke_owner(owner);
        owner
    })
}

/// User stack size in 4 KiB pages.
pub(crate) const USER_STACK_PAGES: usize = 4;

/// Soft floor of free frames required before a user `spawn` / blob load.
///
/// Covers one task's FreshL4, stack, scratch, code pages, and page-table
/// growth, plus headroom so the rest of the kernel can still allocate.
/// Below this, spawn returns `NoResource` instead of panicking mid-map.
pub const SPAWN_FRAME_RESERVE: usize = 64;

/// True when the frame pool can absorb another user-task spawn.
pub fn spawn_frames_available() -> bool {
    crate::arch::mm::free_frames() >= SPAWN_FRAME_RESERVE
}
/// User stack offset inside the task's P4 region (1 GiB in — keeps the
/// code page and stack far apart; the region is 512 GiB).
pub(crate) const USER_STACK_OFFSET: u64 = 1 << 30;
// The GUARD fence is an ABSENCE: the page directly below the user stack is
// left unmapped (nothing maps it, nothing needs to). A stack walking past
// its region faults in ring 3, and the page-fault path tombstones the
// task — silent corruption becomes a clean kill.

/// Result of a user-task spawn: the addresses ring-3 code was granted plus
/// the scratch page's PHYSICAL address (kernel-side pollers read through
/// the phys map — the task tree is not active from the kernel's context).
#[derive(Debug, Clone, Copy)]
pub struct UserRegion {
    /// Code page base (RIP entry point of the task) — task-private space.
    pub code: VirtAddr,
    /// RW scratch page in task-private space.
    pub scratch: VirtAddr,
    /// Physical address of the scratch page's backing frame (kernel poll).
    pub scratch_phys: PhysAddr,
}

/// Spawns a ring-3 task with ITS OWN address space: a `FreshL4` cloned
/// from the kernel's table (kernel half shared verbatim, user half empty).
/// `build` receives the granted addresses and returns the code bytes to
/// map (≤ one page).
///
/// Layout per task (task-private tree): one free P4 entry `N` scanned
/// top-down BELOW 256, code at `(N<<39) + 0`, user stack at `+1 GiB`,
/// scratch page right above the stack. The task's own kernel-mode stack
/// (heap) serves its ring 3→0 crossings via TSS.RSP0. IRQ-gated.
pub fn spawn_user_task(name: &str, build: impl FnOnce(UserRegion) -> Vec<u8>) -> (UserRegion, u8) {
    spawn_user_with(name, galfs::admin_cred(), build)
}

/// Like [`spawn_user_task`], pinned to `cpu`.
///
/// Test kernels that peek a task's scratch page after it exits must own the
/// reap: an AP owner reaps (and zero-wipes) the tree from its idle loop the
/// moment the task exits, racing the BSP's peek (`test-treechurn`).
pub fn spawn_user_task_on(
    name: &str,
    cpu: u8,
    build: impl FnOnce(UserRegion) -> Vec<u8>,
) -> (UserRegion, u8) {
    spawn_user_with_grants(
        name,
        galfs::admin_cred(),
        Grants::console(),
        build,
        Some(cpu),
    )
}

/// Like [`spawn_user_task`], with shell-grade grants (loader, queries, …).
///
/// Pinned to the BSP so the BIOS harness can peek scratch before any
/// remote reap wipes DONE marks (`test-procgive`, Cap batteries).
pub fn spawn_user_launcher(
    name: &str,
    build: impl FnOnce(UserRegion) -> Vec<u8>,
) -> (UserRegion, u8) {
    spawn_user_with_grants(
        name,
        galfs::admin_cred(),
        Grants::launcher(),
        build,
        Some(0),
    )
}

/// Like [`spawn_user_launcher`], with explicit galfs credentials — a
/// logged-out seat is `galfs::unauth_cred()` (`test-negative`).
pub fn spawn_user_launcher_with(
    name: &str,
    fs: galfs::FsCred,
    build: impl FnOnce(UserRegion) -> Vec<u8>,
) -> (UserRegion, u8) {
    spawn_user_with_grants(name, fs, Grants::launcher(), build, Some(0))
}

/// Like [`spawn_user_task`], with explicit galfs credentials (token tests).
pub fn spawn_user_with(
    name: &str,
    fs: galfs::FsCred,
    build: impl FnOnce(UserRegion) -> Vec<u8>,
) -> (UserRegion, u8) {
    spawn_user_with_grants(name, fs, Grants::console(), build, None)
}

/// Like [`spawn_user_with`], with an explicit grant set.
///
/// `owner = None` → pin-at-spawn round-robin; `Some(cpu)` forces that CPU.
pub(crate) fn spawn_user_with_grants(
    name: &str,
    fs: galfs::FsCred,
    grants: Grants,
    build: impl FnOnce(UserRegion) -> Vec<u8>,
    owner: Option<u8>,
) -> (UserRegion, u8) {
    interrupts::without_interrupts(|| {
        // The loader allocates. A syscall runs with interrupts off, so the
        // load stays on the main loop, which is the kernel table.
        assert!(
            mm::on_kernel_tree(),
            "spawn_user_task: must run on the kernel tree (main-loop context)"
        );
        assert!(
            spawn_frames_available(),
            "spawn_user_task: free frames {} < SPAWN_FRAME_RESERVE {}",
            mm::free_frames(),
            SPAWN_FRAME_RESERVE
        );
        let fresh = mm::FreshL4::new().expect("no frame for a fresh task table");
        let root = fresh.frame;
        let p4_index =
            mm::top_user_p4_index_in(root).expect("no free user P4 entry in the fresh tree");
        let region = VirtAddr::new((p4_index as u64) << 39);

        // Data frames first (so `build` can embed the scratch's physical
        // address for kernel-side polling).
        let code_frame = mm::allocate_frame().expect("no frame for user code");
        let mut stack_frames: [PhysFrame<Size4KiB>; USER_STACK_PAGES] =
            [PhysFrame::from_start_address(PhysAddr::new(0)).unwrap(); USER_STACK_PAGES];
        for slot in &mut stack_frames {
            let frame = mm::allocate_frame().expect("no frame for user stack");
            *slot = frame;
        }
        let scratch_frame = mm::allocate_frame().expect("no frame for user scratch");

        let stack_base = region + USER_STACK_OFFSET;
        let scratch = stack_base + (USER_STACK_PAGES * 4096) as u64 + 4096;
        let granted = UserRegion {
            code: region,
            scratch,
            scratch_phys: scratch_frame.start_address(),
        };
        let code = build(granted);

        // Code grows into a single page: the blob ABI.
        assert!(
            code.len() <= 4096,
            "spawn_user_task: blob exceeds one page ({} bytes)",
            code.len()
        );

        // SAFETY: allocator-owned frames (exclusive access per contract);
        // write through the phys map, no task-tree activation needed.
        unsafe {
            core::ptr::copy_nonoverlapping(
                code.as_ptr(),
                mm::frame_virt(code_frame.start_address()).as_mut_ptr::<u8>(),
                code.len(),
            );
            // Scratch page zeroed: consumers poll for the first non-zero
            // write; allocator frames may carry stale bytes.
            core::ptr::write_bytes(
                mm::frame_virt(scratch_frame.start_address()).as_mut_ptr::<u64>(),
                0,
                512,
            );
        }

        // Map everything INTO THE TASK'S OWN TREE (it is not CR3-active —
        // no TLB flushes needed; mapping machinery allocates the task's
        // own P3/P2/P1 frames below the entry).
        // SAFETY: a FreshL4 root: coherent, freshly cloned, not active.
        unsafe {
            mm::with_table(root, |mapper| {
                // Code: RX (PRESENT|USER, never WRITABLE). Stack/scratch: RW|NX.
                let code_page = Page::containing_address(region);
                mapper
                    .map_to(
                        code_page,
                        code_frame,
                        PageTableFlags::PRESENT | PageTableFlags::USER_ACCESSIBLE,
                        &mut mm::TaskFrameAlloc,
                    )
                    .expect("user code map failed")
                    .flush();
                for (i, frame) in stack_frames.iter().enumerate() {
                    let page = Page::containing_address(stack_base + (i * 4096) as u64);
                    mapper
                        .map_to(
                            page,
                            *frame,
                            PageTableFlags::PRESENT
                                | PageTableFlags::WRITABLE
                                | PageTableFlags::USER_ACCESSIBLE
                                | PageTableFlags::NO_EXECUTE,
                            &mut mm::TaskFrameAlloc,
                        )
                        .expect("user stack map failed")
                        .flush();
                }
                let scratch_page = Page::containing_address(scratch);
                mapper
                    .map_to(
                        scratch_page,
                        scratch_frame,
                        PageTableFlags::PRESENT
                            | PageTableFlags::WRITABLE
                            | PageTableFlags::USER_ACCESSIBLE
                            | PageTableFlags::NO_EXECUTE,
                        &mut mm::TaskFrameAlloc,
                    )
                    .expect("user scratch map failed")
                    .flush();
            });
        }

        // Initial ring-3 frame: fabricated through the phys-map image of
        // the TOP stack page — fab = image end of the top page = the exact
        // stack_top of the task's user stack; the builder writes downward.
        let stack_top = (stack_base + (USER_STACK_PAGES * 4096) as u64).as_u64() & !0xF;
        debug_assert!(stack_top.is_multiple_of(4096));
        let fab_vaddr = mm::frame_virt(stack_frames[USER_STACK_PAGES - 1].start_address()) + 4096;
        let (cs, ss) = context::user_cs_ss();
        // SAFETY: `fab_vaddr` is the phys-map image of the fresh top stack
        // page; the entry RIP is the task's own code page.
        let ctx = unsafe {
            context::init_user_frame(
                fab_vaddr.as_u64(),
                stack_top - 512, // user RSP (user-space address!)
                region.as_u64(),
                0,
                0,
                cs,
                ss,
            )
        };

        // Kernel-mode stack for ring 3→0 crossings: heap-backed, 32 KiB,
        // canary at the bottom.
        let mut kstack = vec![0u8; THREAD_STACK_SIZE];
        kstack[..8].copy_from_slice(&STACK_CANARY.to_le_bytes());
        let kstack_top = (kstack.as_ptr() as u64 + kstack.len() as u64) & !0xF;

        let fx = Box::into_raw(Box::new(FxArea::new()));
        let owner = owner.unwrap_or_else(next_cpu);
        let (name_bytes, name_len) = pack_name(name);
        let _slot = push_thread(Thread {
            name_bytes,
            name_len,
            state: AtomicU8::new(STATE_RUNNING),
            ctx: AtomicU64::new(ctx),
            ticks: AtomicU64::new(0),
            fx,
            is_user: true,
            stack: Vec::new(),
            kstack,
            kstack_top,
            cr3: AtomicU64::new(root.start_address().as_u64()),
            user_p4: p4_index,
            owner,
            stolen_at: AtomicU64::new(0),
            files: [None; MAX_OPEN_FILES],
            procs: [None; MAX_PROC_CAPS],
            // Hand-rolled test blobs coordinate via yield races; keep them
            // on their spawn CPU so idle steal cannot starve a waiter.
            no_steal: true,
            wait_child_slot: AtomicU8::new(0),
            wait_proc_index: AtomicU8::new(WAIT_PROC_NONE),
            wait_child_gen: AtomicU32::new(0),
            wait_for_exit: AtomicBool::new(false),
            sleep_deadline: AtomicU64::new(0),
            io_kind: AtomicU8::new(0),
            io_pipe: AtomicU8::new(0),
            io_addr: AtomicU64::new(0),
            io_len: AtomicU32::new(0),
            io_cap: AtomicU64::new(0),
            exit_code: AtomicU64::new(0),
            cap_gen: AtomicU32::new(1),
            debug_id: NEXT_DEBUG_ID.fetch_add(1, Ordering::Relaxed),
            parent_slot: 0,
            is_init: false,
            exit_waited: AtomicBool::new(false),
            grants,
            tty: 0,
            fs_root: fs.root,
            fs_tokens: fs.tokens,
            born_admin: galfs::is_admin_root(fs.root),
            must_change: false,
            session_gen: 0,
            last_input_tick: 0,
            console_budget_tick: 0,
            console_budget_used: 0,
            heap_pages: 0,
            io_extra: AtomicU64::new(0),
        });
        serial_println!(
            "[sched] user task '{}' ready (own tree cr3={:#x}, p4={}, code @ {:#x}, kstack top {:#x})",
            name,
            root.start_address().as_u64(),
            p4_index,
            region.as_u64(),
            kstack_top
        );
        let _ = _slot;
        poke_owner(owner);
        (granted, owner)
    })
}

/// Threads that still hold their stacks and address spaces: running, or
/// exited but not yet reaped. Reap is owner-local, so a drain on the BSP
/// must wait for this rather than [`threads_count`] — a stolen task goes
/// `EXITED` on the other CPU before that CPU's idle loop frees it.
pub fn unreaped_threads() -> usize {
    interrupts::without_interrupts(|| {
        THREADS
            .lock()
            .iter()
            .filter(|t| {
                let state = t.state.load(Ordering::Acquire);
                state == STATE_RUNNING || state == STATE_EXITED
            })
            .count()
    })
}

/// Number of live (running) preemptive threads.
pub fn threads_count() -> usize {
    interrupts::without_interrupts(|| {
        THREADS
            .lock()
            .iter()
            .filter(|t| t.state.load(Ordering::Relaxed) == STATE_RUNNING)
            .count()
    })
}

/// True while a task named `name` is RUNNING or WAITING (sleep / Cap-wait /
/// spawn park). False once it has exited (or never existed).
pub fn is_name_live(name: &str) -> bool {
    interrupts::without_interrupts(|| {
        THREADS.lock().iter().any(|t| {
            let state = t.state.load(Ordering::Relaxed);
            t.name() == name && (state == STATE_RUNNING || state == STATE_WAITING)
        })
    })
}

/// The owner CPU of the thread named `name` ("pinned at spawn" — SMP M18);
/// `None` when no RUNNING thread by that name exists.
pub fn thread_owner(name: &str) -> Option<u8> {
    interrupts::without_interrupts(|| {
        THREADS.lock().iter().find_map(|t| {
            (t.name() == name && t.state.load(Ordering::Relaxed) == STATE_RUNNING)
                .then_some(t.owner)
        })
    })
}

/// Sum of CPU-time ticks across every thread ever (post-mortem liveness of
/// the per-CPU timer machine: a thread serviced anywhere accumulated >0).
pub fn thread_tick_total() -> u64 {
    interrupts::without_interrupts(|| {
        THREADS
            .lock()
            .iter()
            .map(|t| t.ticks.load(Ordering::Relaxed))
            .sum()
    })
}

/// Calls `each` with every RUNNING thread's debug id, name, and tick count.
///
/// The name is borrowed from the slot and is only valid inside `each`.
/// No allocation: the syscall path renders query text into a stack buffer
/// and must not grow the heap (a grow there broadcasts a shootdown).
pub(crate) fn for_running_threads(mut each: impl FnMut(u64, &str, u64)) {
    interrupts::without_interrupts(|| {
        for thread in THREADS.lock().iter() {
            if thread.state.load(Ordering::Relaxed) == STATE_RUNNING {
                each(
                    thread.debug_id,
                    thread.name(),
                    thread.ticks.load(Ordering::Relaxed),
                );
            }
        }
    });
}

/// Calls `each` for every non-freed user task: debug id, name, state label.
pub(crate) fn for_user_tasks(mut each: impl FnMut(u64, &str, &str)) {
    interrupts::without_interrupts(|| {
        for thread in THREADS.lock().iter() {
            if !thread.is_user {
                continue;
            }
            let state = match thread.state.load(Ordering::Relaxed) {
                STATE_RUNNING => "running",
                STATE_WAITING => "waiting",
                STATE_EXITED => "exited",
                _ => continue,
            };
            each(thread.debug_id, thread.name(), state);
        }
    });
}

/// Writes the current task's inspect line into `dst` (`id=… name=… state=…\n`).
pub(crate) fn task_self_inspect(dst: &mut [u8]) -> Result<usize, SysError> {
    let slot = current_slot();
    if slot == 0 {
        return Err(SysError::BadCap);
    }
    interrupts::without_interrupts(|| {
        let threads = THREADS.lock();
        let thread = threads.get(slot - 1).ok_or(SysError::BadCap)?;
        if !thread.is_user {
            return Err(SysError::BadCap);
        }
        let state = match thread.state.load(Ordering::Relaxed) {
            STATE_RUNNING => "running",
            STATE_WAITING => "waiting",
            STATE_EXITED => "exited",
            _ => "unknown",
        };
        Ok(format_inspect_line(
            dst,
            thread.debug_id,
            thread.name(),
            state,
        ))
    })
}

/// Inspect a child named by a process Cap (`PROC_INSPECT`).
pub(crate) fn task_proc_inspect(cap: Cap, dst: &mut [u8]) -> Result<usize, SysError> {
    if !cap.rights().contains(CapRights::PROC_INSPECT) {
        return Err(SysError::AccessDenied);
    }
    let pi = proc_slot(cap)?;
    let slot = current_slot();
    if slot == 0 {
        return Err(SysError::BadCap);
    }
    interrupts::without_interrupts(|| {
        let threads = THREADS.lock();
        let handle =
            threads.get(slot - 1).ok_or(SysError::BadCap)?.procs[pi].ok_or(SysError::BadCap)?;
        if !handle.rights.contains(CapRights::PROC_INSPECT) {
            return Err(SysError::AccessDenied);
        }
        let ci = handle.child_slot as usize;
        if ci == 0 || ci > threads.len() {
            return Err(SysError::BadCap);
        }
        let child = &threads[ci - 1];
        if child.cap_gen.load(Ordering::Acquire) != handle.gen {
            return Err(SysError::BadCap);
        }
        let state = match child.state.load(Ordering::Acquire) {
            STATE_RUNNING => "running",
            STATE_WAITING => "waiting",
            STATE_EXITED => "exited",
            STATE_FREED => "freed",
            _ => "unknown",
        };
        Ok(format_inspect_line(
            dst,
            child.debug_id,
            child.name(),
            state,
        ))
    })
}

/// `id=<n> name=<label> state=<s>\n` into `dst`; returns bytes written.
pub(in crate::sched) fn format_inspect_line(
    dst: &mut [u8],
    id: u64,
    name: &str,
    state: &str,
) -> usize {
    let mut n = 0usize;
    let mut push = |bytes: &[u8]| {
        let take = bytes.len().min(dst.len().saturating_sub(n));
        dst[n..n + take].copy_from_slice(&bytes[..take]);
        n += take;
    };
    push(b"id=");
    let mut tmp = [0u8; 20];
    let mut i = tmp.len();
    let mut v = id;
    if v == 0 {
        push(b"0");
    } else {
        while v > 0 {
            i -= 1;
            tmp[i] = b'0' + (v % 10) as u8;
            v /= 10;
        }
        push(&tmp[i..]);
    }
    push(b" name=");
    push(name.as_bytes());
    push(b" state=");
    push(state.as_bytes());
    push(b"\n");
    n
}

/// `(name, ticks)` for every RUNNING thread. The name is copied so the
/// status bar can format it after the table lock drops.
///
/// Nothing allocates while `THREADS` is held: the names land in a
/// fixed-size stack snapshot under the lock, and the `String`s are built
/// after it drops. This is the 1 Hz status-bar path; an allocation that
/// grew the heap under `THREADS` is what wedged `passwd` (galexy.os#86).
/// The lock relax now acks shootdowns regardless, but the hot path should
/// not depend on the safety net.
pub fn thread_stats() -> alloc::vec::Vec<(alloc::string::String, u64)> {
    let mut snap: [([u8; NAME_CAP], u8, u64); MAX_THREADS] = [([0; NAME_CAP], 0, 0); MAX_THREADS];
    let count = interrupts::without_interrupts(|| {
        let threads = THREADS.lock();
        let mut n = 0;
        for t in threads
            .iter()
            .filter(|t| t.state.load(Ordering::Relaxed) == STATE_RUNNING)
        {
            if n == snap.len() {
                break;
            }
            snap[n] = (t.name_bytes, t.name_len, t.ticks.load(Ordering::Relaxed));
            n += 1;
        }
        n
    });
    snap[..count]
        .iter()
        .map(|(bytes, len, ticks)| {
            let name = core::str::from_utf8(&bytes[..*len as usize]).unwrap_or("");
            (alloc::string::String::from(name), *ticks)
        })
        .collect()
}

/// Is any RUNNING thread registered under `name`?
///
/// Foreground-job query for the shell's launch prompt pacing: the pending
/// program's exit (syscall tombstone) flips it to false within one gate.
pub fn is_name_running(name: &str) -> bool {
    interrupts::without_interrupts(|| {
        THREADS
            .lock()
            .iter()
            .any(|t| t.state.load(Ordering::Relaxed) == STATE_RUNNING && t.name() == name)
    })
}

/// Parent slot (1-based; `0` = kernel) for a RUNNING or WAITING task named
/// `name`. Integration tests use this to assert orphan reparenting.
pub fn parent_slot_of(name: &str) -> Option<u8> {
    interrupts::without_interrupts(|| {
        THREADS.lock().iter().find_map(|t| {
            let state = t.state.load(Ordering::Acquire);
            if (state == STATE_RUNNING || state == STATE_WAITING) && t.name() == name {
                Some(t.parent_slot)
            } else {
                None
            }
        })
    })
}

/// 1-based thread slot for a RUNNING or WAITING task named `name`.
pub fn slot_of_name(name: &str) -> Option<u8> {
    interrupts::without_interrupts(|| {
        THREADS.lock().iter().enumerate().find_map(|(i, t)| {
            let state = t.state.load(Ordering::Acquire);
            if (state == STATE_RUNNING || state == STATE_WAITING) && t.name() == name {
                Some((i + 1) as u8)
            } else {
                None
            }
        })
    })
}

// (main_ticks moved into the per-CPU table above.)
