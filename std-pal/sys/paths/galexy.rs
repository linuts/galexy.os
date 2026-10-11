//! Paths. galfs paths are relative to the task's root (`note`,
//! `box/leaf`, `owner@shared/…`); a leading `/` is accepted and ignored by
//! the kernel. There is no working directory syscall, so `getcwd` is `/`
//! and `chdir` is `Unsupported`. `current_exe` is the task's label.

use crate::ffi::{OsStr, OsString};
use crate::os::galexy::ffi::{OsStrExt, OsStringExt};
use crate::path::{self, Path, PathBuf};
use crate::sys::pal::{syscall, unsupported};
use crate::{fmt, io};

pub fn getcwd() -> io::Result<PathBuf> {
    Ok(PathBuf::from("/"))
}

pub fn chdir(_: &path::Path) -> io::Result<()> {
    unsupported()
}

/// `:`-separated paths. Spawn does not search `PATH` (a program name is a
/// ramdisk label), but `rustc` still joins one before it starts the linker.
pub struct SplitPaths<'a> {
    inner: core::slice::Split<'a, u8, fn(&u8) -> bool>,
}
pub struct SplitPathsRef<'a> {
    inner: core::slice::Split<'a, u8, fn(&u8) -> bool>,
}

fn is_sep(b: &u8) -> bool {
    *b == b':'
}

pub fn split_paths(unparsed: &OsStr) -> SplitPaths<'_> {
    SplitPaths {
        inner: unparsed.as_bytes().split(is_sep),
    }
}

pub fn split_paths_ref(unparsed: &OsStr) -> Option<SplitPathsRef<'_>> {
    Some(SplitPathsRef {
        inner: unparsed.as_bytes().split(is_sep),
    })
}

impl<'a> Iterator for SplitPaths<'a> {
    type Item = PathBuf;
    fn next(&mut self) -> Option<PathBuf> {
        self.inner
            .next()
            .map(|b| PathBuf::from(OsString::from_vec(b.to_vec())))
    }
}

impl<'a> Iterator for SplitPathsRef<'a> {
    type Item = &'a Path;
    fn next(&mut self) -> Option<&'a Path> {
        self.inner.next().map(|b| Path::new(OsStr::from_bytes(b)))
    }
}

#[derive(Debug)]
pub struct JoinPathsError;

pub fn join_paths<I, T>(paths: I) -> Result<OsString, JoinPathsError>
where
    I: Iterator<Item = T>,
    T: AsRef<OsStr>,
{
    let mut joined = Vec::new();
    for (i, path) in paths.enumerate() {
        let path = path.as_ref().as_bytes();
        if path.contains(&b':') {
            return Err(JoinPathsError);
        }
        if i > 0 {
            joined.push(b':');
        }
        joined.extend_from_slice(path);
    }
    Ok(OsString::from_vec(joined))
}

impl fmt::Display for JoinPathsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        "path segment contains separator `:`".fmt(f)
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
