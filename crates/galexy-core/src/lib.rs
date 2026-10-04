//! galexy-core: kernel primitives shared across drivers and future
//! sched/user code.
//!
//! Allocation-free, `no_std`, and platform-independent (no `arch`
//! dependencies) — the bottom of the galexy.os dependency stack. Host unit
//! tests run directly via `cargo test -p galexy-core`.

#![no_std]
#![deny(clippy::all)]
#![deny(missing_docs)]

mod bitmap;
mod ring;

#[cfg(test)]
mod bitmap_test;
#[cfg(test)]
mod ring_test;

pub use bitmap::Bitmap;
pub use ring::Ring;
