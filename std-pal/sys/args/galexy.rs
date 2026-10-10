//! `env::args` from the spawn argument blob.
//!
//! The blob is NUL-separated argv without a program name (Milestone 66
//! layout). `args()[0]` is the task's own label from the self snapshot,
//! so Unix-shaped `args().skip(1)` code works unchanged. Empty pieces and
//! a trailing NUL are skipped; a blob with no NUL is one argument.

pub use super::common::Args;
use crate::ffi::OsString;
use crate::os::galexy::ffi::OsStringExt;
use crate::sys::pal::{startup, syscall};

pub fn args() -> Args {
    let mut out: Vec<OsString> = Vec::new();
    out.push(OsString::from(syscall::self_name().unwrap_or_default()));
    for piece in startup::arg_blob().split(|b| *b == 0) {
        if !piece.is_empty() {
            out.push(OsString::from_vec(piece.to_vec()));
        }
    }
    Args::new(out)
}
