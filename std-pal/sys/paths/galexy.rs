//! Paths. galfs paths are relative to the task's root (`note`,
//! `box/leaf`, `owner@shared/…`); a leading `/` is accepted and ignored by
//! the kernel. There is no working directory syscall, so `getcwd` is `/`
//! and `chdir` is `Unsupported`. `current_exe` is the task's label.

use crate::ffi::{OsStr, OsString};
use crate::marker::PhantomData;
use crate::path::{self, Path, PathBuf};
use crate::sys::pal::{syscall, unsupported};
use crate::{fmt, io};

pub fn getcwd() -> io::Result<PathBuf> {
    Ok(PathBuf::from("/"))
}

pub fn chdir(_: &path::Path) -> io::Result<()> {
    unsupported()
}

pub struct SplitPaths<'a>(!, PhantomData<&'a ()>);
pub struct SplitPathsRef<'a>(!, PhantomData<&'a ()>);

pub fn split_paths(_unparsed: &OsStr) -> SplitPaths<'_> {
    panic!("unsupported")
}

pub fn split_paths_ref(_unparsed: &OsStr) -> Option<SplitPathsRef<'_>> {
    None
}

impl<'a> Iterator for SplitPaths<'a> {
    type Item = PathBuf;
    fn next(&mut self) -> Option<PathBuf> {
        self.0
    }
}

impl<'a> Iterator for SplitPathsRef<'a> {
    type Item = &'a Path;
    fn next(&mut self) -> Option<&'a Path> {
        self.0
    }
}

#[derive(Debug)]
pub struct JoinPathsError;

pub fn join_paths<I, T>(_paths: I) -> Result<OsString, JoinPathsError>
where
    I: Iterator<Item = T>,
    T: AsRef<OsStr>,
{
    Err(JoinPathsError)
}

impl fmt::Display for JoinPathsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        "no PATH on galexy".fmt(f)
    }
}

impl crate::error::Error for JoinPathsError {}

pub fn current_exe() -> io::Result<PathBuf> {
    match syscall::self_name() {
        Some(name) => Ok(PathBuf::from(name)),
        None => unsupported(),
    }
}

/// Scratch space is the task's own galfs root; `/` is where a bare
/// `Create` lands.
pub fn temp_dir() -> PathBuf {
    PathBuf::from("/")
}

pub fn home_dir() -> Option<PathBuf> {
    None
}
