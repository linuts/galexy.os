//! Environment variables: there are none. The spawn ABI carries argv only
//! (Milestone 66), so `env::vars()` is empty rather than a panic,
//! `env::var` is `NotPresent`, and setting one is `Unsupported`.

use crate::ffi::{OsStr, OsString};
use crate::{fmt, io};

pub struct Env(());

impl fmt::Debug for Env {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_list().finish()
    }
}

impl Iterator for Env {
    type Item = (OsString, OsString);
    fn next(&mut self) -> Option<(OsString, OsString)> {
        None
    }
}

pub fn env() -> Env {
    Env(())
}

pub fn getenv(_: &OsStr) -> Option<OsString> {
    None
}

pub unsafe fn setenv(_: &OsStr, _: &OsStr) -> io::Result<()> {
    Err(io::const_error!(io::ErrorKind::Unsupported, "no environment on galexy"))
}

pub unsafe fn unsetenv(_: &OsStr) -> io::Result<()> {
    Err(io::const_error!(io::ErrorKind::Unsupported, "no environment on galexy"))
}
