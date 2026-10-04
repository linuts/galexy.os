//! Kernel primitives shared across drivers and future sched/user code.
//!
//! Candidate for promotion to its own workspace crate (`galexy-core`) when a
//! second consumer (e.g. userspace) needs the same types — see the crate-lift
//! policy in `docs/DESIGN.md`.

mod ring;

pub use ring::Ring;
