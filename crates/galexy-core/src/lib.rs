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
#[cfg(test)]
mod bitmap_test;
mod crc32;
#[cfg(test)]
mod crc32_test;
mod password;
mod ring;
#[cfg(test)]
mod ring_test;
mod tar;
#[cfg(test)]
mod tar_test;

pub use bitmap::Bitmap;
pub use crc32::crc32;
pub use password::{hash_eq, hash_password, salt_from_seed, HASH_LEN, SALT_LEN};
pub use ring::Ring;
pub use tar::TarCursor;
