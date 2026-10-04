//! Demo tasks for the boot banner.
//!
//! - Cooperative tickers interleave visibly via round-robin sweeps.
//! - Preemptive threads interleave purely via the timer (they never yield).

use super::{spawn, spawn_thread, TaskCtx, TaskStatus};

/// How many steps between prints.
const PRINT_EVERY: u64 = 3;
/// Steps until the task is done.
const STEPS_TOTAL: u64 = 12;

/// Spawns the demo task pair and the demo thread pair.
pub fn spawn_all() {
    spawn("ticker-a", ticker_a);
    spawn("ticker-b", ticker_b);
    spawn_thread("thread-a", thread_a);
    spawn_thread("thread-b", thread_b);
}

fn ticker_a(ctx: &mut TaskCtx) -> TaskStatus {
    ticker(ctx, b'A')
}

fn ticker_b(ctx: &mut TaskCtx) -> TaskStatus {
    ticker(ctx, b'B')
}

/// Shared ticker state machine: counts steps, prints every `PRINT_EVERY`,
/// finishes after `STEPS_TOTAL`.
fn ticker(ctx: &mut TaskCtx, letter: u8) -> TaskStatus {
    ctx.data[0] += 1;
    let step = ctx.data[0];
    if step.is_multiple_of(PRINT_EVERY) {
        print!("[{}#{}]\n", letter as char, step);
    }
    if step >= STEPS_TOTAL {
        print!("[{} done]\n", letter as char);
        TaskStatus::Done
    } else {
        TaskStatus::Yield
    }
}

/// Busy-loop thread: increments a local counter, prints a marker every
/// `THREAD_PRINT_EVERY` million iterations. Never yields — the timer
/// preempts it.
extern "C" fn thread_a() {
    thread_loop(b'A')
}

extern "C" fn thread_b() {
    thread_loop(b'B')
}

/// Iterations between thread prints (tuned for ~20ms between markers).
const THREAD_PRINT_EVERY: u64 = 40_000_000;

fn thread_loop(letter: u8) -> ! {
    let mut i: u64 = 0;
    loop {
        i += 1;
        if i.is_multiple_of(THREAD_PRINT_EVERY) {
            print!("<{}@{}>\n", letter as char, i / THREAD_PRINT_EVERY);
        }
    }
}
