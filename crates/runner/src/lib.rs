//! Host-side helpers shared by `cargo run` and the boot tests.
//!
//! Image building stays in `build.rs`. QEMU command lines for the suite
//! stay in `tests/common`. Accelerator selection lives here so both
//! entry points apply the same rule.

mod accel;

pub use accel::{configure_accel, use_kvm, Accel, KvmUnavailable};
