//! Demo tasks for the boot banner: two tickers that interleave visibly,
//! proving the cooperative round-robin works.

use super::{spawn, TaskCtx, TaskStatus};

/// How many steps between prints.
const PRINT_EVERY: u64 = 3;
/// Steps until the task is done.
const STEPS_TOTAL: u64 = 12;

/// Spawns the demo task pair.
pub fn spawn_all() {
    spawn("ticker-a", ticker_a);
    spawn("ticker-b", ticker_b);
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
