//! Scheduler (planned).
//!
//! Hook point: the PIT timer handler in `arch/timer.rs` (and the tick
//! counter) — when preemptive scheduling lands, the timer handler body swaps
//! to a schedule() call and per-task kernel stacks + context switching live
//! here. Nothing else in the kernel changes shape.
