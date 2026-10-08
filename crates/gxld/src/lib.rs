//! gxld — the Galexy static ELF64 linker.
//!
//! Turns x86_64 relocatable objects and `ar` archives of them into the
//! static, non-PIE `ET_EXEC` that `sched/loader.rs` maps: three
//! page-aligned W^X `PT_LOAD`s (`R`, `RX`, `RW`) at
//! `galexy_abi::USER_IMAGE_BASE`. Static only — no `PT_INTERP`, no
//! `PT_DYNAMIC`, no PLT. Plan and scope: `docs/LINKER.md`.
//!
//! The crate is `no_std + alloc`: inputs are byte slices, the output is a
//! `Vec<u8>`. The `std` feature adds the command-line binary.

#![no_std]
#![deny(missing_docs)]

extern crate alloc;
#[cfg(feature = "std")]
extern crate std;

pub mod args;
pub mod elf;
pub mod error;
mod input;
mod link;

pub use elf::validate;
pub use error::{Error, Result};
pub use link::{link, Input, Options};

/// Linker name and version for `--version`.
pub const VERSION: &str = concat!("gxld ", env!("CARGO_PKG_VERSION"));

#[cfg(test)]
mod tests;
