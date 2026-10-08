//! Scheduling: cooperative tasks AND timer-preemptive kernel threads.
//!
//! Two models, layered:
//! - **Cooperative tasks** (`spawn`/`run_once` below): state machines
//!   stepped from the main loop, round-robin via a heap-backed queue.
//! - **Preemptive threads** (`spawn_thread` + `sched/context.rs`): stackful
//!   kernel threads switched by the PIT timer handler (naked asm swaps full
//!   CPU contexts). The only preemptor is the timer IRQ — hence the lock
//!   audit rule in `docs/DESIGN.md`: *locks held by preemptable code must be
//!   held with interrupts off*.

pub mod context;
pub mod demo;
pub mod galfs;
pub mod loader;
pub mod pipe;
pub mod ramdisk;
pub mod syscalls;

use alloc::boxed::Box;
use alloc::collections::VecDeque;
use alloc::vec;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicU8, AtomicUsize, Ordering};
use spin::Mutex;
use x86_64::instructions::interrupts;
use x86_64::structures::paging::{Mapper, Page, PageTableFlags, PhysFrame, Size4KiB};
use x86_64::{PhysAddr, VirtAddr};

use galexy_abi::{Cap, CapRights, SysError, SyscallResult, PROC_CAP_BASE};

use crate::arch::mm;
use crate::serial_println;

/* ---------------- cooperative tasks ---------------- */

/// What a task wants after one step.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskStatus {
    /// Run another step later.
    Yield,
    /// Finished; remove from the queue.
    Done,
}

/// Per-task scratch state for the step function's state machine.
#[derive(Debug, Default)]
pub struct TaskCtx {
    /// Eight 64-bit slots, interpreted by the step function.
    pub data: [u64; 8],
}

/// A runnable cooperative task.
struct Task {
    step: fn(&mut TaskCtx) -> TaskStatus,
    ctx: TaskCtx,
}

/// Run queue. Only the main loop touches this.
static SCHED: Mutex<Scheduler> = Mutex::new(Scheduler::EMPTY);

struct Scheduler {
    queue: VecDeque<Task>,
    spawned_total: usize,
}

impl Scheduler {
    const EMPTY: Scheduler = Scheduler {
        queue: VecDeque::new(),
        spawned_total: 0,
    };
}

/// Initializes the scheduler (empty queues) and the default galfs actor.
pub fn init() {
    galfs::init();
    serial_println!("[sched] ready");
}

/// Adds a cooperative task to the run queue. Call from main-loop context
/// only; `name` is used for serial-log accounting.
pub fn spawn(name: &'static str, step: fn(&mut TaskCtx) -> TaskStatus) {
    let mut sched = SCHED.lock();
    sched.queue.push_back(Task {
        step,
        ctx: TaskCtx::default(),
    });
    sched.spawned_total += 1;
    serial_println!("[sched] spawned '{}'", name);
}

/// Runs one step of the front task (classic round-robin: yield → re-queue
/// at the back, done → drop).
pub fn run_once() {
    let Some(mut task) = SCHED.lock().queue.pop_front() else {
        return;
    };
    let status = (task.step)(&mut task.ctx);
    if status == TaskStatus::Yield {
        SCHED.lock().queue.push_back(task);
    }
}

/// Number of cooperative tasks currently queued.
pub fn active_tasks() -> usize {
    SCHED.lock().queue.len()
}

/// Total cooperative tasks spawned since boot.
pub fn spawned_total() -> usize {
    SCHED.lock().spawned_total
}

/// Runs every queued task once (one full round-robin sweep). The main loop
/// calls this between `hlt()`s.
pub fn run() {
    let rounds = active_tasks();
    for _ in 0..rounds {
        run_once();
    }
}

/* ---------------- preemptive threads ---------------- */

/// Per-thread kernel stack size.
pub(crate) const THREAD_STACK_SIZE: usize = 32 * 1024;

/// 16-byte-aligned buffer (FXSAVE requires it).
#[repr(align(16))]
struct FxArea(
    // Storage targeted by raw pointer in FXSAVE/FXRSTOR — never read by name.
    #[allow(dead_code)] [u8; context::FX_AREA_SIZE],
);

impl FxArea {
    const fn new() -> Self {
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
const STATE_RUNNING: u8 = 0;
const STATE_EXITED: u8 = 1; // returned from its entry; reaped by the main loop
const STATE_FREED: u8 = 2; // stack + fx freed; rotation-skipped until reused
const STATE_WAITING: u8 = 3; // parked: spawn/wait/sleep/I/O until an event

/// Bytes kept for a thread's name. Spawn already rejects a longer name.
const NAME_CAP: usize = 64;

/// Process Caps per task (matches [`galexy_abi::MAX_PROC_CAPS`]).
const MAX_PROC_CAPS: usize = galexy_abi::MAX_PROC_CAPS as usize;
const _: () = assert!(MAX_PROC_CAPS == 16);

/// One process Cap entry: child thread slot + generation + rights.
#[derive(Clone, Copy)]
struct ProcHandle {
    /// 1-based index into [`THREADS`].
    child_slot: u8,
    /// Must match the child's `cap_gen` or the Cap is stale.
    gen: u32,
    rights: CapRights,
}

/// Monotonic debug id (listings / serial only — never an open-by-id key).
static NEXT_DEBUG_ID: AtomicU64 = AtomicU64::new(1);

/// Magic word painted at the very bottom of each thread's stack (lowest
/// address). A stack that overflows far enough to corrupt the heap walks
/// downward through this word first — reaping detects the clobber.
const STACK_CANARY: u64 = 0x0CA7_AB1E_500D_F00D;

/// Open ramdisk files one task may hold at once. The table is carved into
/// the `Thread` at spawn so `open` never allocates (the syscall runs IF=0;
/// a heap grow there would broadcast a shootdown that targets must ack).
const MAX_OPEN_FILES: usize = 8;

/// Where an open file's bytes live. The per-task slot only keeps the cursor.
#[derive(Clone, Copy)]
enum FileBody {
    /// Immutable archive bytes. The slice lives in the ramdisk.
    Archive(&'static [u8]),
    /// Index into [`galfs`] object table. The bytes are writable.
    Galfs(u16),
    /// Anonymous pipe end.
    Pipe { id: u8, end: pipe::PipeEnd },
}

/// One open file. Archive bytes live in the bootloader's ramdisk; galfs
/// bytes live in the global table. Only the cursor is per-open.
#[derive(Clone, Copy)]
struct OpenFile {
    body: FileBody,
    offset: usize,
    /// Authoritative rights. The handle's upper half is a snapshot; a call
    /// is allowed only for the intersection of the two.
    rights: CapRights,
}

/// Rights the launcher recorded on a task. A fabricated cap index is not
/// enough: the matching bit has to be set here.
#[derive(Clone, Copy)]
pub(crate) struct Grants {
    console: bool,
    keyboard: bool,
    loader: bool,
    /// `stats`, `tasks`, `threads`, and `ls`.
    query: bool,
    power: bool,
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

    fn allows(self, grant: Grant) -> bool {
        match grant {
            Grant::Console => self.console,
            Grant::Keyboard => self.keyboard,
            Grant::Loader => self.loader,
            Grant::Query => self.query,
            Grant::Power => self.power,
        }
    }
}

struct Thread {
    /// Display name. Copied at spawn so the caller's buffer can go away.
    name_bytes: [u8; NAME_CAP],
    name_len: u8,
    /// Lifecycle state (see STATE_* consts).
    state: AtomicU8,
    /// Saved context pointer; valid while the thread is NOT running.
    ctx: AtomicU64,
    /// Timer ticks charged to this thread (CPU-time attribution).
    ticks: AtomicU64,
    /// FXSAVE area — freed by the reaper once the thread exits.
    fx: *mut FxArea,
    /// `true` for ring-3 tasks: the fabricated context runs in user mode
    /// and preemption pushes onto `kstack` via TSS.RSP0.
    is_user: bool,
    /// Kernel threads: the 32 KiB mode+context stack (heap-backed), canary
    /// painted at the bottom. User tasks: EMPTY (their context lives on
    /// mapped user-space pages).
    stack: Vec<u8>,
    /// User tasks only: the kernel-mode stack for ring 3→0 transitions
    /// (TSS.RSP0 target). Empty for kernel threads.
    kstack: Vec<u8>,
    /// User tasks only: aligned top of `kstack` (the RSP0 value).
    kstack_top: u64,
    /// The task's page-table root (physical address). `0` = the kernel's
    /// table (Step A: every task shares it; Step B: only kernel threads —
    /// user tasks get a FreshL4 at spawn).
    cr3: AtomicU64,
    /// User tasks only: the P4 entry index of their region in their own
    /// tree (the reaper's tree walk needs it).
    user_p4: u16,
    /// The CPU that owns (runs + reaps) this thread — "pinned at spawn"
    /// (SMP M18); work stealing (M19) may flip it to an idle CPU.
    owner: u8,
    /// The timer tick of this thread's last steal (anti-ping-pong cooldown
    /// for the idle-CPU steal path). 0 = never stolen (eligible).
    stolen_at: AtomicU64,
    /// File capabilities belonging to this task. Empty for kernel threads.
    /// Indexes are [`galexy_abi::FILE_CAP_BASE`] + slot. Cleared on reap.
    files: [Option<OpenFile>; MAX_OPEN_FILES],
    /// Process Caps (children). Indexes are [`PROC_CAP_BASE`] + slot.
    procs: [Option<ProcHandle>; MAX_PROC_CAPS],
    /// Idle stealing skips this thread. The interactive shell is resident
    /// on the BSP: the keyboard and the framebuffer have one consumer.
    no_steal: bool,
    /// When `STATE_WAITING`: 1-based child slot to wake on (0 = wait for
    /// pending spawn load only, or a sleep / I/O wait). Cap-wait uses this
    /// instead of a name.
    wait_child_slot: AtomicU8,
    /// When waiting on a child: true = wake on exit; false = wake on load.
    wait_for_exit: AtomicBool,
    /// Absolute `timer_ticks` deadline for [`Syscall::Sleep`]. `0` means
    /// this wait is not a sleep (spawn/Cap-wait/I/O).
    sleep_deadline: AtomicU64,
    /// I/O wait kind while `STATE_WAITING` (Milestone 57): `0` none,
    /// `1` keyboard, `2` pipe read, `3` pipe write.
    io_kind: AtomicU8,
    /// Pipe id when `io_kind` is pipe read/write.
    io_pipe: AtomicU8,
    /// User buffer address for a parked I/O syscall.
    io_addr: AtomicU64,
    /// User buffer length for a parked I/O syscall.
    io_len: AtomicU32,
    /// Cap bits for the parked I/O syscall (file/keyboard).
    io_cap: AtomicU64,
    /// Exit status stamped on [`STATE_EXITED`] (read by Cap-wait).
    exit_code: AtomicU64,
    /// Bumped when the slot is reaped/reused so old process Caps fail.
    cap_gen: AtomicU32,
    /// Monotonic debug id for `tasks` / serial (not a handle).
    debug_id: u64,
    /// 1-based parent slot; `0` = kernel-spawned root.
    parent_slot: u8,
    /// Milestone 53: orphan-root / first ring-3 supervisor. At most one.
    is_init: bool,
    /// Set when a Cap-wait (or `SPAWN_WAIT`) has collected the exit code.
    exit_waited: AtomicBool,
    /// Reserved services this task may call. Set at spawn, never grown.
    grants: Grants,
    /// Console this task writes, and whose keyboard queue it reads.
    /// Inherited from the task that spawned it. F1 is 0.
    tty: u8,
    /// Actor root this task walks from when a path has no `owner@`.
    fs_root: u16,
    /// Tokens that authorize galfs paths. Utilities inherit; bare spawns do not.
    fs_tokens: [galfs::Token; galfs::TOKEN_SLOTS],
    /// Set when the task was created as admin. Survives [`task_su`] so the
    /// seat can return to admin after switching to another actor.
    born_admin: bool,
    /// Timer tick when [`console_budget_used`] was last reset.
    console_budget_tick: u64,
    /// Console bytes written during [`console_budget_tick`].
    console_budget_used: u32,
}

impl Thread {
    fn name(&self) -> &str {
        let n = self.name_len as usize;
        core::str::from_utf8(&self.name_bytes[..n]).unwrap_or("")
    }
}

/// Copies `name` into a fixed buffer, stopping on a char boundary at 64.
fn pack_name(name: &str) -> ([u8; NAME_CAP], u8) {
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
static THREADS: Mutex<Vec<Thread>> = Mutex::new(Vec::new());

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
struct CpuSched {
    /// 0 = this CPU's main is current; otherwise a thread index + 1
    /// (only threads OWNED by this CPU may ever be current elsewhere).
    current: AtomicUsize,
    /// This CPU's round-robin cursor (usize::MAX = none yet).
    last_served: AtomicUsize,
    /// This CPU's main saved context pointer (0 = not yet saved).
    main_ctx: AtomicU64,
    /// This CPU's main FXSAVE area.
    main_fx: Mutex<FxArea>,
    /// CPU ticks charged to THIS CPU's main.
    main_ticks: AtomicU64,
}

impl CpuSched {
    const fn new() -> Self {
        Self {
            current: AtomicUsize::new(0),
            last_served: AtomicUsize::new(usize::MAX),
            main_ctx: AtomicU64::new(0),
            main_fx: Mutex::new(FxArea::new()),
            main_ticks: AtomicU64::new(0),
        }
    }
}

static CPU_SCHED: [CpuSched; crate::arch::cpu::MAX_CPUS] =
    [const { CpuSched::new() }; crate::arch::cpu::MAX_CPUS];

/// This CPU's rotation state. Per-CPU ownership (fenced by IRQ gating in
/// every user); NEVER locks CPU_SCHED[i] from a foreign CPU.
fn cpu_sched() -> &'static CpuSched {
    &CPU_SCHED[crate::arch::cpu::current_index()]
}
/// The BSP's main ticks (shell-side accounting; the status bar and tests
/// use this — AP idles are off-graph).
pub fn main_ticks() -> u64 {
    CPU_SCHED[0].main_ticks.load(Ordering::Relaxed)
}
/// Where the next spawned thread/task lands: round-robin across the CPUs
/// the MADT brought online ("pinned at spawn"; no migration, no stealing).
static NEXT_CPU: AtomicUsize = AtomicUsize::new(0);
/// Completed work-steals (diagnostics + test assertions for the steal
/// proof).
static STEALS: AtomicU64 = AtomicU64::new(0);
/// A freshly stolen thread cannot be stolen again for this many timer
/// ticks (~0.1 s machine time): idle CPUs must not ping-pong a hot task
/// between them.
const STEAL_COOLDOWN_TICKS: u64 = 100;

/// Live slots. A freed record is reusable, so this caps threads that still
/// occupy a slot (running, waiting, exited, or not yet safe to recycle).
const MAX_THREADS: usize = 64;
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
fn slot_reusable(threads: &[Thread], index: usize) -> bool {
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
fn push_thread(thread: Thread) -> u8 {
    let mut threads = THREADS.lock();
    if let Some(index) = (0..threads.len()).find(|&i| slot_reusable(&threads, i)) {
        // False until this thread's owner publishes a switch-out. Stored
        // before the record becomes RUNNING, so a steal scan cannot take
        // the slot on its first run.
        CTX_STABLE[index].store(false, Ordering::Release);
        threads[index] = thread;
        return (index + 1) as u8;
    }
    assert!(
        threads.len() < MAX_THREADS,
        "sched: thread table full ({MAX_THREADS} live slots)"
    );
    CTX_STABLE[threads.len()].store(false, Ordering::Release);
    threads.push(thread);
    threads.len() as u8
}

/// Incoming `slot` (1-based; 0 = main) is about to be entered, so its saved
/// context is not stealable until this CPU switches off it.
fn claim_incoming(slot: usize) {
    if slot != 0 {
        CTX_STABLE[slot - 1].store(false, Ordering::Release);
    }
}

/// Number of completed work-steals since boot.
pub fn steal_count() -> u64 {
    STEALS.load(Ordering::Relaxed)
}

fn next_cpu() -> u8 {
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
fn enter_task_cr3(cr3_addr: u64) {
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
            // File/process caps die with the task. Bump gen so foreign Caps fail.
            threads[i].files = [None; MAX_OPEN_FILES];
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
}

pub(crate) fn register_user_task(init: TaskInit<'_>) -> u8 {
    interrupts::without_interrupts(|| {
        // Canary at the very bottom of the kernel-mode stack.
        let mut kstack = init.kstack;
        kstack[..8].copy_from_slice(&STACK_CANARY.to_le_bytes());
        let fx = Box::into_raw(Box::new(FxArea::new()));
        let owner = init.owner.unwrap_or_else(next_cpu);
        let (name_bytes, name_len) = pack_name(init.name);
        push_thread(Thread {
            name_bytes,
            name_len,
            state: AtomicU8::new(STATE_RUNNING),
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
            console_budget_tick: 0,
            console_budget_used: 0,
        })
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
            console_budget_tick: 0,
            console_budget_used: 0,
        });
        serial_println!("[sched] thread '{}' ready (owner cpu {})", name, owner);
        let _ = _slot;
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

/// Like [`spawn_user_task`], with shell-grade grants (loader, queries, …).
pub fn spawn_user_launcher(
    name: &str,
    build: impl FnOnce(UserRegion) -> Vec<u8>,
) -> (UserRegion, u8) {
    spawn_user_with_grants(name, galfs::admin_cred(), Grants::launcher(), build)
}

/// Like [`spawn_user_task`], with explicit galfs credentials (token tests).
pub fn spawn_user_with(
    name: &str,
    fs: galfs::FsCred,
    build: impl FnOnce(UserRegion) -> Vec<u8>,
) -> (UserRegion, u8) {
    spawn_user_with_grants(name, fs, Grants::console(), build)
}

/// Like [`spawn_user_with`], with an explicit grant set.
pub(crate) fn spawn_user_with_grants(
    name: &str,
    fs: galfs::FsCred,
    grants: Grants,
    build: impl FnOnce(UserRegion) -> Vec<u8>,
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
        let owner = next_cpu();
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
            no_steal: false,
            wait_child_slot: AtomicU8::new(0),
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
            console_budget_tick: 0,
            console_budget_used: 0,
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
        let handle = threads
            .get(slot - 1)
            .ok_or(SysError::BadCap)?
            .procs[pi]
            .ok_or(SysError::BadCap)?;
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
fn format_inspect_line(dst: &mut [u8], id: u64, name: &str, state: &str) -> usize {
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
pub fn thread_stats() -> alloc::vec::Vec<(alloc::string::String, u64)> {
    interrupts::without_interrupts(|| {
        THREADS
            .lock()
            .iter()
            .filter(|t| t.state.load(Ordering::Relaxed) == STATE_RUNNING)
            .map(|t| {
                (
                    alloc::string::String::from(t.name()),
                    t.ticks.load(Ordering::Relaxed),
                )
            })
            .collect()
    })
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

/// Argument bytes copied onto a new task's stack. Matches the syscall cap.
pub(crate) const ARG_MAX: usize = 256;

/// One queued `spawn`. The syscall path only copies the name and the
/// argument (it runs IF=0); the main loop loads the ELF on the kernel
/// page table. Without [`galexy_abi::SPAWN_WAIT`], the caller wakes once
/// the child is running (with a process Cap in `rax`); with it, the
/// caller wakes when the child exits (exit code in `rax`).
struct PendingSpawn {
    name: [u8; 64],
    len: u8,
    arg: [u8; ARG_MAX],
    arg_len: u16,
    query: bool,
    /// Park until the child exits (not only until load finishes).
    wait_exit: bool,
    /// Console the child inherits from the task that asked.
    tty: u8,
    /// Milestone 54: init spawning an F-key seat (`shell`…`shell12`).
    seat: bool,
    /// Child inherits the waiter's galfs credentials.
    fs: galfs::FsCred,
    /// 1-based slot of the parked parent.
    waiter_slot: u8,
    armed: bool,
}

static PENDING_SPAWN: Mutex<PendingSpawn> = Mutex::new(PendingSpawn {
    name: [0; 64],
    len: 0,
    arg: [0; ARG_MAX],
    arg_len: 0,
    query: false,
    wait_exit: false,
    tty: 0,
    seat: false,
    fs: galfs::FsCred::none(),
    waiter_slot: 0,
    armed: false,
});

/// Queues `name` and parks the current task.
///
/// `arg` is handed to the child. `query` adds the query grant on top of
/// the console. `wait_exit` keeps the caller parked until the child
/// exits. `inherit` copies the parent's galfs tokens (utilities). The
/// caller must already be a running user task. Lock order: this takes
/// `PENDING_SPAWN`, then `THREADS`.
pub(crate) fn task_spawn(
    name: &str,
    arg: &[u8],
    query: bool,
    wait_exit: bool,
    inherit: bool,
) -> Result<(), SysError> {
    if name.len() > 64 || arg.len() > ARG_MAX {
        return Err(SysError::BadValue);
    }
    let slot = current_slot();
    if slot == 0 {
        return Err(SysError::BadCap);
    }
    interrupts::without_interrupts(|| {
        let mut pending = PENDING_SPAWN.lock();
        if pending.armed {
            return Err(SysError::NoResource);
        }
        let mut threads = THREADS.lock();
        {
            let thread = threads.get(slot - 1).ok_or(SysError::BadCap)?;
            if !thread.is_user || thread.state.load(Ordering::Acquire) != STATE_RUNNING {
                return Err(SysError::BadCap);
            }
            // F-key console names: only init may spawn seats (Milestone 54).
            let seat = is_console_shell_name(name);
            if seat && !thread.is_init {
                return Err(SysError::Unsupported);
            }
        }
        let seat = is_console_shell_name(name);
        // One live task per name: two `linger`s on one TTY would fight
        // the console. Wait/kill keys are Caps, not names.
        let name_busy = threads.iter().any(|thread| {
            let state = thread.state.load(Ordering::Acquire);
            thread.name() == name && (state == STATE_RUNNING || state == STATE_WAITING)
        });
        if name_busy {
            return Err(SysError::NoResource);
        }
        let parent_tty = threads[slot - 1].tty;
        let parent_fs = galfs::FsCred {
            root: threads[slot - 1].fs_root,
            tokens: threads[slot - 1].fs_tokens,
        };
        pending.name[..name.len()].copy_from_slice(name.as_bytes());
        pending.len = name.len() as u8;
        pending.arg[..arg.len()].copy_from_slice(arg);
        pending.arg_len = arg.len() as u16;
        pending.query = query;
        pending.wait_exit = wait_exit;
        // Seat TTY: 1-based index in arg[0] (same as kernel spawn_shell_on).
        pending.tty = if seat {
            arg.first().copied().unwrap_or(1).saturating_sub(1).min(11)
        } else {
            parent_tty
        };
        pending.seat = seat;
        pending.waiter_slot = slot as u8;
        // Seats start logged out (no cards). Utilities inherit the session.
        // Bare programs keep the root for path context but hold no cards.
        pending.fs = if seat {
            galfs::unauth_cred()
        } else if inherit || wait_exit {
            parent_fs
        } else {
            galfs::FsCred {
                root: parent_fs.root,
                tokens: [galfs::Token::empty(); galfs::TOKEN_SLOTS],
            }
        };
        pending.armed = true;
        // 0 = waiting for load; drain installs Cap then either wakes or
        // sets wait_child_slot to the new child for exit wait.
        let thread = &mut threads[slot - 1];
        thread.wait_child_slot.store(0, Ordering::Relaxed);
        thread.wait_for_exit.store(wait_exit, Ordering::Relaxed);
        thread.state.store(STATE_WAITING, Ordering::Release);
        Ok(())
    })
}

/// Loads a queued program, if the shell has asked for one.
///
/// Runs from the main loop: that context is the kernel page table, which
/// `spawn_program` clones. Installs a process Cap on the waiter; without
/// `wait_exit` wakes with Cap bits in `rax`, with `wait_exit` parks until
/// the child exits (exit code in `rax`).
pub fn drain_spawn() {
    let queued = interrupts::without_interrupts(|| {
        let mut pending = PENDING_SPAWN.lock();
        if !pending.armed {
            return None;
        }
        let len = pending.len as usize;
        let mut name = [0u8; 64];
        name[..len].copy_from_slice(&pending.name[..len]);
        let arg_len = pending.arg_len as usize;
        let mut arg = [0u8; ARG_MAX];
        arg[..arg_len].copy_from_slice(&pending.arg[..arg_len]);
        let query = pending.query;
        let wait_exit = pending.wait_exit;
        let tty = pending.tty;
        let seat = pending.seat;
        let fs = pending.fs;
        let waiter_slot = pending.waiter_slot;
        pending.armed = false;
        Some((len, name, arg_len, arg, query, wait_exit, tty, seat, fs, waiter_slot))
    });
    let Some((len, name_raw, arg_len, arg, query, wait_exit, tty, seat, fs, waiter_slot)) =
        queued
    else {
        return;
    };
    let name = core::str::from_utf8(&name_raw[..len]).unwrap_or("");
    if !spawn_frames_available() {
        serial_println!(
            "[sched] spawn '{}': low frames ({} < {}); waking NoResource",
            name,
            crate::arch::mm::free_frames(),
            SPAWN_FRAME_RESERVE
        );
        interrupts::without_interrupts(|| {
            let mut threads = THREADS.lock();
            wake_spawn_waiter(
                &mut threads,
                waiter_slot,
                SyscallResult::err(SysError::NoResource),
            );
        });
        return;
    }
    let grants = if seat {
        Grants::pre_login()
    } else if query {
        Grants::console_query()
    } else {
        Grants::console()
    };
    // Seats share the `shell` ELF under twelve reserved names.
    let elf_name = if seat { "shell" } else { name };
    let child_slot = if let Some(bytes) = ramdisk::find(elf_name) {
        Some(if seat {
            loader::spawn_launched_seat(
                name,
                bytes,
                grants,
                &arg[..arg_len],
                tty,
                fs,
                waiter_slot,
            )
        } else {
            loader::spawn_launched(
                name,
                bytes,
                grants,
                &arg[..arg_len],
                tty,
                fs,
                waiter_slot,
            )
        })
    } else {
        serial_println!(
            "[sched] spawn '{}' elf='{}' seat={} missing at drain",
            name,
            elf_name,
            seat
        );
        None
    };
    interrupts::without_interrupts(|| {
        let mut threads = THREADS.lock();
        let Some(child_slot) = child_slot else {
            wake_spawn_waiter(&mut threads, waiter_slot, SyscallResult::err(SysError::NotFound));
            return;
        };
        let Some(cap_bits) = install_proc_cap(&mut threads, waiter_slot, child_slot) else {
            wake_spawn_waiter(
                &mut threads,
                waiter_slot,
                SyscallResult::err(SysError::NoResource),
            );
            return;
        };
        // Milestone 55: seat (or any) spawn makes the child the TTY's
        // foreground job Cap target for Ctrl-C.
        if let Some(w) = threads.get(waiter_slot as usize - 1) {
            let tty = w.tty as usize;
            if tty < FG_SLOTS.len() {
                let gen = threads[child_slot as usize - 1]
                    .cap_gen
                    .load(Ordering::Acquire);
                FG_SLOTS[tty].store(child_slot, Ordering::Release);
                FG_GENS[tty].store(gen, Ordering::Release);
            }
        }
        if wait_exit {
            if let Some(w) = threads.get_mut(waiter_slot as usize - 1) {
                w.wait_child_slot.store(child_slot, Ordering::Release);
                w.wait_for_exit.store(true, Ordering::Release);
            }
        } else {
            wake_spawn_waiter(&mut threads, waiter_slot, SyscallResult::ok(cap_bits));
        }
    });
}

/// Names of the twelve shells. F1 keeps `shell` so a faulted shell is
/// still the task the restart log and the typing tests already know.
const SHELL_NAMES: [&str; 12] = [
    "shell", "shell2", "shell3", "shell4", "shell5", "shell6", "shell7", "shell8", "shell9",
    "shell10", "shell11", "shell12",
];

/// True when `name` is reserved for an F-key console shell.
pub fn is_console_shell_name(name: &str) -> bool {
    SHELL_NAMES.contains(&name)
}

/// True when a task named `name` is running or parked on a load.
fn named_is_live(name: &str) -> bool {
    interrupts::without_interrupts(|| {
        THREADS.lock().iter().any(|thread| {
            let state = thread.state.load(Ordering::Acquire);
            thread.name() == name && (state == STATE_RUNNING || state == STATE_WAITING)
        })
    })
}

/// True when a task named `shell` is running or parked on a load.
pub fn shell_is_live() -> bool {
    named_is_live("shell")
}

/// True when every F-key seat name is live (Milestone 54 boot gate).
pub fn seats_are_live() -> bool {
    SHELL_NAMES.iter().all(|name| named_is_live(name))
}

/// Per-TTY foreground job (Milestone 55): child slot + cap_gen.
const FG_COUNT: usize = crate::drivers::keyboard::TTY_COUNT;
static FG_SLOTS: [AtomicU8; FG_COUNT] = [const { AtomicU8::new(0) }; FG_COUNT];
static FG_GENS: [AtomicU32; FG_COUNT] = [const { AtomicU32::new(0) }; FG_COUNT];

/// Test helper: pin TTY `tty`'s foreground job to `child_slot`.
pub fn set_foreground_for_test(tty: u8, child_slot: u8) {
    let tty = tty as usize;
    if tty >= FG_COUNT || child_slot == 0 {
        return;
    }
    interrupts::without_interrupts(|| {
        let threads = THREADS.lock();
        let Some(t) = threads.get(child_slot as usize - 1) else {
            return;
        };
        let gen = t.cap_gen.load(Ordering::Acquire);
        FG_SLOTS[tty].store(child_slot, Ordering::Release);
        FG_GENS[tty].store(gen, Ordering::Release);
    });
}

/// Ctrl-C / signals-lite: kill the foreground job on `tty` if live.
///
/// Returns true when a foreground task was stopped (caller should not
/// deliver `^C` into the keyboard ring). The process Cap holder Cap-waits
/// exit status `137`.
pub fn interrupt_foreground(tty: u8) -> bool {
    let tty = tty as usize;
    if tty >= FG_COUNT {
        return false;
    }
    interrupts::without_interrupts(|| {
        let mut threads = THREADS.lock();
        let slot = FG_SLOTS[tty].load(Ordering::Acquire);
        let gen = FG_GENS[tty].load(Ordering::Acquire);
        if slot == 0 || slot as usize > threads.len() {
            return false;
        }
        let i = slot as usize - 1;
        if threads[i].cap_gen.load(Ordering::Acquire) != gen {
            FG_SLOTS[tty].store(0, Ordering::Release);
            return false;
        }
        if threads[i].is_init {
            return false;
        }
        let state = threads[i].state.load(Ordering::Acquire);
        if state != STATE_RUNNING && state != STATE_WAITING {
            FG_SLOTS[tty].store(0, Ordering::Release);
            return false;
        }
        if state == STATE_WAITING {
            interrupt_io_waiter(&mut threads, i);
        }
        threads[i]
            .exit_code
            .store(EXIT_KILLED, Ordering::Release);
        threads[i].state.store(STATE_EXITED, Ordering::Release);
        let mut raw = [0u8; NAME_CAP];
        let n = threads[i].name_len as usize;
        raw[..n].copy_from_slice(&threads[i].name_bytes[..n]);
        let name = core::str::from_utf8(&raw[..n]).unwrap_or("");
        serial_println!(
            "[sched] fg job '{}' killed by Ctrl-C on tty{} id={}",
            name,
            tty + 1,
            threads[i].debug_id
        );
        wake_exit_waiters(&mut threads, slot, EXIT_KILLED);
        FG_SLOTS[tty].store(0, Ordering::Release);
        true
    })
}

/// True when a user `spawn` is queued (single-slot PENDING_SPAWN).
pub fn spawn_is_pending() -> bool {
    interrupts::without_interrupts(|| PENDING_SPAWN.lock().armed)
}

/// Loads one interactive shell on `tty` when that name is not already live.
fn ensure_one_shell(name: &str, tty: u8) {
    if named_is_live(name) {
        return;
    }
    let Some(bytes) = ramdisk::find("shell") else {
        return;
    };
    if name == "shell" {
        serial_println!("[sched] shell is gone; loading it again");
    } else {
        serial_println!("[sched] {} is gone; loading it again", name);
    }
    loader::spawn_shell_on(name, bytes, tty);
}

/// Loads every F-key shell that has exited.
///
/// Other tasks are left alone. Each new shell starts logged out (pre-login
/// grants) on the console it had. A missing ramdisk entry does nothing.
pub fn ensure_shell() {
    for (tty, name) in SHELL_NAMES.iter().enumerate() {
        ensure_one_shell(name, tty as u8);
    }
}

/// Starts one shell on every F-key before the main loop reports ready.
pub fn spawn_all_shells() {
    let Some(bytes) = ramdisk::find("shell") else {
        return;
    };
    for (tty, name) in SHELL_NAMES.iter().enumerate() {
        loader::spawn_shell_on(name, bytes, tty as u8);
    }
}

/// True when a live init task exists (Cap-kill must return AccessDenied).
pub fn init_unkillable() -> bool {
    init_slot().is_some()
}

/// Live slot of userspace init, if any.
pub fn init_slot() -> Option<u8> {
    interrupts::without_interrupts(|| {
        THREADS.lock().iter().enumerate().find_map(|(i, t)| {
            if !t.is_init {
                return None;
            }
            let state = t.state.load(Ordering::Acquire);
            if state == STATE_RUNNING || state == STATE_WAITING {
                Some((i + 1) as u8)
            } else {
                None
            }
        })
    })
}

/// Loads ramdisk `init` once (Milestone 53). Returns false if missing.
pub fn spawn_init() -> bool {
    let Some(bytes) = ramdisk::find("init") else {
        return false;
    };
    if init_slot().is_some() {
        return true;
    }
    loader::spawn_init(bytes);
    serial_println!("[sched] init loaded (orphan root)");
    true
}

/// Moves process Caps from a reaped parent to init and reparents children.
fn transfer_orphans_to_init(threads: &mut [Thread], dead_slot: u8, dead_index: usize) {
    let init_slot = threads.iter().enumerate().find_map(|(i, t)| {
        if t.is_init {
            let state = t.state.load(Ordering::Acquire);
            if state == STATE_RUNNING || state == STATE_WAITING {
                return Some((i + 1) as u8);
            }
        }
        None
    });
    let Some(init_slot) = init_slot else {
        for t in threads.iter_mut() {
            if t.parent_slot == dead_slot {
                t.parent_slot = 0;
            }
        }
        return;
    };
    let mut moved = alloc::vec::Vec::new();
    for slot in threads[dead_index].procs.iter_mut() {
        if let Some(handle) = slot.take() {
            moved.push(handle);
        }
    }
    let ii = init_slot as usize - 1;
    for handle in moved {
        let ci = handle.child_slot as usize;
        if ci == 0 || ci > threads.len() {
            continue;
        }
        if threads[ci - 1].cap_gen.load(Ordering::Acquire) != handle.gen {
            continue;
        }
        let state = threads[ci - 1].state.load(Ordering::Acquire);
        if state == STATE_FREED {
            continue;
        }
        if let Some(empty) = threads[ii].procs.iter().position(|p| p.is_none()) {
            threads[ii].procs[empty] = Some(handle);
        }
    }
    for t in threads.iter_mut() {
        if t.parent_slot == dead_slot {
            t.parent_slot = init_slot;
        }
    }
}

/// Installs a [`PROC_PARENT`] Cap on `waiter_slot` for `child_slot`.
/// Returns Cap bits, or `None` if the parent's process-Cap table is full.
fn install_proc_cap(
    threads: &mut [Thread],
    waiter_slot: u8,
    child_slot: u8,
) -> Option<u64> {
    if waiter_slot == 0 || child_slot == 0 {
        return None;
    }
    let wi = waiter_slot as usize - 1;
    let ci = child_slot as usize - 1;
    let gen = threads.get(ci)?.cap_gen.load(Ordering::Acquire);
    let parent = threads.get_mut(wi)?;
    let slot = parent.procs.iter().position(|p| p.is_none())?;
    parent.procs[slot] = Some(ProcHandle {
        child_slot,
        gen,
        rights: CapRights::PROC_PARENT,
    });
    Some(Cap::new(PROC_CAP_BASE + slot as u64, CapRights::PROC_PARENT).bits())
}

/// Stamps `result` into a parked waiter's saved frame and marks it runnable.
fn wake_spawn_waiter(threads: &mut [Thread], waiter_slot: u8, result: SyscallResult) {
    if waiter_slot == 0 {
        return;
    }
    let Some(thread) = threads.get_mut(waiter_slot as usize - 1) else {
        return;
    };
    if thread.state.load(Ordering::Acquire) != STATE_WAITING {
        return;
    }
    clear_wait_fields(thread);
    stamp_waiter_frame(thread, result);
    thread.state.store(STATE_RUNNING, Ordering::Release);
}

/// Writes syscall result registers into a parked task's saved context.
fn stamp_waiter_frame(thread: &Thread, result: SyscallResult) {
    let ctx_ptr = thread.ctx.load(Ordering::Acquire);
    if ctx_ptr == 0 {
        return;
    }
    // SAFETY: waiter is STATE_WAITING; ctx points at its saved syscall frame.
    let ctx = ctx_ptr as *mut context::Context;
    unsafe {
        (*ctx).rax = result.value;
        (*ctx).rdx = if result.ok { 1 } else { 0 };
    }
}

/// Wakes every task Cap-waiting on `child_slot` for exit, stamping exit codes.
fn wake_exit_waiters(threads: &mut [Thread], child_slot: u8, exit_code: u64) {
    let mut any = false;
    for thread in threads.iter_mut() {
        if thread.wait_child_slot.load(Ordering::Acquire) != child_slot {
            continue;
        }
        if !thread.wait_for_exit.load(Ordering::Acquire) {
            continue;
        }
        if thread.state.load(Ordering::Acquire) != STATE_WAITING {
            continue;
        }
        clear_wait_fields(thread);
        stamp_waiter_frame(thread, SyscallResult::ok(exit_code));
        thread.state.store(STATE_RUNNING, Ordering::Release);
        any = true;
    }
    // Only mark waited when a Cap-waiter (or SPAWN_WAIT) collected the
    // status. Kill alone must leave a zombie until Wait / Cap drop.
    if any {
        if let Some(child) = threads.get(child_slot as usize - 1) {
            child.exit_waited.store(true, Ordering::Release);
        }
    }
}

/// Calls `each` with every galfs path the current task may list
/// (`Desktop/`, `dan@Desktop/notes`). The bytes are only valid inside
/// the callback.
pub(crate) fn for_each_scratch_path(mut each: impl FnMut(&[u8])) {
    let slot = current_slot();
    if slot == 0 {
        return;
    }
    interrupts::without_interrupts(|| {
        let threads = THREADS.lock();
        let Some(thread) = threads.get(slot - 1) else {
            return;
        };
        galfs::for_each_visible(thread.fs_root, &thread.fs_tokens, |path| each(path));
    });
}

/// Opens a file for the current user task.
///
/// An exact ramdisk name (`banner.txt`, `hello`) grants READ. Any other
/// path is a galfs file and grants READ and WRITE when a token covers it.
/// A directory is `Unsupported`. A path with no token is `AccessDenied`.
pub(crate) fn task_open(name: &str) -> Result<Cap, SysError> {
    if !name.contains('/') && !name.contains('@') {
        if let Some(bytes) = crate::sched::ramdisk::find(name) {
            return install_open(FileBody::Archive(bytes), CapRights::READ);
        }
    }
    let slot = current_slot();
    if slot == 0 {
        return Err(SysError::BadCap);
    }
    interrupts::without_interrupts(|| {
        let mut threads = THREADS.lock();
        let thread = threads.get_mut(slot - 1).ok_or(SysError::BadCap)?;
        if !thread.is_user || thread.state.load(Ordering::Acquire) != STATE_RUNNING {
            return Err(SysError::BadCap);
        }
        let Some(index) = thread.files.iter().position(|slot| slot.is_none()) else {
            return Err(SysError::NoResource);
        };
        let found = galfs::open_file(thread.fs_root, &thread.fs_tokens, name)?;
        let rights = CapRights::READ.union(CapRights::WRITE);
        thread.files[index] = Some(OpenFile {
            body: FileBody::Galfs(found),
            offset: 0,
            rights,
        });
        Ok(Cap::new(galexy_abi::FILE_CAP_BASE + index as u64, rights))
    })
}

fn install_open(body: FileBody, rights: CapRights) -> Result<Cap, SysError> {
    let slot = current_slot();
    if slot == 0 {
        return Err(SysError::BadCap);
    }
    interrupts::without_interrupts(|| {
        let mut threads = THREADS.lock();
        let thread = threads.get_mut(slot - 1).ok_or(SysError::BadCap)?;
        if !thread.is_user || thread.state.load(Ordering::Acquire) != STATE_RUNNING {
            return Err(SysError::BadCap);
        }
        let Some(index) = thread.files.iter().position(|slot| slot.is_none()) else {
            return Err(SysError::NoResource);
        };
        thread.files[index] = Some(OpenFile {
            body,
            offset: 0,
            rights,
        });
        Ok(Cap::new(galexy_abi::FILE_CAP_BASE + index as u64, rights))
    })
}

/// Copies the next bytes of an open file into `dst`. `Ok(0)` is end of file.
///
/// The effective right is the intersection of the kernel grant and the
/// handle snapshot, so a task cannot inflate READ onto a handle it stripped,
/// and cannot use a WRITE-only forgery of a file index.
/// Sync wrapper around [`task_read_ex`]; keep for call sites that cannot park.
#[allow(dead_code)]
pub(crate) fn task_read(cap: Cap, dst: &mut [u8]) -> Result<usize, SysError> {
    match task_read_ex(cap, dst)? {
        IoOp::Ready(n) => Ok(n),
        IoOp::ParkPipe { .. } => Ok(0),
    }
}

/// Blocking-aware file read (Milestone 57). Archive/galfs stay non-blocking.
pub(crate) fn task_read_ex(cap: Cap, dst: &mut [u8]) -> Result<IoOp, SysError> {
    task_read_inner(cap, dst)
}

/// Result of a file/pipe I/O attempt that may need to park.
#[derive(Clone, Copy, Debug)]
pub(crate) enum IoOp {
    /// Completed with `n` bytes (EOF is `Ready(0)`).
    Ready(usize),
    /// Caller should park on this pipe end.
    ParkPipe { id: u8, read: bool },
}

fn task_read_inner(cap: Cap, dst: &mut [u8]) -> Result<IoOp, SysError> {
    let index = file_slot(cap)?;
    let slot = current_slot();
    if slot == 0 {
        return Err(SysError::BadCap);
    }
    interrupts::without_interrupts(|| {
        let mut threads = THREADS.lock();
        let thread = threads.get_mut(slot - 1).ok_or(SysError::BadCap)?;
        let file = thread.files[index].as_mut().ok_or(SysError::BadCap)?;
        let effective = file.rights.intersection(cap.rights());
        if !effective.contains(CapRights::READ) {
            return Err(SysError::AccessDenied);
        }
        if dst.is_empty() {
            return Ok(IoOp::Ready(0));
        }
        let start = file.offset;
        let op = match file.body {
            FileBody::Archive(bytes) => {
                let available = bytes.len().saturating_sub(start);
                let n = dst.len().min(available);
                dst[..n].copy_from_slice(&bytes[start..start + n]);
                IoOp::Ready(n)
            }
            FileBody::Galfs(obj) => {
                IoOp::Ready(galfs::read_at(obj, start, dst).ok_or(SysError::BadCap)?)
            }
            FileBody::Pipe { id, end } => {
                if end != pipe::PipeEnd::Read {
                    return Err(SysError::AccessDenied);
                }
                match pipe::try_read(id, dst)? {
                    pipe::ReadResult::Ready(n) => IoOp::Ready(n),
                    pipe::ReadResult::Eof => IoOp::Ready(0),
                    pipe::ReadResult::WouldBlock => IoOp::ParkPipe { id, read: true },
                }
            }
        };
        if let IoOp::Ready(n) = op {
            if !matches!(file.body, FileBody::Pipe { .. }) {
                file.offset = start + n;
            }
        }
        Ok(op)
    })
}

/// Appends `src` to a galfs file. An archive open is `Unsupported`.
///
/// The read cursor stays put, so a later `read` still starts at the
/// beginning. A write that does not fit is short: the count is the bytes
/// copied, and `0` means the buffer is already full.
/// Sync wrapper around [`task_write_ex`]; keep for call sites that cannot park.
#[allow(dead_code)]
pub(crate) fn task_write(cap: Cap, src: &[u8]) -> Result<usize, SysError> {
    match task_write_ex(cap, src)? {
        IoOp::Ready(n) => Ok(n),
        IoOp::ParkPipe { .. } => Ok(0),
    }
}

/// Blocking-aware file write (Milestone 57). Full pipes return [`IoOp::ParkPipe`].
pub(crate) fn task_write_ex(cap: Cap, src: &[u8]) -> Result<IoOp, SysError> {
    let index = file_slot(cap)?;
    let slot = current_slot();
    if slot == 0 {
        return Err(SysError::BadCap);
    }
    let (op, galfs_wrote, wake_pipe) = interrupts::without_interrupts(|| {
        let mut threads = THREADS.lock();
        let thread = threads.get_mut(slot - 1).ok_or(SysError::BadCap)?;
        let file = thread.files[index].as_mut().ok_or(SysError::BadCap)?;
        let effective = file.rights.intersection(cap.rights());
        if !effective.contains(CapRights::WRITE) {
            return Err(SysError::AccessDenied);
        }
        if src.is_empty() {
            return Ok((IoOp::Ready(0), false, None));
        }
        match file.body {
            FileBody::Galfs(obj) => {
                let n = galfs::append(obj, src).ok_or(SysError::BadCap)?;
                Ok((IoOp::Ready(n), n > 0, None))
            }
            FileBody::Pipe { id, end } => {
                if end != pipe::PipeEnd::Write {
                    return Err(SysError::AccessDenied);
                }
                match pipe::try_write(id, src)? {
                    pipe::WriteResult::Ready(n) => Ok((IoOp::Ready(n), false, Some(id))),
                    pipe::WriteResult::WouldBlock => {
                        Ok((IoOp::ParkPipe { id, read: false }, false, None))
                    }
                    pipe::WriteResult::Closed => Err(SysError::Unsupported),
                }
            }
            FileBody::Archive(_) => Err(SysError::Unsupported),
        }
    })?;
    if galfs_wrote {
        galfs::mark_dirty();
    }
    if let Some(id) = wake_pipe {
        wake_pipe_waiters(id);
    }
    Ok(op)
}

/// Creates a galfs file or directory for the current user task.
///
/// A path ending in `/` is a directory and the returned cap is null.
/// A file cap carries READ and WRITE. `replace` empties an existing
/// file instead of failing. A ramdisk name at `/` cannot be
/// replaced. A name that already exists is `Unsupported`. A missing
/// parent is `NotFound`. A full object table, or a full per-task file
/// table, is `NoResource`. A path with no create token is `AccessDenied`.
pub(crate) fn task_create(name: &str, replace: bool) -> Result<Cap, SysError> {
    let parsed = galfs::parse_path(name)?;
    if parsed.owner.is_none()
        && parsed.n == 1
        && crate::sched::ramdisk::find(parsed.comps[0]).is_some()
    {
        return Err(SysError::Unsupported);
    }
    let slot = current_slot();
    if slot == 0 {
        return Err(SysError::BadCap);
    }
    let cap = interrupts::without_interrupts(|| {
        let mut threads = THREADS.lock();
        let thread = threads.get_mut(slot - 1).ok_or(SysError::BadCap)?;
        if !thread.is_user || thread.state.load(Ordering::Acquire) != STATE_RUNNING {
            return Err(SysError::BadCap);
        }
        let file_index = thread.files.iter().position(|slot| slot.is_none());
        let created = galfs::create(thread.fs_root, &thread.fs_tokens, name, replace)?;
        match created {
            None => Ok(Cap::null()),
            Some(obj) => {
                let Some(index) = file_index else {
                    return Err(SysError::NoResource);
                };
                let rights = CapRights::READ.union(CapRights::WRITE);
                thread.files[index] = Some(OpenFile {
                    body: FileBody::Galfs(obj),
                    offset: 0,
                    rights,
                });
                Ok(Cap::new(galexy_abi::FILE_CAP_BASE + index as u64, rights))
            }
        }
    })?;
    galfs::mark_dirty();
    Ok(cap)
}

/// Deletes a galfs file or an empty directory and frees its slot.
///
/// A ramdisk name at `/` is `Unsupported`. A directory that still has a
/// child is `Unsupported`. A missing path is `NotFound`. Any task's open
/// cap on that slot is dropped, so a later read or write is `BadCap`.
/// A path with no remove token is `AccessDenied`.
pub(crate) fn task_remove(name: &str) -> Result<(), SysError> {
    let parsed = galfs::parse_path(name)?;
    if parsed.owner.is_none()
        && parsed.n == 1
        && crate::sched::ramdisk::find(parsed.comps[0]).is_some()
    {
        return Err(SysError::Unsupported);
    }
    let slot = current_slot();
    if slot == 0 {
        return Err(SysError::BadCap);
    }
    interrupts::without_interrupts(|| {
        let mut threads = THREADS.lock();
        {
            let thread = threads.get(slot - 1).ok_or(SysError::BadCap)?;
            if !thread.is_user || thread.state.load(Ordering::Acquire) != STATE_RUNNING {
                return Err(SysError::BadCap);
            }
        }
        let (fs_root, fs_tokens) = {
            let thread = threads.get(slot - 1).ok_or(SysError::BadCap)?;
            (thread.fs_root, thread.fs_tokens)
        };
        let removed = galfs::remove(fs_root, &fs_tokens, name)?;
        for thread in threads.iter_mut() {
            for open in &mut thread.files {
                let stale = matches!(
                    *open,
                    Some(file) if matches!(file.body, FileBody::Galfs(body) if body == removed)
                );
                if stale {
                    *open = None;
                }
            }
        }
        Ok(())
    })?;
    galfs::mark_dirty();
    Ok(())
}

/// Moves a galfs dirent from `old` to `new`.
pub(crate) fn task_rename(old: &str, new: &str) -> Result<(), SysError> {
    let old_parsed = galfs::parse_path(old)?;
    let new_parsed = galfs::parse_path(new)?;
    if old_parsed.owner.is_none()
        && old_parsed.n == 1
        && crate::sched::ramdisk::find(old_parsed.comps[0]).is_some()
    {
        return Err(SysError::Unsupported);
    }
    if new_parsed.owner.is_none()
        && new_parsed.n == 1
        && crate::sched::ramdisk::find(new_parsed.comps[0]).is_some()
    {
        return Err(SysError::Unsupported);
    }
    let slot = current_slot();
    if slot == 0 {
        return Err(SysError::BadCap);
    }
    interrupts::without_interrupts(|| {
        let threads = THREADS.lock();
        let thread = threads.get(slot - 1).ok_or(SysError::BadCap)?;
        if !thread.is_user || thread.state.load(Ordering::Acquire) != STATE_RUNNING {
            return Err(SysError::BadCap);
        }
        galfs::rename(thread.fs_root, &thread.fs_tokens, old, new)
    })?;
    galfs::mark_dirty();
    Ok(())
}

/// Sets the length of an open galfs file.
pub(crate) fn task_truncate(cap: Cap, new_len: u64) -> Result<(), SysError> {
    let index = file_slot(cap)?;
    let slot = current_slot();
    if slot == 0 {
        return Err(SysError::BadCap);
    }
    if new_len > galfs::FILE_BYTES as u64 {
        return Err(SysError::BadValue);
    }
    interrupts::without_interrupts(|| {
        let mut threads = THREADS.lock();
        let thread = threads.get_mut(slot - 1).ok_or(SysError::BadCap)?;
        let file = thread.files[index].as_mut().ok_or(SysError::BadCap)?;
        let effective = file.rights.intersection(cap.rights());
        if !effective.contains(CapRights::WRITE) {
            return Err(SysError::AccessDenied);
        }
        match file.body {
            FileBody::Galfs(obj) => {
                galfs::truncate(obj, new_len as usize)?;
                if file.offset > new_len as usize {
                    file.offset = new_len as usize;
                }
                Ok(())
            }
            FileBody::Archive(_) | FileBody::Pipe { .. } => Err(SysError::Unsupported),
        }
    })?;
    galfs::mark_dirty();
    Ok(())
}

/// Writes galfs metadata for `name` into `out`.
pub(crate) fn task_stat(name: &str, out: &mut [u8]) -> Result<usize, SysError> {
    let parsed = galfs::parse_path(name)?;
    if parsed.owner.is_none()
        && parsed.n == 1
        && crate::sched::ramdisk::find(parsed.comps[0]).is_some()
    {
        return Err(SysError::Unsupported);
    }
    let slot = current_slot();
    if slot == 0 {
        return Err(SysError::BadCap);
    }
    interrupts::without_interrupts(|| {
        let threads = THREADS.lock();
        let thread = threads.get(slot - 1).ok_or(SysError::BadCap)?;
        if !thread.is_user || thread.state.load(Ordering::Acquire) != STATE_RUNNING {
            return Err(SysError::BadCap);
        }
        galfs::stat(thread.fs_root, &thread.fs_tokens, name, out)
    })
}

/// Drops one file capability belonging to the current task.
///
/// Close is possession of the slot, not a READ: the index names the open
/// in this task's table, and no other task has that table.
pub(crate) fn task_close(cap: Cap) -> Result<(), SysError> {
    let slot = current_slot();
    if slot == 0 {
        return Err(SysError::BadCap);
    }
    if let Ok(pi) = proc_slot(cap) {
        return interrupts::without_interrupts(|| {
            let mut threads = THREADS.lock();
            let thread = threads.get_mut(slot - 1).ok_or(SysError::BadCap)?;
            let Some(_) = thread.procs[pi].take() else {
                return Err(SysError::BadCap);
            };
            Ok(())
        });
    }
    let index = file_slot(cap)?;
    let wake_id = interrupts::without_interrupts(|| {
        let mut threads = THREADS.lock();
        let thread = threads.get_mut(slot - 1).ok_or(SysError::BadCap)?;
        let Some(file) = thread.files[index].take() else {
            return Err(SysError::BadCap);
        };
        Ok(match file.body {
            FileBody::Pipe { id, end } => {
                pipe::close_end(id, end);
                Some(id)
            }
            _ => None,
        })
    })?;
    // Wake outside THREADS — `wake_pipe_waiters` takes the same lock.
    if let Some(id) = wake_id {
        wake_pipe_waiters(id);
    }
    Ok(())
}

/// Exit status stamped when a task is stopped by [`task_kill`].
const EXIT_KILLED: u64 = 137;

/// Parks until the process Cap's child exits; returns the exit code.
///
/// On success the Cap slot is cleared (stale). If the child has already
/// exited, returns immediately (`Some(code)`). Otherwise consumes the Cap
/// into a park and returns `None` (caller must hand off the CPU).
pub(crate) fn task_wait(cap: Cap) -> Result<Option<u64>, SysError> {
    if !cap.rights().contains(CapRights::PROC_WAIT) {
        return Err(SysError::AccessDenied);
    }
    let pi = proc_slot(cap)?;
    let slot = current_slot();
    if slot == 0 {
        return Err(SysError::BadCap);
    }
    interrupts::without_interrupts(|| {
        let mut threads = THREADS.lock();
        {
            let thread = threads.get(slot - 1).ok_or(SysError::BadCap)?;
            if !thread.is_user || thread.state.load(Ordering::Acquire) != STATE_RUNNING {
                return Err(SysError::BadCap);
            }
        }
        let handle = threads[slot - 1].procs[pi].ok_or(SysError::BadCap)?;
        if !handle.rights.contains(CapRights::PROC_WAIT) {
            return Err(SysError::AccessDenied);
        }
        let ci = handle.child_slot as usize;
        if ci == 0 || ci > threads.len() {
            threads[slot - 1].procs[pi] = None;
            return Err(SysError::BadCap);
        }
        let gen = threads[ci - 1].cap_gen.load(Ordering::Acquire);
        if gen != handle.gen {
            threads[slot - 1].procs[pi] = None;
            return Err(SysError::BadCap);
        }
        let state = threads[ci - 1].state.load(Ordering::Acquire);
        if state == STATE_EXITED || state == STATE_FREED {
            let code = threads[ci - 1].exit_code.load(Ordering::Acquire);
            threads[ci - 1]
                .exit_waited
                .store(true, Ordering::Release);
            threads[slot - 1].procs[pi] = None;
            return Ok(Some(code));
        }
        if state != STATE_RUNNING && state != STATE_WAITING {
            threads[slot - 1].procs[pi] = None;
            return Err(SysError::BadCap);
        }
        // Consume Cap into the park (abi: Cap stale after successful wait).
        let child_slot = handle.child_slot;
        threads[slot - 1].procs[pi] = None;
        let waiter = &mut threads[slot - 1];
        waiter.wait_child_slot.store(child_slot, Ordering::Release);
        waiter.wait_for_exit.store(true, Ordering::Release);
        waiter.state.store(STATE_WAITING, Ordering::Release);
        Ok(None)
    })
}

/// Stops the task named by a process Cap (`PROC_KILL`).
///
/// Idempotent if the child has already exited. Does not reap — the
/// holder still [`task_wait`]s (or drops the Cap) for zombie cleanup.
pub(crate) fn task_kill(cap: Cap) -> Result<(), SysError> {
    if !cap.rights().contains(CapRights::PROC_KILL) {
        return Err(SysError::AccessDenied);
    }
    let pi = proc_slot(cap)?;
    let slot = current_slot();
    if slot == 0 {
        return Err(SysError::BadCap);
    }
    interrupts::without_interrupts(|| {
        let mut threads = THREADS.lock();
        let handle = {
            let thread = threads.get(slot - 1).ok_or(SysError::BadCap)?;
            if !thread.is_user {
                return Err(SysError::BadCap);
            }
            thread.procs[pi].ok_or(SysError::BadCap)?
        };
        if !handle.rights.contains(CapRights::PROC_KILL) {
            return Err(SysError::AccessDenied);
        }
        let ci = handle.child_slot as usize;
        if ci == 0 || ci > threads.len() {
            return Err(SysError::BadCap);
        }
        if threads[ci - 1].cap_gen.load(Ordering::Acquire) != handle.gen {
            return Err(SysError::BadCap);
        }
        let state = threads[ci - 1].state.load(Ordering::Acquire);
        if state == STATE_EXITED || state == STATE_FREED {
            return Ok(());
        }
        if state != STATE_RUNNING && state != STATE_WAITING {
            return Err(SysError::BadCap);
        }
        // Milestone 53: init is immortal to user kill.
        if threads[ci - 1].is_init {
            return Err(SysError::AccessDenied);
        }
        // Milestone 57: cancel sleep / I/O parks with Interrupted so a
        // kill of a blocked task never leaves a waiter stranded; Cap-wait
        // still observes EXIT_KILLED below.
        if state == STATE_WAITING {
            interrupt_io_waiter(&mut threads, ci - 1);
        }
        threads[ci - 1]
            .exit_code
            .store(EXIT_KILLED, Ordering::Release);
        threads[ci - 1]
            .state
            .store(STATE_EXITED, Ordering::Release);
        let mut raw = [0u8; NAME_CAP];
        let n = threads[ci - 1].name_len as usize;
        raw[..n].copy_from_slice(&threads[ci - 1].name_bytes[..n]);
        let debug_id = threads[ci - 1].debug_id;
        let name = core::str::from_utf8(&raw[..n]).unwrap_or("");
        serial_println!(
            "[sched] task '{}' killed code={} id={}",
            name,
            EXIT_KILLED,
            debug_id
        );
        wake_exit_waiters(&mut threads, handle.child_slot, EXIT_KILLED);
        Ok(())
    })
}

/// Installs a galfs token on a live user task named `target`.
///
/// The current task must already hold every bit in `rights` on the
/// resolved object. Lock order: [`THREADS`] then the galfs table.
pub(crate) fn task_grant(path: &str, rights: u8, target: &str) -> Result<(), SysError> {
    let slot = current_slot();
    if slot == 0 {
        return Err(SysError::BadCap);
    }
    interrupts::without_interrupts(|| {
        let mut threads = THREADS.lock();
        let (fs_root, fs_tokens) = {
            let caller = threads.get(slot - 1).ok_or(SysError::BadCap)?;
            if !caller.is_user || caller.state.load(Ordering::Acquire) != STATE_RUNNING {
                return Err(SysError::BadCap);
            }
            (caller.fs_root, caller.fs_tokens)
        };
        let object = galfs::resolve_and_check(fs_root, &fs_tokens, path, rights)?;
        let Some(ti) = threads.iter().position(|t| {
            t.is_user && t.state.load(Ordering::Acquire) == STATE_RUNNING && t.name() == target
        }) else {
            return Err(SysError::NotFound);
        };
        galfs::push_token(&mut threads[ti].fs_tokens, object, rights)
    })
}

/// Drops galfs token rights on a live user task named `target`.
pub(crate) fn task_revoke(path: &str, rights: u8, target: &str) -> Result<(), SysError> {
    let slot = current_slot();
    if slot == 0 {
        return Err(SysError::BadCap);
    }
    interrupts::without_interrupts(|| {
        let mut threads = THREADS.lock();
        let (fs_root, fs_tokens) = {
            let caller = threads.get(slot - 1).ok_or(SysError::BadCap)?;
            if !caller.is_user || caller.state.load(Ordering::Acquire) != STATE_RUNNING {
                return Err(SysError::BadCap);
            }
            (caller.fs_root, caller.fs_tokens)
        };
        let object = galfs::resolve_and_check(fs_root, &fs_tokens, path, rights)?;
        let Some(ti) = threads.iter().position(|t| {
            t.is_user && t.state.load(Ordering::Acquire) == STATE_RUNNING && t.name() == target
        }) else {
            return Err(SysError::NotFound);
        };
        galfs::revoke_token(&mut threads[ti].fs_tokens, object, rights)
    })
}

/// Creates a pipe and installs both ends in the caller's file table.
/// Returns `(read_cap, write_cap)`.
pub(crate) fn task_pipe() -> Result<(Cap, Cap), SysError> {
    let slot = current_slot();
    if slot == 0 {
        return Err(SysError::BadCap);
    }
    interrupts::without_interrupts(|| {
        let mut threads = THREADS.lock();
        let thread = threads.get_mut(slot - 1).ok_or(SysError::BadCap)?;
        if !thread.is_user || thread.state.load(Ordering::Acquire) != STATE_RUNNING {
            return Err(SysError::BadCap);
        }
        let mut free = [usize::MAX; 2];
        let mut nfree = 0usize;
        for (i, slot) in thread.files.iter().enumerate() {
            if slot.is_none() {
                free[nfree] = i;
                nfree += 1;
                if nfree == 2 {
                    break;
                }
            }
        }
        if nfree < 2 {
            return Err(SysError::NoResource);
        }
        let id = pipe::alloc()?;
        let ri = free[0];
        let wi = free[1];
        let read_rights = CapRights::READ;
        let write_rights = CapRights::WRITE;
        thread.files[ri] = Some(OpenFile {
            body: FileBody::Pipe {
                id,
                end: pipe::PipeEnd::Read,
            },
            offset: 0,
            rights: read_rights,
        });
        thread.files[wi] = Some(OpenFile {
            body: FileBody::Pipe {
                id,
                end: pipe::PipeEnd::Write,
            },
            offset: 0,
            rights: write_rights,
        });
        Ok((
            Cap::new(galexy_abi::FILE_CAP_BASE + ri as u64, read_rights),
            Cap::new(galexy_abi::FILE_CAP_BASE + wi as u64, write_rights),
        ))
    })
}

/// Moves an open file/pipe **or process Cap** from the caller to `target`.
/// Returns the new Cap (rights attenuated for process Caps).
pub(crate) fn task_give(cap: Cap, target: &str) -> Result<Cap, SysError> {
    if proc_slot(cap).is_ok() {
        return task_give_proc(cap, target);
    }
    let index = file_slot(cap)?;
    let slot = current_slot();
    if slot == 0 {
        return Err(SysError::BadCap);
    }
    interrupts::without_interrupts(|| {
        let mut threads = THREADS.lock();
        let file = {
            let caller = threads.get_mut(slot - 1).ok_or(SysError::BadCap)?;
            if !caller.is_user || caller.state.load(Ordering::Acquire) != STATE_RUNNING {
                return Err(SysError::BadCap);
            }
            caller.files[index].take().ok_or(SysError::BadCap)?
        };
        let Some(ti) = threads.iter().position(|t| {
            t.is_user && t.state.load(Ordering::Acquire) == STATE_RUNNING && t.name() == target
        }) else {
            // Put it back — target missing.
            if let Some(caller) = threads.get_mut(slot - 1) {
                caller.files[index] = Some(file);
            } else if let FileBody::Pipe { id, end } = file.body {
                pipe::close_end(id, end);
            }
            return Err(SysError::NotFound);
        };
        if ti == slot - 1 {
            threads[ti].files[index] = Some(file);
            return Err(SysError::BadValue);
        }
        let Some(dest) = threads[ti].files.iter().position(|s| s.is_none()) else {
            threads[slot - 1].files[index] = Some(file);
            return Err(SysError::NoResource);
        };
        let rights = file.rights;
        threads[ti].files[dest] = Some(file);
        Ok(Cap::new(galexy_abi::FILE_CAP_BASE + dest as u64, rights))
    })
}

/// Moves a process Cap to `target`. Requires [`CapRights::PROC_TRANSFER`].
fn task_give_proc(cap: Cap, target: &str) -> Result<Cap, SysError> {
    if !cap.rights().contains(CapRights::PROC_TRANSFER) {
        return Err(SysError::AccessDenied);
    }
    let pi = proc_slot(cap)?;
    let slot = current_slot();
    if slot == 0 {
        return Err(SysError::BadCap);
    }
    interrupts::without_interrupts(|| {
        let mut threads = THREADS.lock();
        let handle = {
            let caller = threads.get_mut(slot - 1).ok_or(SysError::BadCap)?;
            if !caller.is_user || caller.state.load(Ordering::Acquire) != STATE_RUNNING {
                return Err(SysError::BadCap);
            }
            caller.procs[pi].ok_or(SysError::BadCap)?
        };
        if !handle.rights.contains(CapRights::PROC_TRANSFER) {
            return Err(SysError::AccessDenied);
        }
        let ci = handle.child_slot as usize;
        if ci == 0
            || ci > threads.len()
            || threads[ci - 1].cap_gen.load(Ordering::Acquire) != handle.gen
        {
            threads[slot - 1].procs[pi] = None;
            return Err(SysError::BadCap);
        }
        // Attenuate: intersection of table rights and Cap word.
        let rights = handle.rights.intersection(cap.rights());
        if !rights.contains(CapRights::PROC_TRANSFER) {
            return Err(SysError::AccessDenied);
        }
        let Some(ti) = threads.iter().position(|t| {
            t.is_user && t.state.load(Ordering::Acquire) == STATE_RUNNING && t.name() == target
        }) else {
            return Err(SysError::NotFound);
        };
        if ti == slot - 1 {
            return Err(SysError::BadValue);
        }
        let Some(dest) = threads[ti].procs.iter().position(|s| s.is_none()) else {
            return Err(SysError::NoResource);
        };
        // Take from caller only after the target has a free slot.
        threads[slot - 1].procs[pi] = None;
        threads[ti].procs[dest] = Some(ProcHandle {
            child_slot: handle.child_slot,
            gen: handle.gen,
            rights,
        });
        Ok(Cap::new(PROC_CAP_BASE + dest as u64, rights))
    })
}

/// Sets the read cursor on an archive or galfs open. Returns the new offset.
pub(crate) fn task_seek(cap: Cap, offset: i64, whence: u64) -> Result<u64, SysError> {
    let index = file_slot(cap)?;
    let slot = current_slot();
    if slot == 0 {
        return Err(SysError::BadCap);
    }
    interrupts::without_interrupts(|| {
        let mut threads = THREADS.lock();
        let thread = threads.get_mut(slot - 1).ok_or(SysError::BadCap)?;
        let file = thread.files[index].as_mut().ok_or(SysError::BadCap)?;
        let effective = file.rights.intersection(cap.rights());
        if !effective.contains(CapRights::READ) && !effective.contains(CapRights::WRITE) {
            return Err(SysError::AccessDenied);
        }
        let len = match file.body {
            FileBody::Archive(bytes) => bytes.len(),
            FileBody::Galfs(obj) => {
                galfs::with_file(obj, |o| o.len as usize).ok_or(SysError::BadCap)?
            }
            FileBody::Pipe { .. } => return Err(SysError::Unsupported),
        };
        let base = match whence {
            galexy_abi::SEEK_SET => 0i64,
            galexy_abi::SEEK_CUR => file.offset as i64,
            galexy_abi::SEEK_END => len as i64,
            _ => return Err(SysError::BadValue),
        };
        let Some(raw) = base.checked_add(offset) else {
            return Err(SysError::BadValue);
        };
        let new = if raw < 0 {
            0usize
        } else if raw as usize > len {
            len
        } else {
            raw as usize
        };
        file.offset = new;
        Ok(new as u64)
    })
}

fn admin_caller(fs_root: u16, _tokens: &[galfs::Token; galfs::TOKEN_SLOTS]) -> bool {
    galfs::is_admin_root(fs_root)
}

/// Writes the current actor name into `out`.
pub(crate) fn task_whoami(out: &mut [u8]) -> Result<usize, SysError> {
    let slot = current_slot();
    if slot == 0 {
        return Err(SysError::BadCap);
    }
    interrupts::without_interrupts(|| {
        let threads = THREADS.lock();
        let thread = threads.get(slot - 1).ok_or(SysError::BadCap)?;
        if !thread.is_user || thread.state.load(Ordering::Acquire) != STATE_RUNNING {
            return Err(SysError::BadCap);
        }
        galfs::name_of_root(thread.fs_root, out)
    })
}

/// Writes every actor name, one per line, into `out`.
pub(crate) fn task_users(out: &mut [u8]) -> Result<usize, SysError> {
    let mut n = 0usize;
    let mut overflow = false;
    galfs::for_each_actor(|name| {
        if overflow {
            return;
        }
        let need = name.len() + 1;
        if n + need > out.len() {
            overflow = true;
            return;
        }
        out[n..n + name.len()].copy_from_slice(name);
        out[n + name.len()] = b'\n';
        n += need;
    });
    if overflow {
        return Err(SysError::BadBuffer);
    }
    Ok(n)
}

/// Writes the caller's galfs tokens into `out`.
pub(crate) fn task_tokens(out: &mut [u8]) -> Result<usize, SysError> {
    let slot = current_slot();
    if slot == 0 {
        return Err(SysError::BadCap);
    }
    interrupts::without_interrupts(|| {
        let threads = THREADS.lock();
        let thread = threads.get(slot - 1).ok_or(SysError::BadCap)?;
        if !thread.is_user || thread.state.load(Ordering::Acquire) != STATE_RUNNING {
            return Err(SysError::BadCap);
        }
        if thread.fs_root == galfs::NO_OBJECT {
            return Err(SysError::AccessDenied);
        }
        Ok(galfs::format_tokens(
            thread.fs_root,
            &thread.fs_tokens,
            out,
        ))
    })
}

/// Explicit galfs flush (`Syscall::Sync`).
pub(crate) fn task_sync() -> Result<(), SysError> {
    galfs::sync_explicit()
}

/// Writes a [`galexy_abi::QUOTA_LEN`] record for `name` (None = caller's actor).
pub(crate) fn task_quota(out: &mut [u8], name: Option<&str>) -> Result<usize, SysError> {
    let slot = current_slot();
    if slot == 0 {
        return Err(SysError::BadCap);
    }
    let (used_o, max_o, used_b, max_b) = interrupts::without_interrupts(|| {
        let threads = THREADS.lock();
        let thread = threads.get(slot - 1).ok_or(SysError::BadCap)?;
        if !thread.is_user || thread.state.load(Ordering::Acquire) != STATE_RUNNING {
            return Err(SysError::BadCap);
        }
        if thread.fs_root == galfs::NO_OBJECT {
            return Err(SysError::AccessDenied);
        }
        match name {
            Some(n) if !n.is_empty() => galfs::actor_quota(n),
            _ => galfs::root_quota(thread.fs_root),
        }
    })?;
    galfs::format_quota_record(used_o, max_o, used_b, max_b, out)
}

/// Sets durable quotas for `name`. Caller must be admin.
pub(crate) fn task_setquota(
    name: &str,
    max_objects: u16,
    max_bytes: u32,
) -> Result<(), SysError> {
    let slot = current_slot();
    if slot == 0 {
        return Err(SysError::BadCap);
    }
    interrupts::without_interrupts(|| {
        let threads = THREADS.lock();
        let thread = threads.get(slot - 1).ok_or(SysError::BadCap)?;
        if !thread.is_user || thread.state.load(Ordering::Acquire) != STATE_RUNNING {
            return Err(SysError::BadCap);
        }
        if !admin_caller(thread.fs_root, &thread.fs_tokens) {
            return Err(SysError::AccessDenied);
        }
        Ok(())
    })?;
    galfs::set_actor_quota(name, max_objects, max_bytes)
}

/// Creates an actor + Desktop with `password`. Caller must be admin.
pub(crate) fn task_useradd(name: &str, password: &[u8]) -> Result<(), SysError> {
    let slot = current_slot();
    if slot == 0 {
        return Err(SysError::BadCap);
    }
    interrupts::without_interrupts(|| {
        let threads = THREADS.lock();
        let thread = threads.get(slot - 1).ok_or(SysError::BadCap)?;
        if !thread.is_user || thread.state.load(Ordering::Acquire) != STATE_RUNNING {
            return Err(SysError::BadCap);
        }
        if !admin_caller(thread.fs_root, &thread.fs_tokens) {
            return Err(SysError::AccessDenied);
        }
        let _ = galfs::add_user(name, password)?;
        Ok(())
    })?;
    galfs::mark_dirty();
    Ok(())
}

/// Password login: replace the caller's session with `ALL` on `name`'s root.
pub(crate) fn task_login(name: &str, password: &[u8]) -> Result<(), SysError> {
    let slot = current_slot();
    if slot == 0 {
        return Err(SysError::BadCap);
    }
    if !galfs::verify_password(name, password)? {
        return Err(SysError::AccessDenied);
    }
    let target = galfs::root_named(name)?;
    interrupts::without_interrupts(|| {
        let mut threads = THREADS.lock();
        let caller = threads.get_mut(slot - 1).ok_or(SysError::BadCap)?;
        if !caller.is_user || caller.state.load(Ordering::Acquire) != STATE_RUNNING {
            return Err(SysError::BadCap);
        }
        install_session(caller, target, true)?;
        Ok(())
    })
}

/// Clear the caller's session (logged out / pre-login).
pub(crate) fn task_logout() -> Result<(), SysError> {
    let slot = current_slot();
    if slot == 0 {
        return Err(SysError::BadCap);
    }
    interrupts::without_interrupts(|| {
        let mut threads = THREADS.lock();
        let caller = threads.get_mut(slot - 1).ok_or(SysError::BadCap)?;
        if !caller.is_user || caller.state.load(Ordering::Acquire) != STATE_RUNNING {
            return Err(SysError::BadCap);
        }
        clear_session(caller);
        Ok(())
    })
}

/// Install `ALL` on `target`, durable home shares, and grants for that actor.
///
/// `from_login` sets [`Thread::born_admin`] from the target (password
/// identity). `su` passes `false` so switching to a non-admin actor does
/// **not** clear born-admin — the seat can `su admin` to return (AUTH.md).
fn install_session(caller: &mut Thread, target: u16, from_login: bool) -> Result<(), SysError> {
    caller.fs_root = target;
    caller.fs_tokens = [galfs::Token::empty(); galfs::TOKEN_SLOTS];
    galfs::push_token(&mut caller.fs_tokens, target, galfs::RIGHT_ALL)?;
    galfs::apply_shares(target, &mut caller.fs_tokens)?;
    let admin = galfs::is_admin_root(target);
    if from_login {
        caller.born_admin = admin;
    }
    caller.grants = if admin {
        Grants::launcher()
    } else {
        Grants::session()
    };
    Ok(())
}

/// Durable home share for actor `grantee` (survives logout; reapplied at login).
pub(crate) fn task_share(path: &str, rights: u8, grantee: &str) -> Result<(), SysError> {
    let slot = current_slot();
    if slot == 0 {
        return Err(SysError::BadCap);
    }
    let (fs_root, fs_tokens) = interrupts::without_interrupts(|| {
        let threads = THREADS.lock();
        let caller = threads.get(slot - 1).ok_or(SysError::BadCap)?;
        if !caller.is_user || caller.state.load(Ordering::Acquire) != STATE_RUNNING {
            return Err(SysError::BadCap);
        }
        Ok((caller.fs_root, caller.fs_tokens))
    })?;
    galfs::add_share(fs_root, &fs_tokens, path, rights, grantee)
}

/// Clears durable share rights for actor `grantee`.
pub(crate) fn task_unshare(path: &str, rights: u8, grantee: &str) -> Result<(), SysError> {
    let slot = current_slot();
    if slot == 0 {
        return Err(SysError::BadCap);
    }
    let (fs_root, fs_tokens) = interrupts::without_interrupts(|| {
        let threads = THREADS.lock();
        let caller = threads.get(slot - 1).ok_or(SysError::BadCap)?;
        if !caller.is_user || caller.state.load(Ordering::Acquire) != STATE_RUNNING {
            return Err(SysError::BadCap);
        }
        Ok((caller.fs_root, caller.fs_tokens))
    })?;
    galfs::remove_share(fs_root, &fs_tokens, path, rights, grantee)
}

fn clear_session(caller: &mut Thread) {
    caller.fs_root = galfs::NO_OBJECT;
    caller.fs_tokens = [galfs::Token::empty(); galfs::TOKEN_SLOTS];
    caller.born_admin = false;
    caller.grants = Grants::pre_login();
}

/// Sets a password. Admin may set any account; others only their own.
pub(crate) fn task_passwd(name: Option<&str>, password: &[u8]) -> Result<(), SysError> {
    let slot = current_slot();
    if slot == 0 {
        return Err(SysError::BadCap);
    }
    let mut name_buf = [0u8; 32];
    let name_len = interrupts::without_interrupts(|| {
        let threads = THREADS.lock();
        let thread = threads.get(slot - 1).ok_or(SysError::BadCap)?;
        if !thread.is_user || thread.state.load(Ordering::Acquire) != STATE_RUNNING {
            return Err(SysError::BadCap);
        }
        let is_admin = admin_caller(thread.fs_root, &thread.fs_tokens);
        if let Some(n) = name {
            if !is_admin {
                let nlen = galfs::name_of_root(thread.fs_root, &mut name_buf)?;
                if &name_buf[..nlen] != n.as_bytes() {
                    return Err(SysError::AccessDenied);
                }
            }
            if n.len() > name_buf.len() {
                return Err(SysError::BadValue);
            }
            name_buf[..n.len()].copy_from_slice(n.as_bytes());
            Ok(n.len())
        } else {
            if thread.fs_root == galfs::NO_OBJECT {
                return Err(SysError::AccessDenied);
            }
            galfs::name_of_root(thread.fs_root, &mut name_buf)
        }
    })?;
    let name_str = core::str::from_utf8(&name_buf[..name_len]).map_err(|_| SysError::BadValue)?;
    galfs::set_password(name_str, password)
}

/// Deletes an empty actor. Refuses admin and roots still in use.
///
/// Also refuses when any task still holds an open galfs cap on that
/// actor's objects. Tokens naming those objects are cleared on success.
pub(crate) fn task_userdel(name: &str) -> Result<(), SysError> {
    let slot = current_slot();
    if slot == 0 {
        return Err(SysError::BadCap);
    }
    interrupts::without_interrupts(|| {
        let mut threads = THREADS.lock();
        let thread = threads.get(slot - 1).ok_or(SysError::BadCap)?;
        if !thread.is_user || thread.state.load(Ordering::Acquire) != STATE_RUNNING {
            return Err(SysError::BadCap);
        }
        if !admin_caller(thread.fs_root, &thread.fs_tokens) {
            return Err(SysError::AccessDenied);
        }
        let root = galfs::root_named(name)?;
        let live = threads.iter().any(|t| {
            t.is_user && t.fs_root == root && {
                let s = t.state.load(Ordering::Acquire);
                s == STATE_RUNNING || s == STATE_WAITING
            }
        });
        if live {
            return Err(SysError::Unsupported);
        }
        let mut objs = [galfs::NO_OBJECT; 8];
        let n = galfs::collect_actor_objects(root, &mut objs);
        let objs = &objs[..n];
        for thread in threads.iter() {
            for open in &thread.files {
                let Some(file) = open else { continue };
                if let FileBody::Galfs(obj) = file.body {
                    if objs.contains(&obj) {
                        return Err(SysError::Unsupported);
                    }
                }
            }
        }
        galfs::remove_user(name)?;
        for thread in threads.iter_mut() {
            galfs::drop_tokens_on(&mut thread.fs_tokens, objs);
        }
        Ok(())
    })?;
    galfs::mark_dirty();
    Ok(())
}

/// Card-based identity switch: replace tokens with `ALL` on `name`'s root.
///
/// Allowed for an admin session, a born-admin seat returning to admin, or
/// a holder of `ALL` on the target root (access card). Password login is
/// [`task_login`].
pub(crate) fn task_su(name: &str) -> Result<(), SysError> {
    let slot = current_slot();
    if slot == 0 {
        return Err(SysError::BadCap);
    }
    interrupts::without_interrupts(|| {
        let mut threads = THREADS.lock();
        let (fs_root, fs_tokens, born_admin) = {
            let caller = threads.get(slot - 1).ok_or(SysError::BadCap)?;
            if !caller.is_user || caller.state.load(Ordering::Acquire) != STATE_RUNNING {
                return Err(SysError::BadCap);
            }
            (caller.fs_root, caller.fs_tokens, caller.born_admin)
        };
        let target = galfs::root_named(name)?;
        let to_admin = galfs::is_admin_root(target);
        let allowed = galfs::is_admin_root(fs_root)
            || (born_admin && to_admin)
            || galfs::holds_all(fs_root, &fs_tokens, target);
        if !allowed {
            return Err(SysError::AccessDenied);
        }
        let caller = threads.get_mut(slot - 1).ok_or(SysError::BadCap)?;
        install_session(caller, target, false)?;
        Ok(())
    })
}

/// Console bytes a task may emit per timer tick before further writes
/// return a short success (0). Stops a tight loop from pinning COM1.
const CONSOLE_BUDGET_PER_TICK: u32 = 512;

/// Takes up to `want` bytes from the current task's console budget.
pub(crate) fn console_take_budget(want: usize) -> usize {
    let slot = current_slot();
    if slot == 0 || want == 0 {
        return want;
    }
    let tick = crate::arch::timer_ticks();
    interrupts::without_interrupts(|| {
        let mut threads = THREADS.lock();
        let Some(thread) = threads.get_mut(slot - 1) else {
            return want;
        };
        if thread.console_budget_tick != tick {
            thread.console_budget_tick = tick;
            thread.console_budget_used = 0;
        }
        let room = CONSOLE_BUDGET_PER_TICK.saturating_sub(thread.console_budget_used) as usize;
        let n = want.min(room);
        thread.console_budget_used = thread.console_budget_used.saturating_add(n as u32);
        n
    })
}

/// File-table index for a user cap, or `BadCap` when it is not a file index.
fn file_slot(cap: Cap) -> Result<usize, SysError> {
    let index = cap.index();
    if index < galexy_abi::FILE_CAP_BASE {
        return Err(SysError::BadCap);
    }
    let slot = (index - galexy_abi::FILE_CAP_BASE) as usize;
    if slot >= MAX_OPEN_FILES {
        return Err(SysError::BadCap);
    }
    Ok(slot)
}

/// Process-Cap table index, or `BadCap` when it is not a process index.
fn proc_slot(cap: Cap) -> Result<usize, SysError> {
    let index = cap.index();
    if index < PROC_CAP_BASE {
        return Err(SysError::BadCap);
    }
    let slot = (index - PROC_CAP_BASE) as usize;
    if slot >= MAX_PROC_CAPS {
        return Err(SysError::BadCap);
    }
    Ok(slot)
}

/// The current rotation slot (0 = main loop; otherwise thread index + 1).
pub fn current_slot() -> usize {
    cpu_sched().current.load(Ordering::Relaxed)
}

/// Console the running task writes. The main loop is TTY 0.
///
/// The lock is dropped before return, so the caller can take the screen
/// lock afterwards.
pub fn current_tty() -> u8 {
    let slot = current_slot();
    if slot == 0 {
        return 0;
    }
    interrupts::without_interrupts(|| {
        THREADS
            .lock()
            .get(slot - 1)
            .map(|thread| thread.tty)
            .unwrap_or(0)
    })
}

/// Whether the current task was granted `grant` at spawn.
///
/// The lock is dropped before return, so the caller can take another lock
/// afterwards. A kernel slot (0) holds nothing.
pub(crate) fn task_granted(grant: Grant) -> bool {
    let slot = current_slot();
    if slot == 0 {
        return false;
    }
    interrupts::without_interrupts(|| {
        let threads = THREADS.lock();
        threads
            .get(slot - 1)
            .is_some_and(|thread| thread.grants.allows(grant))
    })
}

/// Is the CURRENT slot a ring-3 task? (`slot` per [`current_slot`].)
pub fn slot_is_user(slot: usize) -> bool {
    if slot == 0 {
        return false;
    }
    interrupts::without_interrupts(|| {
        let threads = THREADS.lock();
        slot <= threads.len() && threads[slot - 1].is_user
    })
}

/// Syscall-side context handoff (called from `arch`'s syscall dispatch on
/// the syscalling task's kernel stack, while its uniform frame is at `rsp`).
///
/// - `exit=false` (yield): the task stays schedulable, hand over the CPU
///   now — the real round-robin switch from inside a syscall.
/// - `exit=true` (exit): tombstone the task; the reaper frees its stacks.
///
/// Returns the context pointer to enter (guarantees a switch: main is
/// always the fallback — the caller is never main). The incoming RSP0 /
/// kstack registry are updated like the timer switch does. NOT irq-gated on
/// entry (the naked syscall entry runs with IF=0); the internal gate keeps
/// the lock-audit rule.
///
/// # Safety
///
/// `frame` must be the CURRENT task's uniform context frame on its kernel
/// stack, exactly as built by the syscall entry.
pub unsafe fn syscall_handoff(
    frame: *mut context::Context,
    exit: bool,
    reason: &'static str,
) -> u64 {
    // No-switch paths (there are none once we return) must not leave a
    // stale departed slot for the naked tail to publish.
    crate::arch::cpu::set_departed_slot(0);
    let me = cpu_sched();
    let slot = me.current.load(Ordering::Relaxed);
    assert!(slot != 0, "syscall_handoff: no task current (cpl bug?)");
    let pending_ctx = interrupts::without_interrupts(|| {
        let mut threads = THREADS.lock();

        // Save the outgoing task's context + FPU state into its slot
        // (yield keeps it schedulable; exit tombstones it).
        {
            let t = &threads[slot - 1];
            t.ctx.store(frame as u64, Ordering::Relaxed);
            context::fx_save(t.fx as *mut u8);
        }
        if exit {
            if threads[slot - 1].is_init {
                panic!("init exited ({reason}) — no orphan root");
            }
            let mut raw = [0u8; NAME_CAP];
            let n = threads[slot - 1].name_len as usize;
            raw[..n].copy_from_slice(&threads[slot - 1].name_bytes[..n]);
            let debug_id = threads[slot - 1].debug_id;
            // Exit code: syscall Exit puts it in rdi before handoff; faults use 0.
            let code = if reason == "syscall" {
                // SAFETY: frame is the exiting task's uniform context.
                unsafe { (*frame).rdi }
            } else {
                0
            };
            threads[slot - 1]
                .exit_code
                .store(code, Ordering::Release);
            threads[slot - 1]
                .state
                .store(STATE_EXITED, Ordering::Release);
            let name = core::str::from_utf8(&raw[..n]).unwrap_or("");
            serial_println!(
                "[sched] task '{}' exited ({}) code={} id={}",
                name,
                reason,
                code,
                debug_id
            );
            wake_exit_waiters(&mut threads, slot as u8, code);
        }

        // Advance the rotation: first eligible slot strictly after the
        // outgoing one (main at slot 0 is always the fallback, and the
        // caller is never main — so this scan ALWAYS finds a switch).
        // SMP: only THIS CPU's own threads are eligible; foreign threads
        // are skipped like tombstones (they rotate on their owner).
        let my_cpu = crate::arch::cpu::current_index() as u8;
        let n = threads.len();
        let mut cand = if slot >= n { 0 } else { slot + 1 };
        let mut scans = n + 1;
        while scans > 0 {
            let eligible = cand == 0
                || (threads[cand - 1].owner == my_cpu
                    && threads[cand - 1].state.load(Ordering::Acquire) == STATE_RUNNING);
            if eligible {
                break;
            }
            cand = if cand + 1 > n { 0 } else { cand + 1 };
            scans -= 1;
        }
        debug_assert!(scans > 0, "handoff rotation scan unwound without main");
        me.last_served.store(cand, Ordering::Relaxed);

        let (who, ctx, rsp0, cr3) = match cand {
            0 => (0usize, me.main_ctx.load(Ordering::Relaxed), None, 0),
            s => {
                let t2 = &threads[s - 1];
                let rsp0 = t2.is_user.then_some(t2.kstack_top);
                (
                    s,
                    t2.ctx.load(Ordering::Relaxed),
                    rsp0,
                    t2.cr3.load(Ordering::Relaxed),
                )
            }
        };
        assert!(ctx != 0, "handoff: entering a task with no saved context");
        crate::arch::syscall::set_task_kstack(rsp0.unwrap_or(0));
        if let Some(top) = rsp0 {
            crate::arch::set_tss_rsp0(VirtAddr::new(top));
        }
        enter_task_cr3(cr3);
        me.current.store(who, Ordering::Relaxed);
        // Outgoing thread's tail is still on its stack until `mov rsp`.
        // The naked tail publishes CTX_STABLE from gs:[40] after that.
        claim_incoming(who);
        crate::arch::cpu::set_departed_slot(slot as u64);
        let fx_ptr = match cand {
            0 => (&*me.main_fx.lock()) as *const FxArea as u64,
            s => threads[s - 1].fx as u64,
        };
        context::fx_restore(fx_ptr as *const u8);
        ctx
    });
    pending_ctx
}

/// The timer switch: saves the outgoing task's context + FPU state (main
/// loop or thread), picks the next thread round-robin, and returns its
/// context pointer — or 0 to resume the outgoing task untouched.
///
/// Called ONLY from the naked timer wrapper (IRQ context, IF=0). Locks are
/// never held across the actual switch: all save/decide/restore happens
/// under the IRQ-gated lock, then the lock is dropped before the naked code
/// swaps RSP.
///
/// # Safety
///
/// `frame` must be the outgoing task's context block (the naked wrapper's
/// RSP).
pub unsafe fn on_timer_tick(frame: *mut context::Context) -> u64 {
    crate::arch::cpu::set_departed_slot(0);
    // Tickless: advance by the one-shot duration that just fired.
    let elapsed = crate::arch::apic::take_armed_ms();
    crate::arch::timer::tick_by(elapsed);
    let now = crate::arch::timer_ticks();

    let me = cpu_sched();
    let next_ctx = interrupts::without_interrupts(|| {
        let mut threads = THREADS.lock();
        // Sleep deadlines use monotonic ticks advanced just above.
        wake_due_sleepers(&mut threads, now);
        let my_cpu = crate::arch::cpu::current_index() as u8;
        let current = me.current.load(Ordering::Relaxed);

        // CPU-time attribution: charge the armed window to whoever ran.
        match current {
            0 => {
                me.main_ticks.fetch_add(elapsed, Ordering::Relaxed);
            }
            i => {
                threads[i - 1].ticks.fetch_add(elapsed, Ordering::Relaxed);
            }
        }

        // Save the outgoing task's context + FPU state.
        match current {
            0 => {
                let mut fx = me.main_fx.lock();
                context::fx_save(&mut *fx as *mut FxArea as *mut u8);
                me.main_ctx.store(frame as u64, Ordering::Relaxed);
            }
            i => {
                let t = &threads[i - 1];
                context::fx_save(t.fx as *mut u8);
                t.ctx.store(frame as u64, Ordering::Relaxed);
            }
        }

        // Unified round-robin over THIS CPU's participants: its main
        // (slot 0), then the threads it OWNS (slots 1..=n; foreign-owned
        // slots are skipped like tombstones — SMP M18). Exited/freed slots
        // are skipped (tombstones; see STATE_* docs). Switching "to main" =
        // returning the per-CPU main context.
        let n = threads.len();
        if n == 0 {
            return None;
        }
        let last = me.last_served.load(Ordering::Relaxed);
        let mut next_slot = if last == usize::MAX {
            1 // first tick ever: serve the first thread
        } else if last + 1 > n {
            0 // wrap to main
        } else {
            last + 1
        };
        // Skip tombstones AND foreign-owned slots; main (slot 0) is always
        // eligible, so the scan terminates after at most n+1 steps.
        let mut scans = n + 1;
        while scans > 0 {
            let eligible = match next_slot {
                0 => true,
                s => {
                    threads[s - 1].owner == my_cpu
                        && threads[s - 1].state.load(Ordering::Acquire) == STATE_RUNNING
                }
            };
            if eligible {
                break;
            }
            next_slot = if next_slot + 1 > n { 0 } else { next_slot + 1 };
            scans -= 1;
        }
        debug_assert!(scans > 0, "rotation scan terminated without main");
        me.last_served.store(next_slot, Ordering::Relaxed);

        // IDLE-PASS WORK STEALING (SMP M19): the scan is about to serve
        // main. That alone is NOT "idle" (the alternate-with-main pattern
        // hits main every other tick even with runnable threads) — so the
        // steal attempt is gated on NO runnable thread being owned by this
        // CPU. Steal = flip the owner (all under the THREADS lock, which
        // both this switch and the victim's switch serialize on); the
        // ENTRY IS DEFERRED TO THIS CPU'S NEXT TICK (the rotation scan
        // picks it up as own+RUNNING). The steal itself requires
        // CTX_STABLE: the victim publishes that only AFTER `mov rsp` off
        // the thread stack. `current != slot` is not enough — it is stored
        // before the lock drops, while the naked tail is still on that
        // stack, and a host-starved victim can miss the stealer's next
        // guest tick (both CPUs then pop one frame). The cooldown
        // (stolen_at) keeps a hot thread from ping-ponging between idle
        // CPUs: it stays put for a while.
        if next_slot == 0 {
            let own_runnable = threads
                .iter()
                .any(|t| t.owner == my_cpu && t.state.load(Ordering::Acquire) == STATE_RUNNING);
            if !own_runnable {
                let now = crate::arch::timer_ticks();
                for (i, t) in threads.iter_mut().enumerate() {
                    let slot_no = i + 1;
                    let stolen_at = t.stolen_at.load(Ordering::Relaxed);
                    if t.owner == my_cpu
                        || t.state.load(Ordering::Acquire) != STATE_RUNNING
                        || t.no_steal
                        || now < stolen_at + STEAL_COOLDOWN_TICKS
                        || CPU_SCHED[t.owner as usize].current.load(Ordering::Relaxed) == slot_no
                        || !CTX_STABLE[i].load(Ordering::Acquire)
                    {
                        continue; // own / dead / resident / cooling / current / tail still on its stack
                    }
                    let victim = t.owner;
                    t.owner = my_cpu;
                    t.stolen_at.store(now, Ordering::Relaxed);
                    STEALS.fetch_add(1, Ordering::Relaxed);
                    serial_println!(
                        "[sched] cpu {} stole '{}' (slot {}) from cpu {} (enters next tick)",
                        my_cpu,
                        t.name(),
                        slot_no,
                        victim
                    );
                    break;
                }
            }
        }

        // Switching to ourselves (all other slots dead or foreign, and no
        // steal landed) = no switch.
        let current = me.current.load(Ordering::Relaxed);
        if next_slot == current {
            return None;
        }

        let (who, ctx, fx_ptr, rsp0, cr3) = match next_slot {
            0 => (
                0usize,
                me.main_ctx.load(Ordering::Relaxed),
                (&*me.main_fx.lock()) as *const FxArea as u64,
                None,
                0,
            ),
            s => {
                let t = &threads[s - 1];
                let rsp0 = t.is_user.then_some(t.kstack_top);
                (
                    s,
                    t.ctx.load(Ordering::Relaxed),
                    t.fx as u64,
                    rsp0,
                    t.cr3.load(Ordering::Relaxed),
                )
            }
        };
        // Ring-3 readiness BEFORE entering the chosen task: a user task's
        // ring 3→0 crossings (timer IRQ via per-CPU TSS.RSP0, later the
        // syscall entry via the per-CPU kstack slot gs:[8]) must push onto
        // ITS OWN kernel stack. Kernel threads/main reset the registry.
        // (Step B: CR3 is installed for the incoming task — the kernel half
        // is shared by every task table, so the switch is safe mid-flight;
        // CR3 itself is per-CPU hardware.)
        crate::arch::syscall::set_task_kstack(rsp0.unwrap_or(0));
        if let Some(top) = rsp0 {
            crate::arch::set_tss_rsp0(VirtAddr::new(top));
        }
        enter_task_cr3(cr3);
        me.current.store(who, Ordering::Relaxed);
        // `current` here is the outgoing slot (reloaded above). Its tail
        // still owns the stack until the naked `mov rsp`; gs:[40] tells
        // that tail which CTX_STABLE byte to set.
        claim_incoming(who);
        crate::arch::cpu::set_departed_slot(current as u64);
        context::fx_restore(fx_ptr as *const u8);
        Some(ctx)
    });

    // Always re-arm a preempt quantum from the IRQ path so boot / busy
    // main work keeps ~1 ms cadence. The idle loop stretches the deadline
    // to the next second via [`arm_timer_for_load`] immediately before `hlt`.
    crate::arch::apic::arm_oneshot_ms(crate::arch::apic::quantum_ms());

    // EOI before entering the next task (or returning to this one).
    crate::arch::end_timer_interrupt();

    next_ctx.unwrap_or(0)
}

/// True when THIS CPU owns at least one `RUNNING` thread.
fn cpu_has_runnable() -> bool {
    let my_cpu = crate::arch::cpu::current_index() as u8;
    interrupts::without_interrupts(|| {
        THREADS.lock().iter().any(|t| {
            t.owner == my_cpu && t.state.load(Ordering::Acquire) == STATE_RUNNING
        })
    })
}

/// Park the current user task until `timer_ticks` reaches `now + ms`.
///
/// Returns `Ok(())` after marking the task `WAITING` (caller must hand off).
/// No Cap required. `ms` is clamped to `1..=SLEEP_MS_MAX`.
pub(crate) fn task_sleep(ms: u64) -> Result<(), SysError> {
    let ms = ms.clamp(1, galexy_abi::SLEEP_MS_MAX);
    let slot = current_slot();
    if slot == 0 {
        return Err(SysError::BadCap);
    }
    let now = crate::arch::timer_ticks();
    let deadline = now.saturating_add(ms);
    interrupts::without_interrupts(|| {
        let mut threads = THREADS.lock();
        let thread = threads.get_mut(slot - 1).ok_or(SysError::BadCap)?;
        if !thread.is_user || thread.state.load(Ordering::Acquire) != STATE_RUNNING {
            return Err(SysError::BadCap);
        }
        thread.wait_child_slot.store(0, Ordering::Relaxed);
        thread.wait_for_exit.store(false, Ordering::Relaxed);
        thread.sleep_deadline.store(deadline, Ordering::Release);
        thread.state.store(STATE_WAITING, Ordering::Release);
        Ok(())
    })
}

/// Wake sleepers whose deadline is due. Call under the timer path after
/// `timer_ticks` advances.
fn wake_due_sleepers(threads: &mut [Thread], now: u64) {
    for thread in threads.iter_mut() {
        if thread.state.load(Ordering::Acquire) != STATE_WAITING {
            continue;
        }
        let deadline = thread.sleep_deadline.load(Ordering::Acquire);
        if deadline == 0 || now < deadline {
            continue;
        }
        clear_wait_fields(thread);
        stamp_waiter_frame(thread, SyscallResult::ok(0));
        thread.state.store(STATE_RUNNING, Ordering::Release);
    }
}

const IO_NONE: u8 = 0;
const IO_KEYBOARD: u8 = 1;
const IO_PIPE_READ: u8 = 2;
const IO_PIPE_WRITE: u8 = 3;

fn clear_wait_fields(thread: &Thread) {
    thread.wait_child_slot.store(0, Ordering::Relaxed);
    thread.wait_for_exit.store(false, Ordering::Relaxed);
    thread.sleep_deadline.store(0, Ordering::Relaxed);
    thread.io_kind.store(IO_NONE, Ordering::Relaxed);
    thread.io_pipe.store(0, Ordering::Relaxed);
    thread.io_addr.store(0, Ordering::Relaxed);
    thread.io_len.store(0, Ordering::Relaxed);
    thread.io_cap.store(0, Ordering::Relaxed);
}

fn park_io(
    threads: &mut [Thread],
    slot: usize,
    kind: u8,
    pipe_id: u8,
    addr: u64,
    len: u32,
    cap_bits: u64,
) {
    let thread = &mut threads[slot - 1];
    thread.wait_child_slot.store(0, Ordering::Relaxed);
    thread.wait_for_exit.store(false, Ordering::Relaxed);
    thread.sleep_deadline.store(0, Ordering::Relaxed);
    thread.io_kind.store(kind, Ordering::Release);
    thread.io_pipe.store(pipe_id, Ordering::Relaxed);
    thread.io_addr.store(addr, Ordering::Relaxed);
    thread.io_len.store(len, Ordering::Relaxed);
    thread.io_cap.store(cap_bits, Ordering::Relaxed);
    thread.state.store(STATE_WAITING, Ordering::Release);
}

/// Wake keyboard readers parked on `tty` (Milestone 57). Completes the
/// pending read into their user buffer when keys are available.
pub fn wake_keyboard_waiters(tty: u8) {
    interrupts::without_interrupts(|| {
        let threads = THREADS.lock();
        for i in 0..threads.len() {
            if threads[i].state.load(Ordering::Acquire) != STATE_WAITING {
                continue;
            }
            if threads[i].io_kind.load(Ordering::Acquire) != IO_KEYBOARD {
                continue;
            }
            if threads[i].tty != tty {
                continue;
            }
            let addr = threads[i].io_addr.load(Ordering::Acquire);
            let len = threads[i].io_len.load(Ordering::Acquire) as usize;
            if len == 0 || addr == 0 {
                clear_wait_fields(&threads[i]);
                stamp_waiter_frame(&threads[i], SyscallResult::ok(0));
                threads[i].state.store(STATE_RUNNING, Ordering::Release);
                continue;
            }
            // Fill from the keyboard queue while the waiter is still parked
            // (its CR3 is not current — copy via phys map / user walk of
            // the waiter's tree). Use the same staging path as the syscall:
            // temporarily enter the waiter's CR3 is heavy; instead re-stamp
            // "retry" by rewinding is avoided — we fill via with_table.
            let cr3 = threads[i].cr3.load(Ordering::Acquire);
            let n = fill_keyboard_into_user(tty, cr3, addr, len);
            if n == 0 {
                // Still empty (spurious wake) — stay parked.
                continue;
            }
            clear_wait_fields(&threads[i]);
            stamp_waiter_frame(&threads[i], SyscallResult::ok(n as u64));
            threads[i].state.store(STATE_RUNNING, Ordering::Release);
        }
    });
}

fn fill_keyboard_into_user(tty: u8, cr3: u64, addr: u64, len: usize) -> usize {
    let mut staged = [0u8; 256];
    let max = len.min(staged.len());
    let mut filled = 0usize;
    while filled < max {
        let Some(c) = crate::drivers::keyboard::pop_key_tty(tty) else {
            break;
        };
        let mut tmp = [0u8; 4];
        let encoded = c.encode_utf8(&mut tmp);
        if filled + encoded.len() > max {
            crate::drivers::keyboard::unget_key_tty(tty, c);
            break;
        }
        staged[filled..filled + encoded.len()].copy_from_slice(encoded.as_bytes());
        filled += encoded.len();
    }
    if filled == 0 {
        return 0;
    }
    // Copy into the waiter's address space via its page tables.
    if cr3 == 0 {
        return 0;
    }
    let root = PhysFrame::from_start_address(PhysAddr::new(cr3)).expect("waiter cr3");
    // SAFETY: waiter FreshL4 root; not necessarily active.
    let ok = unsafe {
        crate::arch::mm::with_table(root, |mapper| {
            copy_to_user_via(mapper, addr, &staged[..filled])
        })
    };
    if ok {
        filled
    } else {
        0
    }
}

fn copy_to_user_via(
    mapper: &mut x86_64::structures::paging::OffsetPageTable<'_>,
    addr: u64,
    src: &[u8],
) -> bool {
    use x86_64::structures::paging::{Page, Size4KiB};
    let mut done = 0usize;
    while done < src.len() {
        let va = VirtAddr::new(addr + done as u64);
        let page = Page::<Size4KiB>::containing_address(va);
        let Ok(frame) = mapper.translate_page(page) else {
            return false;
        };
        let off = (va.as_u64() as usize) & 0xFFF;
        let room = (0x1000 - off).min(src.len() - done);
        let dst = crate::arch::mm::frame_virt(frame.start_address()).as_mut_ptr::<u8>();
        // SAFETY: frame backing a present user mapping; exclusive while waiter parks.
        unsafe {
            core::ptr::copy_nonoverlapping(src[done..].as_ptr(), dst.add(off), room);
        }
        done += room;
    }
    true
}

/// Wake pipe readers (data or EOF) / writers (space or closed).
pub fn wake_pipe_waiters(id: u8) {
    interrupts::without_interrupts(|| {
        let mut threads = THREADS.lock();
        // Alternate reader/writer passes: a completed write frees data for
        // readers, and a completed read frees space for writers.
        for _ in 0..MAX_THREADS {
            let mut readers = alloc::vec::Vec::new();
            let mut writers = alloc::vec::Vec::new();
            for (i, t) in threads.iter().enumerate() {
                if t.state.load(Ordering::Acquire) != STATE_WAITING {
                    continue;
                }
                if t.io_pipe.load(Ordering::Acquire) != id {
                    continue;
                }
                match t.io_kind.load(Ordering::Acquire) {
                    IO_PIPE_READ => readers.push(i),
                    IO_PIPE_WRITE => writers.push(i),
                    _ => {}
                }
            }
            let before = readers.len() + writers.len();
            if before == 0 {
                break;
            }
            for i in readers {
                complete_pipe_read(&mut threads, i);
            }
            for i in writers {
                complete_pipe_write(&mut threads, i);
            }
            let mut still = 0usize;
            for t in threads.iter() {
                if t.state.load(Ordering::Acquire) != STATE_WAITING {
                    continue;
                }
                if t.io_pipe.load(Ordering::Acquire) != id {
                    continue;
                }
                let k = t.io_kind.load(Ordering::Acquire);
                if k == IO_PIPE_READ || k == IO_PIPE_WRITE {
                    still += 1;
                }
            }
            if still >= before {
                break; // no forward progress (still WouldBlock)
            }
        }
    });
}

fn complete_pipe_read(threads: &mut [Thread], index: usize) {
    let addr = threads[index].io_addr.load(Ordering::Acquire);
    let len = threads[index].io_len.load(Ordering::Acquire) as usize;
    let id = threads[index].io_pipe.load(Ordering::Acquire);
    let cap_bits = threads[index].io_cap.load(Ordering::Acquire);
    let cr3 = threads[index].cr3.load(Ordering::Acquire);
    let mut staged = [0u8; 256];
    let max = len.min(staged.len());
    let result = match pipe::try_read(id, &mut staged[..max]) {
        Ok(pipe::ReadResult::Ready(0)) => SyscallResult::ok(0),
        Ok(pipe::ReadResult::Ready(n)) => {
            let root = PhysFrame::from_start_address(PhysAddr::new(cr3)).expect("cr3");
            let ok = unsafe {
                crate::arch::mm::with_table(root, |mapper| {
                    copy_to_user_via(mapper, addr, &staged[..n])
                })
            };
            if ok {
                SyscallResult::ok(n as u64)
            } else {
                SyscallResult::err(SysError::BadBuffer)
            }
        }
        Ok(pipe::ReadResult::Eof) => SyscallResult::ok(0),
        Ok(pipe::ReadResult::WouldBlock) => return, // stay parked
        Err(e) => SyscallResult::err(e),
    };
    let _ = cap_bits;
    clear_wait_fields(&threads[index]);
    stamp_waiter_frame(&threads[index], result);
    threads[index].state.store(STATE_RUNNING, Ordering::Release);
}

fn complete_pipe_write(threads: &mut [Thread], index: usize) {
    let addr = threads[index].io_addr.load(Ordering::Acquire);
    let len = threads[index].io_len.load(Ordering::Acquire) as usize;
    let id = threads[index].io_pipe.load(Ordering::Acquire);
    let cr3 = threads[index].cr3.load(Ordering::Acquire);
    let mut staged = [0u8; 256];
    let max = len.min(staged.len());
    // Copy FROM user into staging.
    let root = PhysFrame::from_start_address(PhysAddr::new(cr3)).expect("cr3");
    let ok = unsafe {
        crate::arch::mm::with_table(root, |mapper| copy_from_user_via(mapper, addr, &mut staged[..max]))
    };
    if !ok {
        clear_wait_fields(&threads[index]);
        stamp_waiter_frame(&threads[index], SyscallResult::err(SysError::BadBuffer));
        threads[index].state.store(STATE_RUNNING, Ordering::Release);
        return;
    }
    let result = match pipe::try_write(id, &staged[..max]) {
        Ok(pipe::WriteResult::Ready(n)) => SyscallResult::ok(n as u64),
        Ok(pipe::WriteResult::WouldBlock) => return,
        Ok(pipe::WriteResult::Closed) => SyscallResult::err(SysError::Unsupported),
        Err(e) => SyscallResult::err(e),
    };
    clear_wait_fields(&threads[index]);
    stamp_waiter_frame(&threads[index], result);
    threads[index].state.store(STATE_RUNNING, Ordering::Release);
}

fn copy_from_user_via(
    mapper: &mut x86_64::structures::paging::OffsetPageTable<'_>,
    addr: u64,
    dst: &mut [u8],
) -> bool {
    use x86_64::structures::paging::{Page, Size4KiB};
    let mut done = 0usize;
    while done < dst.len() {
        let va = VirtAddr::new(addr + done as u64);
        let page = Page::<Size4KiB>::containing_address(va);
        let Ok(frame) = mapper.translate_page(page) else {
            return false;
        };
        let off = (va.as_u64() as usize) & 0xFFF;
        let room = (0x1000 - off).min(dst.len() - done);
        let src = crate::arch::mm::frame_virt(frame.start_address()).as_ptr::<u8>();
        unsafe {
            core::ptr::copy_nonoverlapping(src.add(off), dst[done..].as_mut_ptr(), room);
        }
        done += room;
    }
    true
}

/// Park the current task on an empty keyboard read. Caller must hand off.
pub(crate) fn task_park_keyboard(cap_bits: u64, addr: u64, len: u32) -> Result<(), SysError> {
    let slot = current_slot();
    if slot == 0 {
        return Err(SysError::BadCap);
    }
    interrupts::without_interrupts(|| {
        let mut threads = THREADS.lock();
        let thread = threads.get_mut(slot - 1).ok_or(SysError::BadCap)?;
        if !thread.is_user || thread.state.load(Ordering::Acquire) != STATE_RUNNING {
            return Err(SysError::BadCap);
        }
        park_io(&mut threads, slot, IO_KEYBOARD, 0, addr, len, cap_bits);
        Ok(())
    })
}

/// Park on a pipe end. `read` selects reader vs writer wait.
pub(crate) fn task_park_pipe(
    pipe_id: u8,
    read: bool,
    cap_bits: u64,
    addr: u64,
    len: u32,
) -> Result<(), SysError> {
    let slot = current_slot();
    if slot == 0 {
        return Err(SysError::BadCap);
    }
    let kind = if read { IO_PIPE_READ } else { IO_PIPE_WRITE };
    interrupts::without_interrupts(|| {
        let mut threads = THREADS.lock();
        let thread = threads.get_mut(slot - 1).ok_or(SysError::BadCap)?;
        if !thread.is_user || thread.state.load(Ordering::Acquire) != STATE_RUNNING {
            return Err(SysError::BadCap);
        }
        park_io(&mut threads, slot, kind, pipe_id, addr, len, cap_bits);
        Ok(())
    })
}

/// Cancel I/O / sleep waits on a slot with [`SysError::Interrupted`].
fn interrupt_io_waiter(threads: &mut [Thread], index: usize) {
    if threads[index].state.load(Ordering::Acquire) != STATE_WAITING {
        return;
    }
    let io = threads[index].io_kind.load(Ordering::Acquire);
    let sleeping = threads[index].sleep_deadline.load(Ordering::Acquire) != 0;
    if io == IO_NONE && !sleeping {
        return; // Cap-wait / spawn — leave for exit wake
    }
    clear_wait_fields(&threads[index]);
    stamp_waiter_frame(
        &threads[index],
        SyscallResult::err(SysError::Interrupted),
    );
    threads[index].state.store(STATE_RUNNING, Ordering::Release);
}

/// Milliseconds until the nearest sleep deadline, if any sleeper exists.
fn ms_until_next_sleep() -> Option<u32> {
    let now = crate::arch::timer_ticks();
    interrupts::without_interrupts(|| {
        let threads = THREADS.lock();
        let mut best: Option<u64> = None;
        for thread in threads.iter() {
            if thread.state.load(Ordering::Acquire) != STATE_WAITING {
                continue;
            }
            let deadline = thread.sleep_deadline.load(Ordering::Acquire);
            if deadline == 0 {
                continue;
            }
            let remain = if deadline <= now {
                1
            } else {
                deadline - now
            };
            best = Some(match best {
                Some(b) => b.min(remain),
                None => remain,
            });
        }
        best.map(|ms| ms.min(u64::from(crate::arch::apic::IDLE_MAX_MS)) as u32)
    })
}

/// Reprogram the local LAPIC for the current load (call before `hlt`).
///
/// Busy → preempt quantum; idle → min(next whole second, next sleeper).
/// Device IRQs still wake the CPU early; the next halt re-arms. The IRQ
/// path itself always re-arms a quantum (preempt fairness).
pub fn arm_timer_for_load() {
    if cpu_has_runnable() {
        crate::arch::apic::arm_oneshot_ms(crate::arch::apic::quantum_ms());
    } else {
        let idle = crate::arch::apic::idle_deadline_ms();
        let ms = match ms_until_next_sleep() {
            Some(s) => idle.min(s).max(1),
            None => idle,
        };
        crate::arch::apic::arm_oneshot_ms(ms);
    }
}
