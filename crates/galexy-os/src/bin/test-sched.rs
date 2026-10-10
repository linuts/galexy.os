//! Integration test kernel: cooperative scheduler — two counting tasks,
//! deterministic yields; asserts interleaved round-robin and completion.

#![no_std]
#![no_main]

use bootloader_api::{entry_point, BootInfo};
use galexy_os::sync::Mutex;
use galexy_os::{
    drivers::screen,
    exit_qemu, println,
    sched::{self, TaskCtx, TaskStatus},
    serial_println, QemuExitCode,
};

entry_point!(test_main_entry, config = &galexy_os::BOOTLOADER_CONFIG);

/// Execution trace: the letter of every task step, in order.
static TRACE: Mutex<Trace> = Mutex::new(Trace::EMPTY);

struct Trace {
    len: usize,
    data: [u8; 16],
}

impl Trace {
    const EMPTY: Trace = Trace {
        len: 0,
        data: [0; 16],
    };

    fn push(&mut self, letter: u8) {
        if self.len < self.data.len() {
            self.data[self.len] = letter;
            self.len += 1;
        }
    }

    fn as_str(&self) -> &[u8] {
        &self.data[..self.len]
    }
}

/// Task A: yields 3 times, done on the 4th step.
fn counter_a(ctx: &mut TaskCtx) -> TaskStatus {
    TRACE.lock().push(b'A');
    ctx.data[0] += 1;
    if ctx.data[0] >= 4 {
        TaskStatus::Done
    } else {
        TaskStatus::Yield
    }
}

/// Task B: yields once, done on the 2nd step (interleaving probe).
fn counter_b(ctx: &mut TaskCtx) -> TaskStatus {
    TRACE.lock().push(b'B');
    ctx.data[0] += 1;
    if ctx.data[0] >= 2 {
        TaskStatus::Done
    } else {
        TaskStatus::Yield
    }
}

fn test_main_entry(boot_info: &'static mut BootInfo) -> ! {
    galexy_os::init();
    screen::init(boot_info);
    println!("[test-sched] running");
    serial_println!("[test-sched] running");

    // The scheduler's run queue is heap-backed: memory bring-up first.
    galexy_os::arch::mm::init(boot_info);

    sched::init();
    sched::spawn("counter-a", counter_a);
    sched::spawn("counter-b", counter_b);
    assert_eq!(sched::active_tasks(), 2, "two tasks queued");

    // Deterministic round-robin trace for [A, B]:
    //   A:1 yield, B:1 yield, A:2 yield, B:2 done, A:3 yield, A:4 done.
    for _ in 0..6 {
        sched::run_once();
    }
    assert_eq!(TRACE.lock().as_str(), b"ABABAA", "round-robin interleaving");

    // Both tasks completed; queue drained.
    assert_eq!(sched::active_tasks(), 0, "both tasks completed");
    assert_eq!(sched::spawned_total(), 2, "spawn accounting");

    // Re-spawn after the queue drained works.
    sched::spawn("counter-a-2", counter_a);
    assert_eq!(sched::active_tasks(), 1, "re-spawn queues a new task");
    for _ in 0..4 {
        sched::run_once();
    }
    assert_eq!(sched::active_tasks(), 0, "second task done after 4 steps");
    assert_eq!(sched::spawned_total(), 3, "spawn accounting");

    println!("[test-sched] all assertions passed");
    serial_println!("[test-sched] passed");
    exit_qemu(QemuExitCode::Success);
}
