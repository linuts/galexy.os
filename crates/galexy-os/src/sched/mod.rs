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

use alloc::boxed::Box;
use alloc::collections::VecDeque;
use alloc::vec;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
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

struct Thread {
    /// Saved context pointer; valid while the thread is NOT running.
    ctx: AtomicU64,
    /// Leaked (stable) FXSAVE area — freed never (kernel-lifetime threads).
    fx: *mut FxArea,
    /// Owns the stack memory; the saved context points inside it. The Vec
    /// struct may move, its buffer never does.
    _stack: Vec<u8>,
}

// SAFETY: `fx` is a leaked, exclusively-owned allocation, dereferenced only
// by the single-core timer switch under the IRQ gate.
unsafe impl Send for Thread {}

/// All preemptive threads, in round-robin order. Touched by the main loop
/// and the timer handler — access is IRQ-gated (see lock audit).
static THREADS: Mutex<Vec<Thread>> = Mutex::new(Vec::new());
/// 0 = main loop is current; otherwise thread index + 1.
static CURRENT: AtomicUsize = AtomicUsize::new(0);
/// Main loop's saved context pointer (0 = not yet saved).
static MAIN_CTX: AtomicU64 = AtomicU64::new(0);
/// Round-robin cursor: index of the last-served thread (usize::MAX = none).
static LAST_SERVED: AtomicUsize = AtomicUsize::new(usize::MAX);

/// Main loop's FXSAVE area (FxArea is 16-aligned).
static MAIN_FX: Mutex<FxArea> = Mutex::new(FxArea::new());

/// Spawns a preemptive kernel thread running `entry` (which parks if it
/// returns). Allocates + maps the thread stack; IRQ-gated while registering.
pub fn spawn_thread(name: &'static str, entry: extern "C" fn()) {
    interrupts::without_interrupts(|| {
        // Zero pages straight into the heap (no big stack temp).
        let stack = vec![0u8; THREAD_STACK_SIZE];
        // Round the stack top down to 16 bytes (SSE alignment).
        let top = (stack.as_ptr() as u64 + stack.len() as u64) & !0xF;
        let (cs, ss) = context::kernel_cs_ss();
        let ctx = unsafe { context::init_stack(top, entry, cs, ss) };
        let fx = Box::into_raw(Box::new(FxArea::new()));
        THREADS.lock().push(Thread {
            ctx: AtomicU64::new(ctx),
            fx,
            _stack: stack,
        });
        serial_println!("[sched] thread '{}' ready", name);
    });
}

/// Number of preemptive threads.
pub fn threads_count() -> usize {
    THREADS.lock().len()
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
        // threads (slots 1..=n). Switching "to main" = returning MAIN_CTX.
        let n = threads.len();
        if n == 0 {
            return None;
        }
        let last = LAST_SERVED.load(Ordering::Relaxed);
        let next_slot = if last == usize::MAX {
            1 // first tick ever: serve the first thread
        } else if last + 1 > n {
            0 // wrap to main
        } else {
            last + 1
        };
        LAST_SERVED.store(next_slot, Ordering::Relaxed);

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
