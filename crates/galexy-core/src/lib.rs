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
mod console_cut;
#[cfg(test)]
mod console_cut_test;
mod crc32;
#[cfg(test)]
mod crc32_test;
mod lockout;
#[cfg(test)]
mod lockout_test;
mod password;
mod path;
#[cfg(test)]
mod path_test;
mod ring;
#[cfg(test)]
mod ring_test;
mod tar;
#[cfg(test)]
mod tar_test;

pub use bitmap::Bitmap;
pub use console_cut::console_commit_len;
pub use crc32::crc32;
pub use lockout::{
    FailNote, LoginLockout, LOCKOUT_ACTORS, LOCKOUT_COOLDOWN_MS, LOCKOUT_MAX_FAILS, LOCKOUT_NAME,
    LOCKOUT_TTYS,
};
pub use password::{HASH_LEN, SALT_LEN};
pub use path::{component_ok, parse_path, ParsedPath, MAX_DEPTH, NAME_CAP};
pub use ring::Ring;
pub use tar::TarCursor;
