//! Galexy.OS-specific extensions to the primitives in the `std::ffi` module.
//!
//! `OsString::from_vec` / `into_vec` and `OsStr::from_bytes` / `as_bytes`,
//! exactly as on Unix: an OS string on this target is a byte string.

#![stable(feature = "rust1", since = "1.0.0")]

#[path = "../unix/ffi/os_str.rs"]
mod os_str;

#[stable(feature = "rust1", since = "1.0.0")]
pub use self::os_str::{OsStrExt, OsStringExt};
