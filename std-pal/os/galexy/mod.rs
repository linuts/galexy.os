//! Galexy.OS-specific definitions.
//!
//! `OsStr` / `OsString` are byte strings here (file names are ASCII
//! labels, arguments are what the launcher passed), so the Unix byte
//! extension traits apply unchanged.

#![stable(feature = "rust1", since = "1.0.0")]
#![deny(unsafe_op_in_unsafe_fn)]

pub mod ffi;

/// A prelude for conveniently writing platform-specific code.
///
/// Includes all extension traits, and some important type definitions.
#[stable(feature = "rust1", since = "1.0.0")]
pub mod prelude {
    #[doc(no_inline)]
    #[stable(feature = "rust1", since = "1.0.0")]
    pub use super::ffi::{OsStrExt, OsStringExt};
}
