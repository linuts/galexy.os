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
pub mod syscalls;

use alloc::boxed::Box;
use alloc::collections::VecDeque;
use alloc::vec;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, AtomicU8, AtomicUsize, Ordering};
use spin::Mutex;
use x86_64::instructions::interrupts;

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
const THREAD_STACK_SIZE: usize = 32 * 1024;

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
/// would shift indexes used by the timer switch (`CURRENT`, `LAST_SERVED`)
/// and corrupt rotation state mid-flight. Dead threads stay as `Freed`
/// tombstones (a few bytes each); slots/tids stay stable forever.
const STATE_RUNNING: u8 = 0;
const STATE_EXITED: u8 = 1; // returned from its entry; reaped by the main loop
const STATE_FREED: u8 = 2; // stack + fx freed; rotation-skipped tombstone

/// Magic word painted at the very bottom of each thread's stack (lowest
/// address). A stack that overflows far enough to corrupt the heap walks
/// downward through this word first — reaping detects the clobber.
const STACK_CANARY: u64 = 0xCA_7A_B1E_5_00D_F00D;

struct Thread {
    /// For status/ps display.
    name: &'static str,
    /// Lifecycle state (see STATE_* consts).
    state: AtomicU8,
    /// Saved context pointer; valid while the thread is NOT running.
    ctx: AtomicU64,
    /// Timer ticks charged to this thread (CPU-time attribution).
    ticks: AtomicU64,
    /// FXSAVE area — freed by the reaper once the thread exits.
    fx: *mut FxArea,
    /// Owns the stack memory; the saved context points inside it. The Vec
    /// struct may move, its buffer never does. Freed by the reaper.
    stack: Vec<u8>,
}

// SAFETY: `fx` is an exclusively-owned allocation, dereferenced only by the
// single-core timer switch under the IRQ gate; `stack` likewise is only
// freed from main-loop context.
unsafe impl Send for Thread {}

/// All preemptive threads, in round-robin order. Touched by the main loop
/// and the timer handler — access is IRQ-gated (see lock audit). Slots are
/// tombstones on death (see STATE_* docs); never removed.
static THREADS: Mutex<Vec<Thread>> = Mutex::new(Vec::new());
/// 0 = main loop is current; otherwise thread index + 1.
static CURRENT: AtomicUsize = AtomicUsize::new(0);
/// Main loop's saved context pointer (0 = not yet saved).
static MAIN_CTX: AtomicU64 = AtomicU64::new(0);
/// Round-robin cursor: index of the last-served thread (usize::MAX = none).
static LAST_SERVED: AtomicUsize = AtomicUsize::new(usize::MAX);

/// Main loop's FXSAVE area (FxArea is 16-aligned).
static MAIN_FX: Mutex<FxArea> = Mutex::new(FxArea::new());
/// CPU ticks charged to the main loop.
static MAIN_TICKS: AtomicU64 = AtomicU64::new(0);

/// Marks the CURRENT thread as exited. Called by the trampoline when a
/// thread's entry returns — the thread keeps executing until the next timer
/// tick (the rotation then skips it forever; the main loop's [`reap`]
/// frees its stack + fx area).
///
/// Must run on a thread's own stack — calling from the main loop is a bug
/// (panics; main has no Thread slot to tombstone).
pub fn thread_exit() {
    let slot = CURRENT.load(Ordering::Relaxed);
    assert!(slot != 0, "thread_exit: called from the main loop");
    interrupts::without_interrupts(|| {
        let threads = THREADS.lock();
        threads[slot - 1].state.store(STATE_EXITED, Ordering::Release);
        serial_println!("[sched] thread '{}' exited", threads[slot - 1].name);
    });
}

/// Frees resources of every exited thread (stack Vec + FXSAVE area) and
/// leaves `Freed` tombstones in place. Called from the main loop (e.g. the
/// shell's periodic sweep); IRQ-gated per the lock-audit rule.
///
/// Safety net on the way: if a thread's stack canary was clobbered (stack
/// overflow deep enough to leave its Vec), reaping panics loudly instead of
/// silently returning corrupted heap blocks to the allocator.
pub fn reap() {
    interrupts::without_interrupts(|| {
        let mut threads = THREADS.lock();
        let mut freed = 0usize;
        for t in threads.iter_mut() {
            if t.state
                .compare_exchange(STATE_EXITED, STATE_FREED, Ordering::AcqRel, Ordering::Relaxed)
                .is_err()
            {
                continue; // running or already freed
            }
            // Canary check BEFORE the stack is freed: a deep overflow writes
            // the magic word last (stack grows downward, canary is at the
            // very bottom).
            let stack_ptr = t.stack.as_ptr();
            let canary = unsafe { (stack_ptr as *const u64).read_unaligned() };
            if canary != STACK_CANARY {
                panic!("reap: stack canary corrupted for thread '{}' (stack overflow)", t.name);
            }
            // SAFETY: the fx area was leaked at spawn; its slot is a
            // tombstone now — no code will dereference it again.
            unsafe { drop(Box::from_raw(t.fx)) };
            t.fx = core::ptr::null_mut();
            // The saved context lives ON this stack; null it so any stray
            // reader fails loudly instead of jumping into freed memory.
            t.ctx.store(0, Ordering::Relaxed);
            let stack = core::mem::take(&mut t.stack);
            drop(stack); // returns the 32 KiB to the heap
            freed += 1;
        }
        drop(threads);
        if freed > 0 {
            serial_println!("[sched] reaped {} thread stack(s)", freed);
        }
    });
}

/// Spawns a preemptive kernel thread running `entry` (which parks if it
/// returns). Allocates + maps the thread stack; IRQ-gated while registering.
pub fn spawn_thread(name: &'static str, entry: extern "C" fn()) {
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
        THREADS.lock().push(Thread {
            name,
            state: AtomicU8::new(STATE_RUNNING),
            ctx: AtomicU64::new(ctx),
            ticks: AtomicU64::new(0),
            fx,
            stack,
        });
        serial_println!("[sched] thread '{}' ready", name);
    });
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

/// `(name, ticks)` for every RUNNING thread, in round-robin order.
///
/// IRQ-gated: the timer handler takes this same lock (lock-audit rule —
/// the gate lives in the API, not at call sites).
pub fn thread_stats() -> alloc::vec::Vec<(&'static str, u64)> {
    interrupts::without_interrupts(|| {
        THREADS
            .lock()
            .iter()
            .filter(|t| t.state.load(Ordering::Relaxed) == STATE_RUNNING)
            .map(|t| (t.name, t.ticks.load(Ordering::Relaxed)))
            .collect()
    })
}

/// CPU ticks charged to the main loop (slot 0 of the rotation).
pub fn main_ticks() -> u64 {
    MAIN_TICKS.load(Ordering::Relaxed)
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
    crate::arch::timer_tick(); // tick accounting + 1s heartbeat

    let next_ctx = interrupts::without_interrupts(|| {
        let threads = THREADS.lock();
        let current = CURRENT.load(Ordering::Relaxed);

        // CPU-time attribution: this tick goes to whoever was running.
        match current {
            0 => {
                MAIN_TICKS.fetch_add(1, Ordering::Relaxed);
            }
            i => {
                threads[i - 1].ticks.fetch_add(1, Ordering::Relaxed);
            }
        }

        // Save the outgoing task's context + FPU state.
        match current {
            0 => {
                let mut fx = MAIN_FX.lock();
                context::fx_save(&mut *fx as *mut FxArea as *mut u8);
                MAIN_CTX.store(frame as u64, Ordering::Relaxed);
            }
            i => {
                let t = &threads[i - 1];
                context::fx_save(t.fx as *mut u8);
                t.ctx.store(frame as u64, Ordering::Relaxed);
            }
        }

        // Unified round-robin over ALL participants: main (slot 0), then
        // threads (slots 1..=n). Exited/freed slots are skipped (tombstones;
        // see STATE_* docs). Switching "to main" = returning MAIN_CTX.
        let n = threads.len();
        if n == 0 {
            return None;
        }
        let last = LAST_SERVED.load(Ordering::Relaxed);
        let mut next_slot = if last == usize::MAX {
            1 // first tick ever: serve the first thread
        } else if last + 1 > n {
            0 // wrap to main
        } else {
            last + 1
        };
        // Skip tombstones; main (slot 0) is always eligible, so the scan
        // terminates after at most n+1 steps.
        let mut scans = n + 1;
        while scans > 0 {
            let eligible = match next_slot {
                0 => true,
                s => threads[s - 1].state.load(Ordering::Acquire) == STATE_RUNNING,
            };
            if eligible {
                break;
            }
            next_slot = if next_slot + 1 > n { 0 } else { next_slot + 1 };
            scans -= 1;
        }
        debug_assert!(scans > 0, "rotation scan terminated without main");
        LAST_SERVED.store(next_slot, Ordering::Relaxed);

        // Switching to ourselves (all other slots dead) = no switch.
        let current = CURRENT.load(Ordering::Relaxed);
        if next_slot == current {
            return None;
        }

        let (who, ctx, fx_ptr) = match next_slot {
            0 => (
                0usize,
                MAIN_CTX.load(Ordering::Relaxed),
                (&*MAIN_FX.lock()) as *const FxArea as u64,
            ),
            s => {
                let t = &threads[s - 1];
                (s, t.ctx.load(Ordering::Relaxed), t.fx as u64)
            }
        };
        CURRENT.store(who, Ordering::Relaxed);
        context::fx_restore(fx_ptr as *const u8);
        Some(ctx)
    });

    // EOI before entering the next task (or returning to this one).
    crate::arch::end_timer_interrupt();

    next_ctx.unwrap_or(0)
}
