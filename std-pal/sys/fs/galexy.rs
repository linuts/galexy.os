//! `std::fs` over the kernel file syscalls.
//!
//! Paths are galfs paths (`note`, `box/leaf`, `owner@shared/x`; a leading
//! `/` is accepted). Bare ramdisk names (`banner.txt`) open read-only.
//! `Open` yields READ|WRITE on a galfs file, `Create` makes a file (or a
//! directory when the path ends in `/`), `Stat` answers metadata, `Remove`
//! deletes a file or an empty directory, and `Write` is positional
//! (Milestone 70) so `seek` + `write` and `append` behave.
//!
//! No timestamps, permissions, symlinks, hard links, or locks: those are
//! `Unsupported`. `read_dir` is built from the `files` snapshot (one
//! kernel read, at most 1 KiB — the same list `ls` shows).

use crate::ffi::OsString;
use crate::fs::TryLockError;
use crate::io::{self, BorrowedCursor, IoSlice, IoSliceMut, SeekFrom};
use crate::os::galexy::ffi::{OsStrExt, OsStringExt};
use crate::path::{Path, PathBuf};
pub use crate::sys::fs::common::{Dir, ExtraHomeDirs, ExtraMediaDirs};
use crate::sys::pal::abi::{self, Cap, SysError};
use crate::sys::pal::{syscall, unsupported};
use crate::sys::time::SystemTime;
use crate::{fmt, mem};

pub struct File {
    cap: Cap,
}

#[derive(Clone, Copy)]
pub struct FileAttr {
    size: u64,
    kind: FileType,
}

pub struct ReadDir {
    dir: PathBuf,
    entries: crate::vec::IntoIter<(OsString, FileType)>,
}

pub struct DirEntry {
    dir: PathBuf,
    name: OsString,
    kind: FileType,
}

#[derive(Clone, Debug)]
pub struct OpenOptions {
    read: bool,
    write: bool,
    append: bool,
    truncate: bool,
    create: bool,
    create_new: bool,
}

#[derive(Copy, Clone, Debug, Default)]
pub struct FileTimes {}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct FilePermissions {
    readonly: bool,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct FileType {
    dir: bool,
}

#[derive(Debug)]
pub struct DirBuilder {}

fn err(e: SysError) -> io::Error {
    syscall::io_error(e)
}

/// The kernel-facing bytes of `path` (an empty path is `InvalidInput`).
fn path_bytes(path: &Path) -> io::Result<&[u8]> {
    let bytes = path.as_os_str().as_bytes();
    if bytes.is_empty() {
        return Err(io::const_error!(io::ErrorKind::InvalidInput, "empty path"));
    }
    Ok(bytes)
}

/// `path` without a leading `/` and without a trailing `/`.
fn normalized(path: &Path) -> &[u8] {
    let mut bytes = path.as_os_str().as_bytes();
    while let Some(rest) = bytes.strip_prefix(b"/") {
        bytes = rest;
    }
    while let Some(rest) = bytes.strip_suffix(b"/") {
        bytes = rest;
    }
    bytes
}

/// A bare ramdisk-style name: one component, no owner prefix.
fn is_bare_name(bytes: &[u8]) -> bool {
    !bytes.contains(&b'/') && !bytes.contains(&b'@')
}

impl FileAttr {
    pub fn size(&self) -> u64 {
        self.size
    }

    pub fn perm(&self) -> FilePermissions {
        FilePermissions { readonly: false }
    }

    pub fn file_type(&self) -> FileType {
        self.kind
    }

    pub fn modified(&self) -> io::Result<SystemTime> {
        unsupported()
    }

    pub fn accessed(&self) -> io::Result<SystemTime> {
        unsupported()
    }

    pub fn created(&self) -> io::Result<SystemTime> {
        unsupported()
    }
}

impl FilePermissions {
    pub fn readonly(&self) -> bool {
        self.readonly
    }

    pub fn set_readonly(&mut self, readonly: bool) {
        self.readonly = readonly;
    }
}

impl FileTimes {
    pub fn set_accessed(&mut self, _t: SystemTime) {}
    pub fn set_modified(&mut self, _t: SystemTime) {}
}

impl FileType {
    pub fn is_dir(&self) -> bool {
        self.dir
    }

    pub fn is_file(&self) -> bool {
        !self.dir
    }

    pub fn is_symlink(&self) -> bool {
        false
    }
}

impl fmt::Debug for ReadDir {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&self.dir, f)
    }
}

impl Iterator for ReadDir {
    type Item = io::Result<DirEntry>;

    fn next(&mut self) -> Option<io::Result<DirEntry>> {
        let (name, kind) = self.entries.next()?;
        Some(Ok(DirEntry { dir: self.dir.clone(), name, kind }))
    }
}

impl DirEntry {
    pub fn path(&self) -> PathBuf {
        self.dir.join(&self.name)
    }

    pub fn file_name(&self) -> OsString {
        self.name.clone()
    }

    pub fn metadata(&self) -> io::Result<FileAttr> {
        stat(&self.path())
    }

    pub fn file_type(&self) -> io::Result<FileType> {
        Ok(self.kind)
    }
}

impl OpenOptions {
    pub fn new() -> OpenOptions {
        OpenOptions {
            read: false,
            write: false,
            append: false,
            truncate: false,
            create: false,
            create_new: false,
        }
    }

    pub fn read(&mut self, read: bool) {
        self.read = read;
    }
    pub fn write(&mut self, write: bool) {
        self.write = write;
    }
    pub fn append(&mut self, append: bool) {
        self.append = append;
    }
    pub fn truncate(&mut self, truncate: bool) {
        self.truncate = truncate;
    }
    pub fn create(&mut self, create: bool) {
        self.create = create;
    }
    pub fn create_new(&mut self, create_new: bool) {
        self.create_new = create_new;
    }
}

impl File {
    /// Hands the capability to the caller (used to move a file into a
    /// child's stdio slot) without closing it on drop.
    pub(crate) fn into_cap(self) -> Cap {
        let cap = self.cap;
        mem::forget(self);
        cap
    }

    pub fn open(path: &Path, opts: &OpenOptions) -> io::Result<File> {
        let p = path_bytes(path)?;
        let writing = opts.write || opts.append;
        if (opts.truncate || opts.create || opts.create_new) && !writing {
            return Err(io::const_error!(
                io::ErrorKind::InvalidInput,
                "create/truncate need write or append"
            ));
        }
        if opts.create_new {
            // `Create` without replace fails on an existing name with
            // `Unsupported`; a `Stat` first gives the kind std expects.
            if exists(path)? {
                return Err(io::const_error!(io::ErrorKind::AlreadyExists, "file exists"));
            }
            let cap = syscall::create(p, false).map_err(err)?;
            return Ok(File { cap });
        }
        if opts.truncate {
            if !opts.create && !exists(path)? {
                return Err(err(SysError::NotFound));
            }
            let cap = syscall::create(p, true).map_err(err)?;
            return Ok(File { cap });
        }
        let cap = match syscall::open(p) {
            Ok(cap) => cap,
            Err(SysError::NotFound) if opts.create => syscall::create(p, false).map_err(err)?,
            Err(e) => return Err(err(e)),
        };
        let file = File { cap };
        // `Open` itself always succeeds on a readable name; a ramdisk file
        // comes back READ-only, so the write request is refused here.
        if writing && !cap.rights().contains(abi::CapRights::WRITE) {
            return Err(err(SysError::AccessDenied));
        }
        if opts.append {
            file.seek(SeekFrom::End(0))?;
        }
        Ok(file)
    }

    pub fn file_attr(&self) -> io::Result<FileAttr> {
        Ok(FileAttr { size: self.size_via_seek()?, kind: FileType { dir: false } })
    }

    /// Length without a path: park the cursor at the end and come back.
    fn size_via_seek(&self) -> io::Result<u64> {
        let here = syscall::seek(self.cap, 0, abi::SEEK_CUR).map_err(err)?;
        let end = syscall::seek(self.cap, 0, abi::SEEK_END).map_err(err)?;
        syscall::seek(self.cap, here as i64, abi::SEEK_SET).map_err(err)?;
        Ok(end)
    }

    pub fn fsync(&self) -> io::Result<()> {
        syscall::sync().map_err(err)
    }

    pub fn datasync(&self) -> io::Result<()> {
        self.fsync()
    }

    pub fn lock(&self) -> io::Result<()> {
        unsupported()
    }

    pub fn lock_shared(&self) -> io::Result<()> {
        unsupported()
    }

    pub fn try_lock(&self) -> Result<(), TryLockError> {
        Err(TryLockError::Error(crate::sys::pal::unsupported_err()))
    }

    pub fn try_lock_shared(&self) -> Result<(), TryLockError> {
        Err(TryLockError::Error(crate::sys::pal::unsupported_err()))
    }

    pub fn unlock(&self) -> io::Result<()> {
        unsupported()
    }

    pub fn truncate(&self, size: u64) -> io::Result<()> {
        syscall::truncate(self.cap, size).map_err(err)
    }

    pub fn read(&self, buf: &mut [u8]) -> io::Result<usize> {
        syscall::read(self.cap, buf).map_err(err)
    }

    pub fn read_vectored(&self, bufs: &mut [IoSliceMut<'_>]) -> io::Result<usize> {
        io::default_read_vectored(|b| self.read(b), bufs)
    }

    pub fn is_read_vectored(&self) -> bool {
        false
    }

    pub fn read_buf(&self, cursor: BorrowedCursor<'_, u8>) -> io::Result<()> {
        io::default_read_buf(|b| self.read(b), cursor)
    }

    pub fn write(&self, buf: &[u8]) -> io::Result<usize> {
        syscall::write(self.cap, buf).map_err(err)
    }

    pub fn write_vectored(&self, bufs: &[IoSlice<'_>]) -> io::Result<usize> {
        io::default_write_vectored(|b| self.write(b), bufs)
    }

    pub fn is_write_vectored(&self) -> bool {
        false
    }

    pub fn flush(&self) -> io::Result<()> {
        Ok(())
    }

    pub fn seek(&self, pos: SeekFrom) -> io::Result<u64> {
        let (offset, whence) = match pos {
            SeekFrom::Start(n) => (n as i64, abi::SEEK_SET),
            SeekFrom::Current(n) => (n, abi::SEEK_CUR),
            SeekFrom::End(n) => (n, abi::SEEK_END),
        };
        syscall::seek(self.cap, offset, whence).map_err(err)
    }

    pub fn size(&self) -> Option<io::Result<u64>> {
        Some(self.size_via_seek())
    }

    pub fn tell(&self) -> io::Result<u64> {
        syscall::seek(self.cap, 0, abi::SEEK_CUR).map_err(err)
    }

    pub fn duplicate(&self) -> io::Result<File> {
        // A Cap is one table slot; there is no `dup`.
        unsupported()
    }

    pub fn set_permissions(&self, _perm: FilePermissions) -> io::Result<()> {
        unsupported()
    }

    pub fn set_times(&self, _times: FileTimes) -> io::Result<()> {
        unsupported()
    }
}

impl Drop for File {
    fn drop(&mut self) {
        let _ = syscall::close(self.cap);
    }
}

impl DirBuilder {
    pub fn new() -> DirBuilder {
        DirBuilder {}
    }

    pub fn mkdir(&self, p: &Path) -> io::Result<()> {
        let mut bytes = normalized(p).to_vec();
        if bytes.is_empty() {
            return Err(io::const_error!(io::ErrorKind::InvalidInput, "empty path"));
        }
        bytes.push(b'/');
        match syscall::create(&bytes, false) {
            Ok(_) => Ok(()),
            // `Create` reports an existing name as `Unsupported`.
            Err(SysError::Unsupported) if exists(p)? => {
                Err(io::const_error!(io::ErrorKind::AlreadyExists, "directory exists"))
            }
            Err(e) => Err(err(e)),
        }
    }
}

impl fmt::Debug for File {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("File").field("cap", &self.cap.bits()).finish()
    }
}

/// One `files` snapshot: every path the task may list, one per line,
/// directories with a trailing `/`.
fn files_snapshot() -> io::Result<Vec<u8>> {
    // One shot, no cursor. The kernel cap is `FILES_SNAPSHOT_MAX`, which
    // is larger than the short `read` used for file bodies.
    let mut buf = vec![0u8; abi::FILES_SNAPSHOT_MAX];
    let n = syscall::syscall(
        abi::Syscall::Read,
        syscall::files_cap().bits(),
        buf.as_mut_ptr().addr() as u64,
        buf.len() as u64,
    )
    .map_err(err)? as usize;
    buf.truncate(n);
    Ok(buf)
}

pub fn readdir(p: &Path) -> io::Result<ReadDir> {
    let dir = normalized(p);
    if !dir.is_empty() && !stat(p)?.file_type().is_dir() {
        return Err(io::const_error!(io::ErrorKind::NotADirectory, "not a directory"));
    }
    let snapshot = files_snapshot()?;
    let mut entries = Vec::new();
    for line in snapshot.split(|b| *b == b'\n') {
        if line.is_empty() {
            continue;
        }
        let is_dir = line.ends_with(b"/");
        let body = if is_dir { &line[..line.len() - 1] } else { line };
        let (parent, name) = match body.iter().rposition(|b| *b == b'/') {
            Some(i) => (&body[..i], &body[i + 1..]),
            None => (&b""[..], body),
        };
        if parent == dir && !name.is_empty() {
            entries.push((OsString::from_vec(name.to_vec()), FileType { dir: is_dir }));
        }
    }
    Ok(ReadDir { dir: p.to_path_buf(), entries: entries.into_iter() })
}

pub fn unlink(p: &Path) -> io::Result<()> {
    syscall::remove(path_bytes(p)?).map_err(err)
}

pub fn rename(old: &Path, new: &Path) -> io::Result<()> {
    syscall::rename(path_bytes(old)?, path_bytes(new)?).map_err(err)
}

pub fn set_perm(_p: &Path, _perm: FilePermissions) -> io::Result<()> {
    unsupported()
}

pub fn set_perm_nofollow(_p: &Path, _perm: FilePermissions) -> io::Result<()> {
    unsupported()
}

pub fn set_times(_p: &Path, _times: FileTimes) -> io::Result<()> {
    unsupported()
}

pub fn set_times_nofollow(_p: &Path, _times: FileTimes) -> io::Result<()> {
    unsupported()
}

pub fn rmdir(p: &Path) -> io::Result<()> {
    let mut bytes = normalized(p).to_vec();
    if bytes.is_empty() {
        return Err(io::const_error!(io::ErrorKind::InvalidInput, "empty path"));
    }
    bytes.push(b'/');
    syscall::remove(&bytes).map_err(err)
}

pub fn remove_dir_all(path: &Path) -> io::Result<()> {
    for entry in readdir(path)? {
        let entry = entry?;
        if entry.kind.is_dir() {
            remove_dir_all(&entry.path())?;
        } else {
            unlink(&entry.path())?;
        }
    }
    rmdir(path)
}

pub fn exists(path: &Path) -> io::Result<bool> {
    match stat(path) {
        Ok(_) => Ok(true),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e),
    }
}

pub fn readlink(_p: &Path) -> io::Result<PathBuf> {
    unsupported()
}

pub fn symlink(_original: &Path, _link: &Path) -> io::Result<()> {
    unsupported()
}

pub fn link(_src: &Path, _dst: &Path) -> io::Result<()> {
    unsupported()
}

pub fn stat(p: &Path) -> io::Result<FileAttr> {
    let bytes = normalized(p);
    if bytes.is_empty() {
        // The root is the task's own directory.
        return Ok(FileAttr { size: 0, kind: FileType { dir: true } });
    }
    let mut record = [0u8; abi::STAT_LEN];
    match syscall::stat(bytes, &mut record) {
        Ok(()) => {
            let kind = FileType { dir: record[0] == abi::STAT_DIR };
            let size = u64::from(u32::from_le_bytes([record[4], record[5], record[6], record[7]]));
            Ok(FileAttr { size, kind })
        }
        // `Stat` knows galfs only; a bare ramdisk name answers through `Open`.
        Err(SysError::NotFound) | Err(SysError::Unsupported) | Err(SysError::BadValue)
            if is_bare_name(bytes) =>
        {
            let cap = syscall::open(bytes).map_err(err)?;
            let file = File { cap };
            Ok(FileAttr { size: file.size_via_seek()?, kind: FileType { dir: false } })
        }
        Err(e) => Err(err(e)),
    }
}

pub fn lstat(p: &Path) -> io::Result<FileAttr> {
    stat(p)
}

pub fn canonicalize(p: &Path) -> io::Result<PathBuf> {
    stat(p)?;
    let mut out = Vec::with_capacity(normalized(p).len() + 1);
    out.push(b'/');
    out.extend_from_slice(normalized(p));
    Ok(PathBuf::from(OsString::from_vec(out)))
}

pub fn copy(from: &Path, to: &Path) -> io::Result<u64> {
    let mut src_opts = OpenOptions::new();
    src_opts.read(true);
    let src = File::open(from, &src_opts)?;
    let mut dst_opts = OpenOptions::new();
    dst_opts.write(true);
    dst_opts.create(true);
    dst_opts.truncate(true);
    let dst = File::open(to, &dst_opts)?;
    let mut total = 0u64;
    let mut chunk = [0u8; 512];
    loop {
        let n = src.read(&mut chunk)?;
        if n == 0 {
            return Ok(total);
        }
        let mut off = 0usize;
        while off < n {
            let w = dst.write(&chunk[off..n])?;
            if w == 0 {
                return Err(io::Error::WRITE_ALL_EOF);
            }
            off += w;
        }
        total += n as u64;
    }
}
