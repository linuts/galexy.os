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

pub mod channel;
pub mod context;
pub mod demo;
pub mod galfs;
pub mod loader;
pub mod lockout;
pub mod pipe;
pub mod ramdisk;
pub mod syscalls;

mod iowait;
mod spawn;
mod task;
mod thread;

pub use iowait::arm_timer_capped;
pub use iowait::arm_timer_for_load;
pub use iowait::clear_io_block;
pub(crate) use iowait::console_budget_room;
pub(crate) use iowait::console_take_budget;
pub use iowait::current_is_init;
pub use iowait::current_slot;
pub use iowait::current_tty;
pub use iowait::fault_task_name;
pub use iowait::io_block_waiting;
pub use iowait::on_timer_tick;
pub use iowait::park_io_block;
pub use iowait::slot_is_user;
pub use iowait::syscall_handoff;
pub(crate) use iowait::task_granted;
pub(crate) use iowait::task_park_keyboard;
pub(crate) use iowait::task_park_pipe;
pub(crate) use iowait::task_sleep;
pub use iowait::wake_channel_waiters;
pub use iowait::wake_io_block;
pub use iowait::wake_keyboard_waiters;
pub use iowait::wake_pipe_waiters;
pub use spawn::drain_spawn;
pub use spawn::ensure_shell;
pub(crate) use spawn::for_each_scratch_path;
pub use spawn::idle_due;
pub use spawn::init_slot;
pub use spawn::init_unkillable;
pub use spawn::interrupt_foreground;
pub use spawn::is_console_shell_name;
pub use spawn::note_tty_input;
pub use spawn::poll_idle_logouts;
pub use spawn::seats_are_live;
pub use spawn::set_foreground_for_test;
pub use spawn::shell_is_live;
pub use spawn::spawn_all_shells;
pub use spawn::spawn_init;
pub use spawn::spawn_is_pending;
pub(crate) use spawn::task_give;
pub(crate) use spawn::task_grant;
pub(crate) use spawn::task_kill;
pub(crate) use spawn::task_login;
pub(crate) use spawn::task_logout;
pub(crate) use spawn::task_passwd;
pub(crate) use spawn::task_pipe;
pub(crate) use spawn::task_quota;
pub(crate) use spawn::task_revoke;
pub(crate) use spawn::task_seek;
pub(crate) use spawn::task_setquota;
pub(crate) use spawn::task_share;
pub(crate) use spawn::task_spawn;
pub(crate) use spawn::task_su;
pub(crate) use spawn::task_sync;
pub(crate) use spawn::task_tokens;
pub(crate) use spawn::task_unshare;
pub(crate) use spawn::task_useradd;
pub(crate) use spawn::task_userdel;
pub(crate) use spawn::task_users;
pub(crate) use spawn::task_wait;
pub(crate) use spawn::task_whoami;
pub use spawn::test_auth_flags;
pub use spawn::test_backdate_input;
pub use spawn::test_poll_idle_all;
pub use spawn::test_push_token;
pub use spawn::test_revoke_token;
pub use spawn::test_set_idle_limit;
pub(crate) use spawn::ARG_MAX;
pub(crate) use task::task_channel;
pub(crate) use task::task_close;
pub(crate) use task::task_create;
pub(crate) use task::task_init_rpc;
pub(crate) use task::task_map;
pub(crate) use task::task_open;
pub(crate) use task::task_read_ex;
pub(crate) use task::task_recv;
pub(crate) use task::task_remove;
pub(crate) use task::task_rename;
pub(crate) use task::task_send;
pub(crate) use task::task_stat;
pub(crate) use task::task_truncate;
pub(crate) use task::task_write_ex;
pub(crate) use task::IoOp;
pub(crate) use task::RecvOp;
pub(crate) use thread::for_running_threads;
pub(crate) use thread::for_user_tasks;
pub use thread::is_name_live;
pub use thread::is_name_running;
pub use thread::main_ticks;
pub use thread::parent_slot_of;
pub use thread::reap;
pub(crate) use thread::register_user_task;
pub use thread::slot_of_name;
pub use thread::spawn_frames_available;
pub use thread::spawn_thread;
pub use thread::spawn_user_launcher;
pub use thread::spawn_user_launcher_with;
pub use thread::spawn_user_task;
pub use thread::spawn_user_task_on;
pub use thread::spawn_user_with;
pub use thread::steal_count;
pub(crate) use thread::task_proc_inspect;
pub(crate) use thread::task_self_inspect;
pub use thread::thread_exit;
pub use thread::thread_owner;
pub use thread::thread_stats;
pub use thread::thread_tick_total;
pub use thread::threads_count;
pub use thread::unreaped_threads;
pub(crate) use thread::Grant;
pub(crate) use thread::Grants;
pub(crate) use thread::OpenFile;
pub(crate) use thread::TaskInit;
pub use thread::UserRegion;
pub(crate) use thread::CTX_STABLE;
pub use thread::IDLE_LOGOUT_MS;
pub use thread::SPAWN_FRAME_RESERVE;
pub(crate) use thread::THREAD_STACK_SIZE;
pub(crate) use thread::USER_STACK_OFFSET;
pub(crate) use thread::USER_STACK_PAGES;

use crate::sync::Mutex;
use alloc::collections::VecDeque;

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
