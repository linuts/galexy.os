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
pub mod loader;
pub mod ramdisk;
pub mod syscalls;

use alloc::boxed::Box;
use alloc::collections::VecDeque;
use alloc::vec;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, AtomicUsize, Ordering};
use spin::Mutex;
use x86_64::instructions::interrupts;
use x86_64::structures::paging::{Mapper, Page, PageTableFlags, PhysFrame, Size4KiB};
use x86_64::{PhysAddr, VirtAddr};

use galexy_abi::{Cap, CapRights, SysError};

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

/// Initializes the scheduler (empty queues). Call before any spawn.
pub fn init() {
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
const STATE_WAITING: u8 = 3; // parked inside `spawn` until that child exits

/// Bytes kept for a thread's name. Spawn already rejects a longer name.
const NAME_CAP: usize = 64;

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
    /// Index into [`SCRATCH`]. The bytes are writable.
    Scratch(u8),
}

/// One open file. Archive bytes live in the bootloader's ramdisk; scratch
/// bytes live in the global table. Only the cursor is per-open.
#[derive(Clone, Copy)]
struct OpenFile {
    body: FileBody,
    offset: usize,
    /// Authoritative rights. The handle's upper half is a snapshot; a call
    /// is allowed only for the intersection of the two.
    rights: CapRights,
}

/// Scratch files the kernel will hold. A slot stays taken until reboot
/// (`close` drops the task's cap, not the bytes).
const SCRATCH_SLOTS: usize = 8;
/// Bytes one scratch file can hold. A longer `write` copies what fits.
const SCRATCH_BYTES: usize = 256;

/// One scratch file. Fixed name, fixed buffer: `create` never allocates.
#[derive(Clone, Copy)]
struct ScratchFile {
    used: bool,
    name: [u8; NAME_CAP],
    name_len: u8,
    data: [u8; SCRATCH_BYTES],
    len: u16,
}

impl ScratchFile {
    const fn empty() -> Self {
        Self {
            used: false,
            name: [0; NAME_CAP],
            name_len: 0,
            data: [0; SCRATCH_BYTES],
            len: 0,
        }
    }

    fn name_is(&self, name: &str) -> bool {
        let n = self.name_len as usize;
        self.used && n == name.len() && &self.name[..n] == name.as_bytes()
    }
}

struct ScratchTable {
    files: [ScratchFile; SCRATCH_SLOTS],
}

/// Global scratch files. Taken only while [`THREADS`] is already held
/// (lock order: `THREADS`, then this).
static SCRATCH: Mutex<ScratchTable> = Mutex::new(ScratchTable {
    files: [ScratchFile::empty(); SCRATCH_SLOTS],
});

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
    /// Idle stealing skips this thread. The interactive shell is resident
    /// on the BSP: the keyboard and the framebuffer have one consumer.
    no_steal: bool,
    /// When `state` is [`STATE_WAITING`], the child name this task is
    /// parked on. The bytes are written under `THREADS` before the state
    /// store; `wait_for_len` is what readers trust.
    wait_for: [u8; 64],
    wait_for_len: AtomicU8,
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
/// vec grows. Indexes of live threads do not move.
fn push_thread(thread: Thread) {
    let mut threads = THREADS.lock();
    if let Some(index) = (0..threads.len()).find(|&i| slot_reusable(&threads, i)) {
        // False until this thread's owner publishes a switch-out. Stored
        // before the record becomes RUNNING, so a steal scan cannot take
        // the slot on its first run.
        CTX_STABLE[index].store(false, Ordering::Release);
        threads[index] = thread;
        return;
    }
    assert!(
        threads.len() < MAX_THREADS,
        "sched: thread table full ({MAX_THREADS} live slots)"
    );
    CTX_STABLE[threads.len()].store(false, Ordering::Release);
    threads.push(thread);
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
        for t in threads.iter_mut() {
            if t.owner != my_cpu {
                continue; // another CPU's thread — its reaper owns it
            }
            if t.state
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
            // Canary check BEFORE the stack is freed: a deep overflow writes
            // the magic word last (stack grows downward, canary is at the
            // very bottom). User tasks: heap check applies to `kstack`
            // instead (their `stack` is empty; the user stack has no heap
            // canary — it's isolated pages).
            let canary_stack: *const u8 = if t.is_user {
                t.kstack.as_ptr()
            } else {
                t.stack.as_ptr()
            };
            let canary = unsafe { (canary_stack as *const u64).read_unaligned() };
            if canary != STACK_CANARY {
                panic!(
                    "reap: stack canary corrupted for thread '{}' (stack overflow)",
                    t.name()
                );
            }
            // File caps die with the task. The bytes stay in the ramdisk.
            t.files = [None; MAX_OPEN_FILES];
            // SAFETY: the fx area was leaked at spawn; its slot is a
            // tombstone now — no code will dereference it again.
            unsafe { drop(Box::from_raw(t.fx)) };
            t.fx = core::ptr::null_mut();
            // The saved context lives ON this stack; null it so any stray
            // reader fails loudly instead of jumping into freed memory.
            t.ctx.store(0, Ordering::Relaxed);
            // User tasks: their ENTIRE tree is reclaimed by a walk under
            // the task's own P4 entry (page-table frames AND data frames —
            // the unmap-per-page pass is gone; the tree is not CR3-active
            // here: tombstoned ⇒ the handoff/switch already moved CR3).
            let task_cr3 = t.cr3.swap(0, Ordering::AcqRel);
            if task_cr3 != 0 {
                // SAFETY: the address came from a real FreshL4 allocation.
                let root = PhysFrame::from_start_address(PhysAddr::new(task_cr3))
                    .expect("reap: corrupt task CR3");
                let count = mm::free_user_tree(root, t.user_p4);
                serial_println!("[sched] freed task '{}' tree: {} frame(s)", t.name(), count);
            }
            let stack = core::mem::take(&mut t.stack);
            drop(stack); // returns the 32 KiB to the heap
            let kstack = core::mem::take(&mut t.kstack);
            drop(kstack); // user tasks: kernel-mode stack back to the heap
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
}

pub(crate) fn register_user_task(init: TaskInit<'_>) {
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
            no_steal: init.no_steal,
            wait_for: [0; 64],
            wait_for_len: AtomicU8::new(0),
        });
    });
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
        push_thread(Thread {
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
            no_steal: false,
            wait_for: [0; 64],
            wait_for_len: AtomicU8::new(0),
        });
        serial_println!("[sched] thread '{}' ready (owner cpu {})", name, owner);
        owner
    })
}

/// User stack size in 4 KiB pages.
pub(crate) const USER_STACK_PAGES: usize = 4;
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
    interrupts::without_interrupts(|| {
        // The loader allocates. A syscall runs with interrupts off, so the
        // load stays on the main loop, which is the kernel table.
        assert!(
            mm::on_kernel_tree(),
            "spawn_user_task: must run on the kernel tree (main-loop context)"
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
        push_thread(Thread {
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
            no_steal: false,
            wait_for: [0; 64],
            wait_for_len: AtomicU8::new(0),
        });
        serial_println!(
            "[sched] user task '{}' ready (own tree cr3={:#x}, p4={}, code @ {:#x}, kstack top {:#x})",
            name,
            root.start_address().as_u64(),
            p4_index,
            region.as_u64(),
            kstack_top
        );
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

/// Calls `each` with every RUNNING thread's name and tick count.
///
/// The name is borrowed from the slot and is only valid inside `each`.
/// No allocation: the syscall path renders query text into a stack buffer
/// and must not grow the heap (a grow there broadcasts a shootdown).
pub(crate) fn for_running_threads(mut each: impl FnMut(&str, u64)) {
    interrupts::without_interrupts(|| {
        for thread in THREADS.lock().iter() {
            if thread.state.load(Ordering::Relaxed) == STATE_RUNNING {
                each(thread.name(), thread.ticks.load(Ordering::Relaxed));
            }
        }
    });
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
/// Foreground-job query for the shell's `run` prompt pacing: the pending
/// program's exit (syscall tombstone) flips it to false within one gate.
pub fn is_name_running(name: &str) -> bool {
    interrupts::without_interrupts(|| {
        THREADS
            .lock()
            .iter()
            .any(|t| t.state.load(Ordering::Relaxed) == STATE_RUNNING && t.name() == name)
    })
}

// (main_ticks moved into the per-CPU table above.)

/// One queued `spawn`. The syscall path only copies the name (it runs
/// IF=0); the main loop loads the ELF on the kernel page table.
struct PendingSpawn {
    name: [u8; 64],
    len: u8,
    armed: bool,
}

static PENDING_SPAWN: Mutex<PendingSpawn> = Mutex::new(PendingSpawn {
    name: [0; 64],
    len: 0,
    armed: false,
});

/// Queues `name` and parks the current task until that program exits.
///
/// The caller must already be a running user task. Lock order: this takes
/// `PENDING_SPAWN`, then `THREADS`.
pub(crate) fn task_spawn(name: &str) -> Result<(), SysError> {
    if name.len() > 64 {
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
        let thread = threads.get_mut(slot - 1).ok_or(SysError::BadCap)?;
        if !thread.is_user || thread.state.load(Ordering::Acquire) != STATE_RUNNING {
            return Err(SysError::BadCap);
        }
        pending.name[..name.len()].copy_from_slice(name.as_bytes());
        pending.len = name.len() as u8;
        pending.armed = true;
        thread.wait_for[..name.len()].copy_from_slice(name.as_bytes());
        thread
            .wait_for_len
            .store(name.len() as u8, Ordering::Relaxed);
        thread.state.store(STATE_WAITING, Ordering::Release);
        Ok(())
    })
}

/// Loads a queued program, if the shell has asked for one.
///
/// Runs from the main loop: that context is the kernel page table, which
/// `spawn_program` clones. The requesting task is already `WAITING`.
pub fn drain_spawn() {
    let name = interrupts::without_interrupts(|| {
        let mut pending = PENDING_SPAWN.lock();
        if !pending.armed {
            return None;
        }
        let len = pending.len as usize;
        let mut raw = [0u8; 64];
        raw[..len].copy_from_slice(&pending.name[..len]);
        pending.armed = false;
        let text = core::str::from_utf8(&raw[..len]).unwrap_or("");
        Some(alloc::string::String::from(text))
    });
    let Some(name) = name else {
        return;
    };
    if let Some(bytes) = ramdisk::find(&name) {
        loader::spawn_program(&name, bytes);
    } else {
        serial_println!("[sched] spawn '{}' missing at drain; waking waiter", name);
        interrupts::without_interrupts(|| {
            let threads = THREADS.lock();
            wake_waiters(&threads, &name);
        });
    }
}

/// Marks every task parked on `name` runnable again. `threads` is the
/// `THREADS` guard. A waiter with an empty name is not parked.
fn wake_waiters(threads: &[Thread], name: &str) {
    let bytes = name.as_bytes();
    for thread in threads.iter() {
        let n = thread.wait_for_len.load(Ordering::Relaxed) as usize;
        if n == 0 || n != bytes.len() || &thread.wait_for[..n] != bytes {
            continue;
        }
        if thread.state.load(Ordering::Acquire) != STATE_WAITING {
            continue;
        }
        thread.wait_for_len.store(0, Ordering::Relaxed);
        thread.state.store(STATE_RUNNING, Ordering::Release);
    }
}

/// Opens a ramdisk file for the current user task. `name` is the exact
/// archive entry (`banner.txt`, `hello`). The returned cap carries READ.
pub(crate) fn task_open(name: &str) -> Result<Cap, SysError> {
    let bytes = crate::sched::ramdisk::find(name).ok_or(SysError::NotFound)?;
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
        let rights = CapRights::READ;
        thread.files[index] = Some(OpenFile {
            body: FileBody::Archive(bytes),
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
pub(crate) fn task_read(cap: Cap, dst: &mut [u8]) -> Result<usize, SysError> {
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
            return Ok(0);
        }
        let start = file.offset;
        let n = match file.body {
            FileBody::Archive(bytes) => {
                let available = bytes.len().saturating_sub(start);
                let n = dst.len().min(available);
                dst[..n].copy_from_slice(&bytes[start..start + n]);
                n
            }
            FileBody::Scratch(index) => {
                let scratch = SCRATCH.lock();
                let stored = &scratch.files[index as usize];
                let available = (stored.len as usize).saturating_sub(start);
                let n = dst.len().min(available);
                dst[..n].copy_from_slice(&stored.data[start..start + n]);
                n
            }
        };
        file.offset = start + n;
        Ok(n)
    })
}

/// Appends `src` to a scratch file. An archive open is `Unsupported`.
///
/// The read cursor stays put, so a later `read` still starts at the
/// beginning. A write that does not fit is short: the count is the bytes
/// copied, and `0` means the buffer is already full.
pub(crate) fn task_write(cap: Cap, src: &[u8]) -> Result<usize, SysError> {
    let index = file_slot(cap)?;
    let slot = current_slot();
    if slot == 0 {
        return Err(SysError::BadCap);
    }
    interrupts::without_interrupts(|| {
        let mut threads = THREADS.lock();
        let thread = threads.get_mut(slot - 1).ok_or(SysError::BadCap)?;
        let file = thread.files[index].as_mut().ok_or(SysError::BadCap)?;
        let FileBody::Scratch(scratch_index) = file.body else {
            return Err(SysError::Unsupported);
        };
        let effective = file.rights.intersection(cap.rights());
        if !effective.contains(CapRights::WRITE) {
            return Err(SysError::AccessDenied);
        }
        if src.is_empty() {
            return Ok(0);
        }
        let mut scratch = SCRATCH.lock();
        let stored = &mut scratch.files[scratch_index as usize];
        let start = stored.len as usize;
        let n = src.len().min(SCRATCH_BYTES.saturating_sub(start));
        stored.data[start..start + n].copy_from_slice(&src[..n]);
        stored.len = (start + n) as u16;
        Ok(n)
    })
}

/// Creates a scratch file for the current user task and returns a cap
/// with READ and WRITE.
///
/// A ramdisk name, or a name already in the scratch table, is
/// `Unsupported`. The scratch table and the task's file table are both
/// fixed; either being full is `NoResource`.
pub(crate) fn task_create(name: &str) -> Result<Cap, SysError> {
    if crate::sched::ramdisk::find(name).is_some() {
        return Err(SysError::Unsupported);
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
        let mut scratch = SCRATCH.lock();
        if scratch.files.iter().any(|file| file.name_is(name)) {
            return Err(SysError::Unsupported);
        }
        let Some(scratch_index) = scratch.files.iter().position(|file| !file.used) else {
            return Err(SysError::NoResource);
        };
        let stored = &mut scratch.files[scratch_index];
        stored.used = true;
        stored.name = [0; NAME_CAP];
        stored.name[..name.len()].copy_from_slice(name.as_bytes());
        stored.name_len = name.len() as u8;
        stored.data = [0; SCRATCH_BYTES];
        stored.len = 0;
        let rights = CapRights::READ.union(CapRights::WRITE);
        thread.files[index] = Some(OpenFile {
            body: FileBody::Scratch(scratch_index as u8),
            offset: 0,
            rights,
        });
        Ok(Cap::new(galexy_abi::FILE_CAP_BASE + index as u64, rights))
    })
}

/// Drops one file capability belonging to the current task.
///
/// Close is possession of the slot, not a READ: the index names the open
/// in this task's table, and no other task has that table.
pub(crate) fn task_close(cap: Cap) -> Result<(), SysError> {
    let index = file_slot(cap)?;
    let slot = current_slot();
    if slot == 0 {
        return Err(SysError::BadCap);
    }
    interrupts::without_interrupts(|| {
        let mut threads = THREADS.lock();
        let thread = threads.get_mut(slot - 1).ok_or(SysError::BadCap)?;
        if thread.files[index].take().is_none() {
            return Err(SysError::BadCap);
        }
        Ok(())
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

/// The current rotation slot (0 = main loop; otherwise thread index + 1).
pub fn current_slot() -> usize {
    cpu_sched().current.load(Ordering::Relaxed)
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
        let threads = THREADS.lock();

        // Save the outgoing task's context + FPU state into its slot
        // (yield keeps it schedulable; exit tombstones it).
        let t = &threads[slot - 1];
        t.ctx.store(frame as u64, Ordering::Relaxed);
        context::fx_save(t.fx as *mut u8);
        if exit {
            let mut raw = [0u8; NAME_CAP];
            let n = t.name_len as usize;
            raw[..n].copy_from_slice(&t.name_bytes[..n]);
            t.state.store(STATE_EXITED, Ordering::Release);
            let name = core::str::from_utf8(&raw[..n]).unwrap_or("");
            serial_println!("[sched] task '{}' exited ({})", name, reason);
            wake_waiters(&threads, name);
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
    crate::arch::timer_tick(); // tick accounting + 1s heartbeat

    let me = cpu_sched();
    let next_ctx = interrupts::without_interrupts(|| {
        let mut threads = THREADS.lock();
        let my_cpu = crate::arch::cpu::current_index() as u8;
        let current = me.current.load(Ordering::Relaxed);

        // CPU-time attribution: this tick goes to whoever was running.
        match current {
            0 => {
                me.main_ticks.fetch_add(1, Ordering::Relaxed);
            }
            i => {
                threads[i - 1].ticks.fetch_add(1, Ordering::Relaxed);
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

    // EOI before entering the next task (or returning to this one).
    crate::arch::end_timer_interrupt();

    next_ctx.unwrap_or(0)
}
