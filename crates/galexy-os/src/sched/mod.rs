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
use x86_64::structures::paging::{Mapper, Page, PageTableFlags, PhysFrame, Size4KiB};
use x86_64::{PhysAddr, VirtAddr};

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
                panic!("reap: stack canary corrupted for thread '{}' (stack overflow)", t.name);
            }
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
                serial_println!("[sched] freed task '{}' tree: {} frame(s)", t.name, count);
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
            is_user: false,
            stack,
            kstack: Vec::new(),
            kstack_top: 0,
            cr3: AtomicU64::new(0),
            user_p4: 0,
        });
        serial_println!("[sched] thread '{}' ready", name);
    });
}

/// User stack size in 4 KiB pages.
const USER_STACK_PAGES: usize = 4;
/// User stack offset inside the task's P4 region (1 GiB in — keeps the
/// code page and stack far apart; the region is 512 GiB).
const USER_STACK_OFFSET: u64 = 1 << 30;
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
pub fn spawn_user_task(
    name: &'static str,
    build: impl FnOnce(UserRegion) -> Vec<u8>,
) -> UserRegion {
    interrupts::without_interrupts(|| {
        // Spawn MUST run on the kernel tree: a FreshL4 clones whatever is
        // active, and user mappings live only in task trees from now on.
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
        let fab_vaddr =
            mm::frame_virt(stack_frames[USER_STACK_PAGES - 1].start_address()) + 4096;
        let (cs, ss) = context::user_cs_ss();
        let ctx = unsafe {
            context::init_user_frame(fab_vaddr.as_u64(), region.as_u64(), cs, ss)
        };

        // Kernel-mode stack for ring 3→0 crossings: heap-backed, 32 KiB,
        // canary at the bottom.
        let mut kstack = vec![0u8; THREAD_STACK_SIZE];
        kstack[..8].copy_from_slice(&STACK_CANARY.to_le_bytes());
        let kstack_top = (kstack.as_ptr() as u64 + kstack.len() as u64) & !0xF;

        let fx = Box::into_raw(Box::new(FxArea::new()));
        THREADS.lock().push(Thread {
            name,
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
        });
        serial_println!(
            "[sched] user task '{}' ready (own tree cr3={:#x}, p4={}, code @ {:#x}, kstack top {:#x})",
            name,
            root.start_address().as_u64(),
            p4_index,
            region.as_u64(),
            kstack_top
        );
        granted
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

/// CPU ticks charged to the main loop (slot 0).
pub fn main_ticks() -> u64 {
    MAIN_TICKS.load(Ordering::Relaxed)
}

/// The current rotation slot (0 = main loop; otherwise thread index + 1).
pub fn current_slot() -> usize {
    CURRENT.load(Ordering::Relaxed)
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
pub unsafe fn syscall_handoff(frame: *mut context::Context, exit: bool, reason: &'static str) -> u64 {
    let slot = CURRENT.load(Ordering::Relaxed);
    assert!(slot != 0, "syscall_handoff: no task current (cpl bug?)");
    let pending_ctx = interrupts::without_interrupts(|| {
        let threads = THREADS.lock();

        // Save the outgoing task's context + FPU state into its slot
        // (yield keeps it schedulable; exit tombstones it).
        let t = &threads[slot - 1];
        t.ctx.store(frame as u64, Ordering::Relaxed);
        context::fx_save(t.fx as *mut u8);
        if exit {
            t.state.store(STATE_EXITED, Ordering::Release);
            serial_println!("[sched] task '{}' exited ({})", t.name, reason);
        }

        // Advance the rotation: first eligible slot strictly after the
        // outgoing one (main at slot 0 is always the fallback, and the
        // caller is never main — so this scan ALWAYS finds a switch).
        let n = threads.len();
        let mut cand = if slot >= n { 0 } else { slot + 1 };
        let mut scans = n + 1;
        while scans > 0 {
            let eligible =
                cand == 0 || threads[cand - 1].state.load(Ordering::Acquire) == STATE_RUNNING;
            if eligible {
                break;
            }
            cand = if cand + 1 > n { 0 } else { cand + 1 };
            scans -= 1;
        }
        debug_assert!(scans > 0, "handoff rotation scan unwound without main");
        LAST_SERVED.store(cand, Ordering::Relaxed);

        let (who, ctx, rsp0, cr3) = match cand {
            0 => (0usize, MAIN_CTX.load(Ordering::Relaxed), None, 0),
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
        CURRENT.store(who, Ordering::Relaxed);
        let fx_ptr = match cand {
            0 => (&*MAIN_FX.lock()) as *const FxArea as u64,
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

        let (who, ctx, fx_ptr, rsp0, cr3) = match next_slot {
            0 => (
                0usize,
                MAIN_CTX.load(Ordering::Relaxed),
                (&*MAIN_FX.lock()) as *const FxArea as u64,
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
        // ring 3→0 crossings (timer IRQ via TSS.RSP0, later the syscall
        // entry via the kstack registry) must push onto ITS OWN kernel
        // stack. Kernel threads/main reset the registry. (Step B: CR3 is
        // installed for the incoming task — the kernel half is shared by
        // every task table, so the switch is safe mid-flight.)
        crate::arch::syscall::set_task_kstack(rsp0.unwrap_or(0));
        if let Some(top) = rsp0 {
            crate::arch::set_tss_rsp0(VirtAddr::new(top));
        }
        enter_task_cr3(cr3);
        CURRENT.store(who, Ordering::Relaxed);
        context::fx_restore(fx_ptr as *const u8);
        Some(ctx)
    });

    // EOI before entering the next task (or returning to this one).
    crate::arch::end_timer_interrupt();

    next_ctx.unwrap_or(0)
}
