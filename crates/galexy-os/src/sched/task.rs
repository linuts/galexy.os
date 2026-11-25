//! File, pipe, and channel syscalls for the current task.
//!
//! These run with interrupts off. They borrow the thread table and
//! return [`IoOp`] / [`RecvOp`] when the caller should park.

use super::iowait::*;
use super::spawn::*;
use super::thread::*;

use super::{channel, galfs, lane, loader, pipe};
use crate::arch::mm;
use core::sync::atomic::Ordering;
use galexy_abi::{Cap, CapRights, SysError, SyscallResult};
use x86_64::instructions::interrupts;
use x86_64::structures::paging::{Page, PhysFrame};
use x86_64::{PhysAddr, VirtAddr};

/// Opens a file for the current user task.
///
/// An exact ramdisk name (`cat`, `testing/banner.txt`) grants READ. Any
/// other path is a galfs file and grants READ and WRITE when a token
/// covers it. A directory is `Unsupported`. A path with no token is
/// `AccessDenied`.
pub(crate) fn task_open(name: &str) -> Result<Cap, SysError> {
    if crate::sched::ramdisk::is_testing_path(name) {
        let Some(bytes) = crate::sched::ramdisk::testing_elf(name) else {
            return Err(SysError::NotFound);
        };
        return install_open(FileBody::Archive(bytes), CapRights::READ);
    }
    if !name.contains('/') && !name.contains('@') {
        if let Some(bytes) = crate::sched::ramdisk::find(name) {
            return install_open(FileBody::Archive(bytes), CapRights::READ);
        }
    }
    let slot = current_slot();
    if slot == 0 {
        return Err(SysError::BadCap);
    }
    interrupts::without_interrupts(|| {
        let mut threads = THREADS.lock();
        let owner = running_proc_index(&threads, slot)?;
        let thread = &mut threads[owner];
        let Some(index) = thread.files.iter().position(|slot| slot.is_none()) else {
            return Err(SysError::NoResource);
        };
        let found = galfs::open_file(thread.fs_root, &thread.fs_tokens, name)?;
        let rights = CapRights::READ.union(CapRights::WRITE);
        thread.files[index] = Some(OpenFile {
            body: FileBody::Galfs(found),
            offset: 0,
            rights,
        });
        Ok(Cap::new(galexy_abi::FILE_CAP_BASE + index as u64, rights))
    })
}

pub(in crate::sched) fn install_open(body: FileBody, rights: CapRights) -> Result<Cap, SysError> {
    let slot = current_slot();
    if slot == 0 {
        return Err(SysError::BadCap);
    }
    interrupts::without_interrupts(|| {
        let mut threads = THREADS.lock();
        let owner = running_proc_index(&threads, slot)?;
        let thread = &mut threads[owner];
        let Some(index) = thread.files.iter().position(|slot| slot.is_none()) else {
            return Err(SysError::NoResource);
        };
        thread.files[index] = Some(OpenFile {
            body,
            offset: 0,
            rights,
        });
        Ok(Cap::new(galexy_abi::FILE_CAP_BASE + index as u64, rights))
    })
}

/// Copies the next bytes of an open file into `dst`. `Ok(0)` is end of file.
///
/// The effective right is the intersection of the kernel grant and the
/// handle snapshot, so a task cannot inflate READ onto a handle it stripped,
/// and cannot use a WRITE-only forgery of a file index.
/// Sync wrapper around [`task_read_ex`]; keep for call sites that cannot park.
#[allow(dead_code)]
pub(crate) fn task_read(cap: Cap, dst: &mut [u8]) -> Result<usize, SysError> {
    match task_read_ex(cap, dst)? {
        IoOp::Ready(n) => Ok(n),
        IoOp::ParkPipe { .. } | IoOp::ParkLane { .. } => Ok(0),
    }
}

/// Blocking-aware file read (Milestone 57). Archive/galfs stay non-blocking.
pub(crate) fn task_read_ex(cap: Cap, dst: &mut [u8]) -> Result<IoOp, SysError> {
    task_read_inner(cap, dst)
}

/// Result of a file/pipe I/O attempt that may need to park.
#[derive(Clone, Copy, Debug)]
pub(crate) enum IoOp {
    /// Completed with `n` bytes (EOF is `Ready(0)`).
    Ready(usize),
    /// Caller should park on this pipe end.
    ParkPipe { id: u8, read: bool },
    /// Caller should park on this lane end.
    ParkLane { id: u8, read: bool },
}

/// What a file read still has to do after `THREADS` drops.
enum ReadReady {
    /// Archive, pipe, lane, or an empty buffer. Already finished.
    Finished(IoOp),
    /// Galfs object. The disk read runs without `THREADS`.
    Galfs { obj: u32, start: usize },
}

/// What a file write still has to do after `THREADS` drops.
enum WriteReady {
    /// Pipe, lane, or an empty buffer. Already finished.
    Finished(IoOp),
    /// Galfs object. The disk write runs without `THREADS`.
    Galfs { obj: u32, offset: usize },
}

/// Moves the cursor when this open still names `obj`.
fn advance_galfs_cursor(slot: usize, index: usize, obj: u32, offset: usize) {
    interrupts::without_interrupts(|| {
        let mut threads = THREADS.lock();
        let Ok(owner) = running_proc_index(&threads, slot) else {
            return;
        };
        let Some(file) = threads[owner].files[index].as_mut() else {
            return;
        };
        if matches!(file.body, FileBody::Galfs(body) if body == obj) {
            file.offset = offset;
        }
    });
}

pub(in crate::sched) fn task_read_inner(cap: Cap, dst: &mut [u8]) -> Result<IoOp, SysError> {
    let index = file_slot(cap)?;
    let slot = current_slot();
    if slot == 0 {
        return Err(SysError::BadCap);
    }
    // The device wait turns interrupts on. It must not run while this
    // CPU holds `THREADS` or a galfs lock. This syscall reaches that
    // wait, so the thread table drops first. Every disk path does this;
    // it is not a special case for read.
    let ready = interrupts::without_interrupts(|| {
        let mut threads = THREADS.lock();
        let owner = running_proc_index(&threads, slot)?;
        let thread = &mut threads[owner];
        let file = thread.files[index].as_mut().ok_or(SysError::BadCap)?;
        let effective = file.rights.intersection(cap.rights());
        if !effective.contains(CapRights::READ) {
            return Err(SysError::AccessDenied);
        }
        if dst.is_empty() {
            return Ok(ReadReady::Finished(IoOp::Ready(0)));
        }
        let start = file.offset;
        match file.body {
            FileBody::Archive(bytes) => {
                let available = bytes.len().saturating_sub(start);
                let n = dst.len().min(available);
                dst[..n].copy_from_slice(&bytes[start..start + n]);
                file.offset = start + n;
                Ok(ReadReady::Finished(IoOp::Ready(n)))
            }
            FileBody::Galfs(obj) => Ok(ReadReady::Galfs { obj, start }),
            FileBody::Pipe { id, end } => {
                if end != pipe::PipeEnd::Read {
                    return Err(SysError::AccessDenied);
                }
                let op = match pipe::try_read(id, dst)? {
                    pipe::ReadResult::Ready(n) => IoOp::Ready(n),
                    pipe::ReadResult::Eof => IoOp::Ready(0),
                    pipe::ReadResult::WouldBlock => IoOp::ParkPipe { id, read: true },
                };
                Ok(ReadReady::Finished(op))
            }
            FileBody::Channel { .. } => Err(SysError::Unsupported),
            FileBody::Lane { id, end } => {
                let op = match lane::try_read(id, end, dst)? {
                    lane::ReadResult::Ready(n) => IoOp::Ready(n),
                    lane::ReadResult::Eof => IoOp::Ready(0),
                    lane::ReadResult::WouldBlock => IoOp::ParkLane { id, read: true },
                };
                Ok(ReadReady::Finished(op))
            }
        }
    })?;
    match ready {
        ReadReady::Finished(op) => Ok(op),
        ReadReady::Galfs { obj, start } => {
            let n = galfs::read_at(obj, start, dst).ok_or(SysError::BadCap)?;
            advance_galfs_cursor(slot, index, obj, start + n);
            Ok(IoOp::Ready(n))
        }
    }
}

/// Writes `src` into a galfs file at the cursor. An archive open is
/// `Unsupported`.
///
/// Milestone 70: positional. Bytes under the cursor are overwritten in
/// place, the remainder (cursor at or past the end) is appended, and the
/// cursor advances by the count — `seek(0)` then `read` sees the bytes
/// just written, and `write` after `read` continues where the read
/// stopped. A write that does not fit is short: the count is the bytes
/// copied, and `0` means the file is already at its maximum.
/// Sync wrapper around [`task_write_ex`]; keep for call sites that cannot park.
#[allow(dead_code)]
pub(crate) fn task_write(cap: Cap, src: &[u8]) -> Result<usize, SysError> {
    match task_write_ex(cap, src)? {
        IoOp::Ready(n) => Ok(n),
        IoOp::ParkPipe { .. } | IoOp::ParkLane { .. } => Ok(0),
    }
}

/// Blocking-aware file write (Milestone 57). Full pipes return [`IoOp::ParkPipe`].
pub(crate) fn task_write_ex(cap: Cap, src: &[u8]) -> Result<IoOp, SysError> {
    let index = file_slot(cap)?;
    let slot = current_slot();
    if slot == 0 {
        return Err(SysError::BadCap);
    }
    // Same rule as [`task_read_inner`]: the write reaches the device wait,
    // so `THREADS` is not held across it.
    let (ready, wake_pipe, wake_lane) = interrupts::without_interrupts(|| {
        let threads = THREADS.lock();
        let owner = running_proc_index(&threads, slot)?;
        let thread = &threads[owner];
        let must_change = thread.must_change;
        let file = thread.files[index].as_ref().ok_or(SysError::BadCap)?;
        let effective = file.rights.intersection(cap.rights());
        if !effective.contains(CapRights::WRITE) {
            return Err(SysError::AccessDenied);
        }
        if src.is_empty() {
            return Ok((WriteReady::Finished(IoOp::Ready(0)), None, None));
        }
        if must_change && matches!(file.body, FileBody::Galfs(_)) {
            return Err(SysError::AccessDenied);
        }
        match file.body {
            FileBody::Galfs(obj) => Ok((
                WriteReady::Galfs {
                    obj,
                    offset: file.offset,
                },
                None,
                None,
            )),
            FileBody::Pipe { id, end } => {
                if end != pipe::PipeEnd::Write {
                    return Err(SysError::AccessDenied);
                }
                match pipe::try_write(id, src)? {
                    pipe::WriteResult::Ready(n) => {
                        Ok((WriteReady::Finished(IoOp::Ready(n)), Some(id), None))
                    }
                    pipe::WriteResult::WouldBlock => Ok((
                        WriteReady::Finished(IoOp::ParkPipe { id, read: false }),
                        None,
                        None,
                    )),
                    pipe::WriteResult::Closed => Err(SysError::Unsupported),
                }
            }
            FileBody::Lane { id, end } => match lane::try_write(id, end, src)? {
                lane::WriteResult::Ready(n) => {
                    Ok((WriteReady::Finished(IoOp::Ready(n)), None, Some(id)))
                }
                lane::WriteResult::WouldBlock => Ok((
                    WriteReady::Finished(IoOp::ParkLane { id, read: false }),
                    None,
                    None,
                )),
                lane::WriteResult::Closed => Err(SysError::Unsupported),
            },
            FileBody::Archive(_) | FileBody::Channel { .. } => Err(SysError::Unsupported),
        }
    })?;
    let op = match ready {
        WriteReady::Finished(op) => op,
        WriteReady::Galfs { obj, offset } => {
            let n = galfs::write_at(obj, offset, src).ok_or(SysError::BadCap)?;
            advance_galfs_cursor(slot, index, obj, offset + n);
            if n > 0 {
                galfs::mark_dirty();
            }
            IoOp::Ready(n)
        }
    };
    if let Some(id) = wake_pipe {
        wake_pipe_waiters(id);
    }
    if let Some(id) = wake_lane {
        wake_lane_waiters(id);
    }
    Ok(op)
}

/// True when `name` is a ramdisk file: a bare label (`cat`) or a
/// `testing/` entry (`testing/banner.txt`).
///
/// Any other path that contains `/` is a galfs object even when the
/// last component matches (`/hello`). Spawn of the bare label still
/// loads the ramdisk program.
fn shadows_ramdisk(name: &str) -> bool {
    if crate::sched::ramdisk::is_testing_path(name) {
        return crate::sched::ramdisk::testing_elf(name).is_some();
    }
    if name.contains('/') || name.contains('@') {
        return false;
    }
    let Ok(parsed) = galfs::parse_path(name) else {
        return false;
    };
    parsed.owner.is_none()
        && parsed.n == 1
        && crate::sched::ramdisk::find(parsed.comps[0]).is_some()
}

/// Creates a galfs file or directory for the current user task.
///
/// A path ending in `/` is a directory and the returned cap is null.
/// A file cap carries READ and WRITE. `replace` empties an existing
/// file instead of failing. A bare ramdisk label cannot be replaced.
/// `/hello` can: it is a galfs file, and `hello` stays the ramdisk
/// program. A name that already exists is `Unsupported`. A missing
/// parent is `NotFound`. A full object table, or a full per-task file
/// table, is `NoResource`. A path with no create token is `AccessDenied`.
pub(crate) fn task_create(name: &str, replace: bool) -> Result<Cap, SysError> {
    galfs::parse_path(name)?;
    if shadows_ramdisk(name) {
        return Err(SysError::Unsupported);
    }
    let slot = current_slot();
    if slot == 0 {
        return Err(SysError::BadCap);
    }
    let cap = interrupts::without_interrupts(|| {
        let _vol = galfs::volume_guard();
        let mut threads = THREADS.lock();
        let owner = running_proc_index(&threads, slot)?;
        let thread = &mut threads[owner];
        deny_must_change(thread)?;
        let file_index = thread.files.iter().position(|slot| slot.is_none());
        let created = galfs::create(thread.fs_root, &thread.fs_tokens, name, replace)?;
        match created {
            None => Ok(Cap::null()),
            Some(obj) => {
                let Some(index) = file_index else {
                    return Err(SysError::NoResource);
                };
                let rights = CapRights::READ.union(CapRights::WRITE);
                thread.files[index] = Some(OpenFile {
                    body: FileBody::Galfs(obj),
                    offset: 0,
                    rights,
                });
                galfs::claim_policy_path(name, obj, &mut thread.fs_tokens);
                Ok(Cap::new(galexy_abi::FILE_CAP_BASE + index as u64, rights))
            }
        }
    })?;
    galfs::mark_dirty();
    Ok(cap)
}

/// Deletes a galfs file or an empty directory and frees its slot.
///
/// A bare ramdisk label is `Unsupported`. A path with `/` is galfs
/// (`/hello` can be removed; `hello` cannot). A directory that still
/// has a child is `Unsupported`. A missing path is `NotFound`. Any
/// task's open cap on that slot is dropped, so a later read or write
/// is `BadCap`. A path with no remove token is `AccessDenied`.
pub(crate) fn task_remove(name: &str) -> Result<(), SysError> {
    galfs::parse_path(name)?;
    if shadows_ramdisk(name) {
        return Err(SysError::Unsupported);
    }
    let slot = current_slot();
    if slot == 0 {
        return Err(SysError::BadCap);
    }
    interrupts::without_interrupts(|| {
        let _vol = galfs::volume_guard();
        let mut threads = THREADS.lock();
        let (fs_root, fs_tokens) = {
            let owner = running_proc_index(&threads, slot)?;
            let thread = &threads[owner];
            deny_must_change(thread)?;
            (thread.fs_root, thread.fs_tokens)
        };
        let removed = galfs::remove(fs_root, &fs_tokens, name)?;
        for thread in threads.iter_mut() {
            for open in &mut thread.files {
                let stale = matches!(
                    *open,
                    Some(file) if matches!(file.body, FileBody::Galfs(body) if body == removed)
                );
                if stale {
                    *open = None;
                }
            }
        }
        Ok(())
    })?;
    galfs::mark_dirty();
    Ok(())
}

/// Moves a galfs dirent from `old` to `new`.
pub(crate) fn task_rename(old: &str, new: &str) -> Result<(), SysError> {
    galfs::parse_path(old)?;
    galfs::parse_path(new)?;
    if shadows_ramdisk(old) || shadows_ramdisk(new) {
        return Err(SysError::Unsupported);
    }
    let slot = current_slot();
    if slot == 0 {
        return Err(SysError::BadCap);
    }
    interrupts::without_interrupts(|| {
        let _vol = galfs::volume_guard();
        let threads = THREADS.lock();
        let owner = running_proc_index(&threads, slot)?;
        let thread = &threads[owner];
        deny_must_change(thread)?;
        galfs::rename(thread.fs_root, &thread.fs_tokens, old, new)
    })?;
    galfs::mark_dirty();
    Ok(())
}

/// Sets the length of an open galfs file.
pub(crate) fn task_truncate(cap: Cap, new_len: u64) -> Result<(), SysError> {
    let index = file_slot(cap)?;
    let slot = current_slot();
    if slot == 0 {
        return Err(SysError::BadCap);
    }
    if new_len > galfs::FILE_BYTES {
        return Err(SysError::BadValue);
    }
    let obj = interrupts::without_interrupts(|| {
        let threads = THREADS.lock();
        let owner = running_proc_index(&threads, slot)?;
        let thread = &threads[owner];
        deny_must_change(thread)?;
        let file = thread.files[index].as_ref().ok_or(SysError::BadCap)?;
        let effective = file.rights.intersection(cap.rights());
        if !effective.contains(CapRights::WRITE) {
            return Err(SysError::AccessDenied);
        }
        match file.body {
            FileBody::Galfs(obj) => Ok(obj),
            FileBody::Archive(_)
            | FileBody::Pipe { .. }
            | FileBody::Channel { .. }
            | FileBody::Lane { .. } => Err(SysError::Unsupported),
        }
    })?;
    // Truncate can read a stable block back from disk. `THREADS` stays
    // dropped across that wait; see [`task_read_inner`].
    galfs::truncate(obj, new_len as usize)?;
    let new_len = new_len as usize;
    interrupts::without_interrupts(|| {
        let mut threads = THREADS.lock();
        let owner = running_proc_index(&threads, slot)?;
        let thread = &mut threads[owner];
        if let Some(file) = thread.files[index].as_mut() {
            if matches!(file.body, FileBody::Galfs(body) if body == obj) && file.offset > new_len {
                file.offset = new_len;
            }
        }
        Ok::<(), SysError>(())
    })?;
    galfs::mark_dirty();
    Ok(())
}

/// Writes galfs metadata for `name` into `out`.
pub(crate) fn task_stat(name: &str, out: &mut [u8]) -> Result<usize, SysError> {
    galfs::parse_path(name)?;
    if shadows_ramdisk(name) {
        return Err(SysError::Unsupported);
    }
    let slot = current_slot();
    if slot == 0 {
        return Err(SysError::BadCap);
    }
    interrupts::without_interrupts(|| {
        let threads = THREADS.lock();
        let owner = running_proc_index(&threads, slot)?;
        let thread = &threads[owner];
        galfs::stat(thread.fs_root, &thread.fs_tokens, name, out)
    })
}

/// Closes a pipe or channel end. Other bodies are just dropped.
pub(in crate::sched) fn release_file(file: OpenFile) {
    match file.body {
        FileBody::Pipe { id, end } => pipe::close_end(id, end),
        FileBody::Channel { id, end } => {
            let held = channel::close_end(id, end);
            for cap in held.into_iter().flatten() {
                release_file(cap);
            }
        }
        FileBody::Lane { id, end } => lane::close_end(id, end),
        FileBody::Archive(_) | FileBody::Galfs(_) => {}
    }
}

/// Grows the current task's heap by `pages`. See `docs/PROCESS.md`.
pub(crate) fn task_map(pages: u64) -> Result<u64, SysError> {
    if pages == 0 || pages > galexy_abi::USER_HEAP_PAGES {
        return Err(SysError::BadValue);
    }
    let slot = current_slot();
    if slot == 0 {
        return Err(SysError::BadCap);
    }
    let (already, image) = interrupts::without_interrupts(|| {
        let threads = THREADS.lock();
        let owner = running_proc_index(&threads, slot)?;
        let thread = &threads[owner];
        // Ramdisk ELFs live at `USER_IMAGE_BASE`. Hand-built tasks use
        // whichever P4 slot the loader picked. Reap walks only that slot.
        Ok((thread.heap_pages, (thread.user_p4 as u64) << 39))
    })?;
    if u64::from(already).saturating_add(pages) > galexy_abi::USER_HEAP_PAGES {
        return Err(SysError::NoResource);
    }
    let base = image + loader::USER_IMAGE_WINDOW + u64::from(already) * 4096;
    let mut mapped = 0u64;
    for i in 0..pages {
        let Some(frame) = mm::allocate_frame() else {
            break;
        };
        // SAFETY: the frame is exclusively ours until it is mapped.
        unsafe {
            core::ptr::write_bytes(
                mm::frame_virt(frame.start_address()).as_mut_ptr::<u8>(),
                0,
                4096,
            );
        }
        let page = Page::containing_address(VirtAddr::new(base + i * 4096));
        if mm::map_active_user_page(page, frame).is_err() {
            mm::deallocate_frame(frame);
            break;
        }
        mapped += 1;
    }
    if mapped == 0 {
        return Err(SysError::NoResource);
    }
    interrupts::without_interrupts(|| {
        let mut threads = THREADS.lock();
        if let Some(owner) = proc_index(&threads, slot) {
            threads[owner].heap_pages = threads[owner]
                .heap_pages
                .saturating_add(u32::try_from(mapped).unwrap_or(u32::MAX));
        }
    });
    if mapped < pages {
        return Err(SysError::NoResource);
    }
    Ok(base)
}

/// Creates a channel and installs both endpoints on the caller.
pub(crate) fn task_channel() -> Result<(Cap, Cap), SysError> {
    let slot = current_slot();
    if slot == 0 {
        return Err(SysError::BadCap);
    }
    interrupts::without_interrupts(|| {
        let mut threads = THREADS.lock();
        let owner = running_proc_index(&threads, slot)?;
        let thread = &mut threads[owner];
        let mut free = [usize::MAX; 2];
        let mut nfree = 0usize;
        for (i, slot) in thread.files.iter().enumerate() {
            if slot.is_none() {
                free[nfree] = i;
                nfree += 1;
                if nfree == 2 {
                    break;
                }
            }
        }
        if nfree < 2 {
            return Err(SysError::NoResource);
        }
        let id = channel::alloc()?;
        if thread.is_init {
            let _ = INIT_CTRL.compare_exchange(0xFF, id, Ordering::AcqRel, Ordering::Acquire);
        }
        if thread.name() == "net" {
            let _ = NET_CTRL.compare_exchange(0xFF, id, Ordering::AcqRel, Ordering::Acquire);
        }
        if thread.name() == "usb" {
            let _ = USB_CTRL.compare_exchange(0xFF, id, Ordering::AcqRel, Ordering::Acquire);
        }
        let rights = CapRights::READ.union(CapRights::WRITE);
        for (end, index) in [free[0], free[1]].into_iter().enumerate() {
            thread.files[index] = Some(OpenFile {
                body: FileBody::Channel { id, end: end as u8 },
                offset: 0,
                rights,
            });
        }
        Ok((
            Cap::new(galexy_abi::FILE_CAP_BASE + free[0] as u64, rights),
            Cap::new(galexy_abi::FILE_CAP_BASE + free[1] as u64, rights),
        ))
    })
}

pub(in crate::sched) fn channel_end(thread: &Thread, cap: Cap) -> Result<(u8, u8), SysError> {
    let index = file_slot(cap)?;
    let file = thread.files[index].as_ref().ok_or(SysError::BadCap)?;
    match file.body {
        FileBody::Channel { id, end } => Ok((id, end)),
        _ => Err(SysError::BadCap),
    }
}

/// Queues one message. Caps named by `cap0` / `cap1` (`0` = none) move
/// only when the queue accepts the message.
pub(crate) fn task_send(cap: Cap, bytes: &[u8], cap0: u64, cap1: u64) -> Result<usize, SysError> {
    if bytes.len() > galexy_abi::CHAN_MSG_MAX {
        return Err(SysError::BadValue);
    }
    let slot = current_slot();
    if slot == 0 {
        return Err(SysError::BadCap);
    }
    let id = interrupts::without_interrupts(|| -> Result<u8, SysError> {
        let mut threads = THREADS.lock();
        let owner = running_proc_index(&threads, slot)?;
        let (id, end) = {
            let thread = &mut threads[owner];
            channel_end(thread, cap)?
        };
        let ctrl = INIT_CTRL.load(Ordering::Acquire);
        let net = NET_CTRL.load(Ordering::Acquire);
        let usb = USB_CTRL.load(Ordering::Acquire);
        if net != 0xFF && id == net && end == 0 {
            deliver_net_send(&mut threads, owner, bytes, cap0, cap1)?;
            return Ok(id);
        }
        if usb != 0xFF && id == usb && end == 0 {
            if cap0 != 0 || cap1 != 0 {
                return Err(SysError::BadValue);
            }
            deliver_usb_reply(&mut threads, bytes)?;
            return Ok(id);
        }
        let thread = &mut threads[owner];
        if ctrl != 0xFF && id == ctrl && end == 0 {
            if cap0 != 0 || cap1 != 0 {
                return Err(SysError::BadValue);
            }
            deliver_init_reply(&mut threads, bytes)?;
            return Ok(id);
        }
        let endpoint = file_slot(cap)?;
        let file = thread.files[endpoint].as_ref().ok_or(SysError::BadCap)?;
        if !file
            .rights
            .intersection(cap.rights())
            .contains(CapRights::WRITE)
        {
            return Err(SysError::AccessDenied);
        }
        // Taken slots are restored on every failure, including a second
        // cap that names the same slot as the first.
        let mut moved: [Option<(usize, OpenFile)>; 2] = [None, None];
        let outcome = (|| -> Result<u8, SysError> {
            for (i, bits) in [cap0, cap1].into_iter().enumerate() {
                if bits == 0 {
                    continue;
                }
                let extra = Cap::from_bits(bits);
                let index = file_slot(extra)?;
                if index == endpoint || moved.iter().flatten().any(|(slot, _)| *slot == index) {
                    return Err(SysError::BadValue);
                }
                let Some(file) = thread.files[index].take() else {
                    return Err(SysError::BadCap);
                };
                if let FileBody::Channel { id: cid, .. } = file.body {
                    if cid == id {
                        thread.files[index] = Some(file);
                        return Err(SysError::BadValue);
                    }
                }
                moved[i] = Some((index, file));
            }
            // `OpenFile` is `Copy`. The channel stores these copies; the
            // table slots stay empty. Dropping `moved` does not close them.
            let caps = [
                moved[0].map(|(_, file)| file),
                moved[1].map(|(_, file)| file),
            ];
            channel::enqueue(id, end, bytes, caps)?;
            Ok(id)
        })();
        if outcome.is_err() {
            for item in moved.into_iter().flatten() {
                thread.files[item.0] = Some(item.1);
            }
        }
        outcome
    })?;
    wake_channel_waiters(id);
    Ok(bytes.len())
}

/// Writes the identity stamp. The caller cannot set these bytes.
fn fill_stamp(
    msg: &mut [u8],
    admin: u8,
    tty: u8,
    name: &[u8],
    entry: u32,
    parent: u32,
    debug_id: u64,
    session_gen: u64,
) {
    msg[0] = admin;
    msg[1] = tty;
    let n = name.len().min(galexy_abi::SEC_NAME_MAX);
    msg[2] = n as u8;
    msg[3] = 0;
    msg[galexy_abi::STAMP_ENTRY..galexy_abi::STAMP_ENTRY + 4].copy_from_slice(&entry.to_le_bytes());
    msg[galexy_abi::STAMP_PARENT..galexy_abi::STAMP_PARENT + 4]
        .copy_from_slice(&parent.to_le_bytes());
    msg[12..20].copy_from_slice(&debug_id.to_le_bytes());
    msg[20..28].copy_from_slice(&session_gen.to_le_bytes());
    msg[galexy_abi::STAMP_NAME..galexy_abi::STAMP_NAME + n].copy_from_slice(&name[..n]);
}

/// One init control RPC is in flight. `Ok(())` means the caller is parked.
pub(crate) fn task_init_rpc(bytes: &[u8], reply: u64, reply_len: u32) -> Result<(), SysError> {
    if bytes.len() > galexy_abi::INIT_RPC_MAX || reply_len as usize > galexy_abi::CHAN_MSG_MAX {
        return Err(SysError::BadValue);
    }
    let ctrl = INIT_CTRL.load(Ordering::Acquire);
    if ctrl == 0xFF {
        return Err(SysError::Unsupported);
    }
    let slot = current_slot();
    if slot == 0 {
        return Err(SysError::BadCap);
    }
    interrupts::without_interrupts(|| {
        let mut threads = THREADS.lock();
        let (admin, tty, debug_id, session_gen, entry, parent, name_raw, name_len) = {
            let owner = running_proc_index(&threads, slot)?;
            let thread = &threads[owner];
            if thread.fs_root == galfs::NO_OBJECT {
                return Err(SysError::AccessDenied);
            }
            let admin = u8::from(galfs::is_admin_root(thread.fs_root));
            let n = thread.name_len as usize;
            let mut raw = [0u8; NAME_CAP];
            raw[..n].copy_from_slice(&thread.name_bytes[..n]);
            (
                admin,
                thread.tty,
                thread.debug_id,
                thread.session_gen,
                thread.entry_object,
                thread.entry_parent,
                raw,
                n,
            )
        };
        if threads.iter().any(|t| {
            t.state.load(Ordering::Acquire) == STATE_WAITING
                && t.io_kind.load(Ordering::Acquire) == IO_INIT_RPC
        }) {
            return Err(SysError::NoResource);
        }
        let mut msg = [0u8; galexy_abi::CHAN_MSG_MAX];
        let n = galexy_abi::INIT_RPC_HDR + bytes.len();
        fill_stamp(
            &mut msg,
            admin,
            tty,
            &name_raw[..name_len],
            entry,
            parent,
            debug_id,
            session_gen,
        );
        msg[galexy_abi::INIT_RPC_HDR..n].copy_from_slice(bytes);
        channel::enqueue(ctrl, 1, &msg[..n], [None, None])?;
        park_io(&mut threads, slot, IO_INIT_RPC, ctrl, reply, reply_len, 0);
        wake_init_for_control(&mut threads);
        Ok(())
    })
}

/// Copies `bytes` into the parked init-RPC waiter and marks it runnable.
pub(in crate::sched) fn deliver_init_reply(
    threads: &mut [Thread],
    bytes: &[u8],
) -> Result<(), SysError> {
    let Some(index) = threads.iter().position(|t| {
        t.state.load(Ordering::Acquire) == STATE_WAITING
            && t.io_kind.load(Ordering::Acquire) == IO_INIT_RPC
    }) else {
        return Err(SysError::NoResource);
    };
    let addr = threads[index].io_addr.load(Ordering::Acquire);
    let len = threads[index].io_len.load(Ordering::Acquire) as usize;
    let cr3 = threads[index].cr3.load(Ordering::Acquire);
    let n = bytes.len().min(len);
    let copied = if n == 0 {
        true
    } else {
        let root = PhysFrame::from_start_address(PhysAddr::new(cr3)).expect("waiter cr3");
        // SAFETY: parked waiter's tree, not CR3-active. Phys-map copy.
        unsafe {
            crate::arch::mm::with_table(root, |mapper| copy_to_user_via(mapper, addr, &bytes[..n]))
        }
    };
    let result = if copied {
        SyscallResult::ok(n as u64)
    } else {
        SyscallResult::err(SysError::BadBuffer)
    };
    finish_chan_waiter(&threads[index], result);
    Ok(())
}

/// `net` answering a control `send`. One optional lane Cap moves onto
/// the waiter. The reply the waiter reads is 8 little-endian Cap bits
/// (0 when there is no lane) followed by `bytes`.
fn deliver_net_send(
    threads: &mut [Thread],
    sender: usize,
    bytes: &[u8],
    cap0: u64,
    cap1: u64,
) -> Result<(), SysError> {
    if cap0 != 0 && cap1 != 0 {
        return Err(SysError::BadValue);
    }
    let bits = if cap0 != 0 { cap0 } else { cap1 };
    let taken = if bits == 0 {
        None
    } else {
        let index = file_slot(Cap::from_bits(bits))?;
        let Some(file) = threads[sender].files[index].take() else {
            return Err(SysError::BadCap);
        };
        Some((index, file))
    };
    let moved = taken.as_ref().map(|(_, file)| *file);
    if let Err((err, restore)) = install_net_reply(threads, bytes, moved) {
        if restore {
            if let Some((index, file)) = taken {
                threads[sender].files[index] = Some(file);
            }
        }
        return Err(err);
    }
    Ok(())
}

/// `Err((_, true))` means the caller still owns `file` and must put it back.
fn install_net_reply(
    threads: &mut [Thread],
    bytes: &[u8],
    file: Option<OpenFile>,
) -> Result<(), (SysError, bool)> {
    let Some(index) = threads.iter().position(|t| {
        t.state.load(Ordering::Acquire) == STATE_WAITING
            && t.io_kind.load(Ordering::Acquire) == IO_NET_RPC
    }) else {
        return Err((SysError::NoResource, true));
    };
    let addr = threads[index].io_addr.load(Ordering::Acquire);
    let len = threads[index].io_len.load(Ordering::Acquire) as usize;
    let cr3 = threads[index].cr3.load(Ordering::Acquire);
    if len < 8 {
        return Err((SysError::BadValue, true));
    }
    let (cap_bits, slot, owner) = if let Some(file) = file.as_ref() {
        let owner = proc_index(threads, index + 1).ok_or((SysError::BadCap, true))?;
        let Some(slot) = threads[owner].files.iter().position(|s| s.is_none()) else {
            return Err((SysError::NoResource, true));
        };
        let rights = file.rights;
        (
            Cap::new(galexy_abi::FILE_CAP_BASE + slot as u64, rights).bits(),
            Some(slot),
            Some(owner),
        )
    } else {
        (0, None, None)
    };
    let mut raw = [0u8; galexy_abi::CHAN_MSG_MAX];
    raw[..8].copy_from_slice(&cap_bits.to_le_bytes());
    let text = bytes.len().min(len - 8).min(raw.len() - 8);
    raw[8..8 + text].copy_from_slice(&bytes[..text]);
    let n = 8 + text;
    let root = PhysFrame::from_start_address(PhysAddr::new(cr3)).expect("waiter cr3");
    // SAFETY: parked waiter's tree, not CR3-active. Phys-map copy.
    let copied = unsafe {
        crate::arch::mm::with_table(root, |mapper| copy_to_user_via(mapper, addr, &raw[..n]))
    };
    if !copied {
        return Err((SysError::BadBuffer, true));
    }
    if let (Some(file), Some(slot), Some(owner)) = (file, slot, owner) {
        threads[owner].files[slot] = Some(file);
    }
    finish_chan_waiter(&threads[index], SyscallResult::ok(n as u64));
    Ok(())
}

/// Mints a lane. Only the task named `net` may call this.
pub(crate) fn task_lane() -> Result<(Cap, Cap), SysError> {
    let slot = current_slot();
    if slot == 0 {
        return Err(SysError::BadCap);
    }
    interrupts::without_interrupts(|| {
        let mut threads = THREADS.lock();
        let owner = running_proc_index(&threads, slot)?;
        let thread = &mut threads[owner];
        if thread.name() != "net" {
            return Err(SysError::AccessDenied);
        }
        let mut free = [usize::MAX; 2];
        let mut nfree = 0usize;
        for (i, slot) in thread.files.iter().enumerate() {
            if slot.is_none() {
                free[nfree] = i;
                nfree += 1;
                if nfree == 2 {
                    break;
                }
            }
        }
        if nfree < 2 {
            return Err(SysError::NoResource);
        }
        let id = lane::alloc()?;
        let rights = CapRights::READ.union(CapRights::WRITE);
        for (end, index) in [free[0], free[1]].into_iter().enumerate() {
            thread.files[index] = Some(OpenFile {
                body: FileBody::Lane { id, end: end as u8 },
                offset: 0,
                rights,
            });
        }
        Ok((
            Cap::new(galexy_abi::FILE_CAP_BASE + free[0] as u64, rights),
            Cap::new(galexy_abi::FILE_CAP_BASE + free[1] as u64, rights),
        ))
    })
}

/// One net control RPC is in flight. `Ok(())` means the caller is parked.
pub(crate) fn task_net_rpc(bytes: &[u8], reply: u64, reply_len: u32) -> Result<(), SysError> {
    if bytes.len() > galexy_abi::INIT_RPC_MAX || reply_len as usize > galexy_abi::CHAN_MSG_MAX {
        return Err(SysError::BadValue);
    }
    if (reply_len as usize) < 8 {
        return Err(SysError::BadValue);
    }
    let ctrl = NET_CTRL.load(Ordering::Acquire);
    let slot = current_slot();
    if slot == 0 {
        return Err(SysError::BadCap);
    }
    interrupts::without_interrupts(|| {
        let mut threads = THREADS.lock();
        let mut name_raw = [0u8; NAME_CAP];
        let (admin, tty, debug_id, session_gen, entry, parent, name_len) = {
            let owner = running_proc_index(&threads, slot)?;
            let thread = &threads[owner];
            // A logged-out seat is denied before the service is consulted,
            // including when `net` has not registered yet.
            if thread.fs_root == galfs::NO_OBJECT {
                return Err(SysError::AccessDenied);
            }
            if ctrl == 0xFF {
                return Err(SysError::Unsupported);
            }
            let admin = u8::from(galfs::is_admin_root(thread.fs_root));
            let n = thread.name_len as usize;
            name_raw[..n].copy_from_slice(&thread.name_bytes[..n]);
            (
                admin,
                thread.tty,
                thread.debug_id,
                thread.session_gen,
                thread.entry_object,
                thread.entry_parent,
                n,
            )
        };
        if threads.iter().any(|t| {
            t.state.load(Ordering::Acquire) == STATE_WAITING
                && t.io_kind.load(Ordering::Acquire) == IO_NET_RPC
        }) {
            return Err(SysError::NoResource);
        }
        let mut msg = [0u8; galexy_abi::CHAN_MSG_MAX];
        let n = galexy_abi::INIT_RPC_HDR + bytes.len();
        fill_stamp(
            &mut msg,
            admin,
            tty,
            &name_raw[..name_len],
            entry,
            parent,
            debug_id,
            session_gen,
        );
        msg[galexy_abi::INIT_RPC_HDR..n].copy_from_slice(bytes);
        channel::enqueue(ctrl, 1, &msg[..n], [None, None])?;
        park_io(&mut threads, slot, IO_NET_RPC, ctrl, reply, reply_len, 0);
        wake_net_for_control(&mut threads);
        Ok(())
    })
}

/// One usb control RPC is in flight. `Ok(())` means the caller is parked.
///
/// The reply is plain text. It does not carry a lane capability.
pub(crate) fn task_usb_rpc(bytes: &[u8], reply: u64, reply_len: u32) -> Result<(), SysError> {
    if bytes.len() > galexy_abi::INIT_RPC_MAX || reply_len as usize > galexy_abi::CHAN_MSG_MAX {
        return Err(SysError::BadValue);
    }
    let ctrl = USB_CTRL.load(Ordering::Acquire);
    if ctrl == 0xFF {
        return Err(SysError::Unsupported);
    }
    let slot = current_slot();
    if slot == 0 {
        return Err(SysError::BadCap);
    }
    interrupts::without_interrupts(|| {
        let mut threads = THREADS.lock();
        let mut name_raw = [0u8; NAME_CAP];
        let (admin, tty, debug_id, session_gen, entry, parent, name_len) = {
            let owner = running_proc_index(&threads, slot)?;
            let thread = &threads[owner];
            if thread.fs_root == galfs::NO_OBJECT {
                return Err(SysError::AccessDenied);
            }
            let admin = u8::from(galfs::is_admin_root(thread.fs_root));
            let n = thread.name_len as usize;
            name_raw[..n].copy_from_slice(&thread.name_bytes[..n]);
            (
                admin,
                thread.tty,
                thread.debug_id,
                thread.session_gen,
                thread.entry_object,
                thread.entry_parent,
                n,
            )
        };
        if threads.iter().any(|t| {
            t.state.load(Ordering::Acquire) == STATE_WAITING
                && t.io_kind.load(Ordering::Acquire) == IO_USB_RPC
        }) {
            return Err(SysError::NoResource);
        }
        let mut msg = [0u8; galexy_abi::CHAN_MSG_MAX];
        let n = galexy_abi::INIT_RPC_HDR + bytes.len();
        fill_stamp(
            &mut msg,
            admin,
            tty,
            &name_raw[..name_len],
            entry,
            parent,
            debug_id,
            session_gen,
        );
        msg[galexy_abi::INIT_RPC_HDR..n].copy_from_slice(bytes);
        channel::enqueue(ctrl, 1, &msg[..n], [None, None])?;
        park_io(&mut threads, slot, IO_USB_RPC, ctrl, reply, reply_len, 0);
        wake_usb_for_control(&mut threads);
        Ok(())
    })
}

/// Copies `bytes` into the parked usb-RPC waiter and marks it runnable.
pub(in crate::sched) fn deliver_usb_reply(
    threads: &mut [Thread],
    bytes: &[u8],
) -> Result<(), SysError> {
    let Some(index) = threads.iter().position(|t| {
        t.state.load(Ordering::Acquire) == STATE_WAITING
            && t.io_kind.load(Ordering::Acquire) == IO_USB_RPC
    }) else {
        return Err(SysError::NoResource);
    };
    let addr = threads[index].io_addr.load(Ordering::Acquire);
    let len = threads[index].io_len.load(Ordering::Acquire) as usize;
    let cr3 = threads[index].cr3.load(Ordering::Acquire);
    let n = bytes.len().min(len);
    let copied = if n == 0 {
        true
    } else {
        let root = PhysFrame::from_start_address(PhysAddr::new(cr3)).expect("waiter cr3");
        // SAFETY: parked waiter's tree, not CR3-active. Phys-map copy.
        unsafe {
            crate::arch::mm::with_table(root, |mapper| copy_to_user_via(mapper, addr, &bytes[..n]))
        }
    };
    let result = if copied {
        SyscallResult::ok(n as u64)
    } else {
        SyscallResult::err(SysError::BadBuffer)
    };
    finish_chan_waiter(&threads[index], result);
    Ok(())
}

/// Lets init observe a queued control message without waiting out a backoff
/// sleep or a live child's Cap-wait. A child that has already exited keeps
/// the exit wake. Other tasks are left parked.
pub(in crate::sched) fn wake_init_for_control(threads: &mut [Thread]) {
    let Some(index) = threads.iter().position(|t| t.is_init) else {
        return;
    };
    if threads[index].state.load(Ordering::Acquire) != STATE_WAITING {
        return;
    }
    if threads[index].sleep_deadline.load(Ordering::Acquire) != 0 {
        clear_wait_fields(&threads[index]);
        stamp_waiter_frame(&threads[index], SyscallResult::err(SysError::Interrupted));
        set_running(&threads[index]);
        return;
    }
    if !threads[index].wait_for_exit.load(Ordering::Acquire) {
        return;
    }
    let child = threads[index].wait_child_slot.load(Ordering::Acquire);
    if child == 0 {
        return;
    }
    let ci = child as usize;
    if ci == 0 || ci > threads.len() {
        return;
    }
    let state = threads[ci - 1].state.load(Ordering::Acquire);
    if state == STATE_EXITED || state == STATE_FREED {
        return;
    }
    let pi = threads[index].wait_proc_index.load(Ordering::Acquire);
    if pi == WAIT_PROC_NONE {
        return;
    }
    let pi = pi as usize;
    if pi >= MAX_PROC_CAPS {
        return;
    }
    let Some(handle) = threads[index].procs[pi] else {
        return;
    };
    if handle.child_slot != child {
        return;
    }
    clear_wait_fields(&threads[index]);
    stamp_waiter_frame(&threads[index], SyscallResult::err(SysError::Interrupted));
    set_running(&threads[index]);
}

/// Outcome of [`task_recv`].
///
/// The payload stays inline. Boxing it would allocate on the syscall path.
#[allow(clippy::large_enum_variant)]
pub(crate) enum RecvOp {
    /// Message copied into `bytes` (`n` of them). `caps` are installed bits.
    Ready {
        /// Payload length.
        n: usize,
        /// Payload.
        bytes: [u8; galexy_abi::CHAN_MSG_MAX],
        /// New Cap bits, or 0.
        caps: [u64; 2],
    },
    /// Peer closed, nothing queued.
    Eof,
    /// Caller is parked.
    Park,
}

/// Receives one message or parks. `caps_out == 0` refuses a message that
/// carries Caps (`BadBuffer`, message stays).
pub(crate) fn task_recv(
    cap: Cap,
    addr: u64,
    len: usize,
    caps_out: u64,
    poll: bool,
) -> Result<RecvOp, SysError> {
    let slot = current_slot();
    if slot == 0 {
        return Err(SysError::BadCap);
    }
    interrupts::without_interrupts(|| {
        let mut threads = THREADS.lock();
        let owner = running_proc_index(&threads, slot)?;
        let (id, end) = {
            let thread = &threads[owner];
            let (id, end) = channel_end(thread, cap)?;
            let file = thread.files[file_slot(cap)?]
                .as_ref()
                .ok_or(SysError::BadCap)?;
            if !file
                .rights
                .intersection(cap.rights())
                .contains(CapRights::READ)
            {
                return Err(SysError::AccessDenied);
            }
            (id, end)
        };
        channel::with_mut(id, |ch| {
            match channel::pull(ch, end)? {
                Ok(delivery) => {
                    let need = delivery.caps.iter().filter(|c| c.is_some()).count();
                    if need > 0 && caps_out == 0 {
                        channel::unpull(ch, delivery);
                        return Err(SysError::BadBuffer);
                    }
                    let free = threads[owner].files.iter().filter(|s| s.is_none()).count();
                    if need > free {
                        channel::unpull(ch, delivery);
                        return Err(SysError::NoResource);
                    }
                    let n = delivery.len.min(len);
                    let mut bytes = [0u8; galexy_abi::CHAN_MSG_MAX];
                    bytes[..n].copy_from_slice(&delivery.data[..n]);
                    let mut caps = [0u64; 2];
                    for (i, file) in delivery.caps.into_iter().enumerate() {
                        let Some(file) = file else { continue };
                        // `need <= free` was checked above, still holding
                        // `THREADS`. `release_file` would re-lock `CHANS`.
                        let Some(index) = threads[owner].files.iter().position(|s| s.is_none())
                        else {
                            continue;
                        };
                        let rights = file.rights;
                        threads[owner].files[index] = Some(file);
                        caps[i] = Cap::new(galexy_abi::FILE_CAP_BASE + index as u64, rights).bits();
                    }
                    Ok(RecvOp::Ready { n, bytes, caps })
                }
                Err(channel::Empty::Eof) => Ok(RecvOp::Eof),
                Err(channel::Empty::Wait) => {
                    if poll {
                        return Err(SysError::NoResource);
                    }
                    park_io(
                        &mut threads,
                        slot,
                        IO_CHAN_RECV,
                        id,
                        addr,
                        len as u32,
                        cap.bits(),
                    );
                    threads[slot - 1]
                        .io_extra
                        .store(caps_out, Ordering::Relaxed);
                    Ok(RecvOp::Park)
                }
            }
        })?
    })
}

/// Drops one file capability belonging to the current task.
///
/// Close is possession of the slot, not a READ: the index names the open
/// in this task's table, and no other task has that table.
pub(crate) fn task_close(cap: Cap) -> Result<(), SysError> {
    let slot = current_slot();
    if slot == 0 {
        return Err(SysError::BadCap);
    }
    if let Ok(pi) = proc_slot(cap) {
        return interrupts::without_interrupts(|| {
            let mut threads = THREADS.lock();
            let owner = running_proc_index(&threads, slot)?;
            let thread = &mut threads[owner];
            let Some(_) = thread.procs[pi].take() else {
                return Err(SysError::BadCap);
            };
            Ok(())
        });
    }
    let index = file_slot(cap)?;
    let (wake_pipe, wake_chan, wake_lane) = interrupts::without_interrupts(|| {
        let mut threads = THREADS.lock();
        let owner = running_proc_index(&threads, slot)?;
        let thread = &mut threads[owner];
        let Some(file) = thread.files[index].take() else {
            return Err(SysError::BadCap);
        };
        Ok(match file.body {
            FileBody::Pipe { id, end } => {
                pipe::close_end(id, end);
                (Some(id), None, None)
            }
            FileBody::Channel { id, end } => {
                let held = channel::close_end(id, end);
                for cap in held.into_iter().flatten() {
                    release_file(cap);
                }
                (None, Some(id), None)
            }
            FileBody::Lane { id, end } => {
                lane::close_end(id, end);
                (None, None, Some(id))
            }
            _ => (None, None, None),
        })
    })?;
    if let Some(id) = wake_pipe {
        wake_pipe_waiters(id);
    }
    if let Some(id) = wake_chan {
        wake_channel_waiters(id);
    }
    if let Some(id) = wake_lane {
        wake_lane_waiters(id);
    }
    Ok(())
}

fn shell_parent(name: &str) -> bool {
    name == "shell"
        || name
            .strip_prefix("shell")
            .is_some_and(|rest| !rest.is_empty() && rest.bytes().all(|b| b.is_ascii_digit()))
}

fn seat_can_answer(threads: &[Thread], slot: usize) -> bool {
    let Some(thread) = threads.get(slot - 1) else {
        return false;
    };
    let parent = thread.parent_slot as usize;
    if parent == 0 || parent > threads.len() {
        return false;
    }
    if !shell_parent(threads[parent - 1].name()) {
        return false;
    }
    let tty = thread.tty as usize;
    tty < FG_SLOTS.len() && FG_SLOTS[tty].load(Ordering::Acquire) == slot as u8
}

#[allow(clippy::too_many_arguments)]
fn park_card_ask(
    threads: &mut [Thread],
    slot: usize,
    kind: u8,
    rights: u8,
    path: &str,
    payload: &[u8],
    reply: u64,
    reply_len: u32,
) -> bool {
    if path.is_empty() || path.len() > 96 || payload.len() > 96 || !seat_can_answer(threads, slot) {
        return false;
    }
    let done_bit = if kind == ASK_NETWORK {
        ASK_DONE_NETWORK
    } else {
        ASK_DONE_STORAGE
    };
    let done = threads[slot - 1].ask_done.load(Ordering::Acquire);
    // A second ask for the same path in this run does not print.
    // A different card is a different right, so it still asks.
    if done & done_bit != 0 {
        let n = threads[slot - 1].ask_len as usize;
        if n == path.len() && &threads[slot - 1].ask_buf[..n] == path.as_bytes() {
            return false;
        }
    }
    let thread = &mut threads[slot - 1];
    thread.ask_buf[..path.len()].copy_from_slice(path.as_bytes());
    thread.ask_len = path.len() as u8;
    thread.ask_arg[..payload.len()].copy_from_slice(payload);
    thread.ask_arg_len = payload.len() as u8;
    thread.ask_rights = rights;
    thread.ask_done.store(done | done_bit, Ordering::Release);
    park_io(threads, slot, IO_ASK, kind, reply, reply_len, 0);
    true
}

/// Parks a foreground program that opened a file it has no card for.
///
/// The shell reads yes or no. A second open of storage in this run
/// does not park. Logged-out and a missing path do not ask.
pub(crate) fn begin_storage_ask(name: &str) -> bool {
    if name.len() > 96 || name.is_empty() {
        return false;
    }
    let slot = current_slot();
    if slot == 0 {
        return false;
    }
    interrupts::without_interrupts(|| {
        let mut threads = THREADS.lock();
        let Ok(owner) = running_proc_index(&threads, slot) else {
            return false;
        };
        let root = threads[owner].fs_root;
        let tokens = threads[owner].fs_tokens;
        if root == galfs::NO_OBJECT {
            return false;
        }
        if !matches!(
            galfs::resolve_and_check(root, &tokens, name, galfs::RIGHT_READ),
            Err(SysError::AccessDenied)
        ) {
            return false;
        }
        park_card_ask(
            &mut threads,
            slot,
            ASK_STORAGE,
            galfs::RIGHT_READ,
            name,
            b"",
            0,
            0,
        )
    })
}

/// Finishes a parked card ask. `yes` retries the open or the net call.
/// Anything else returns `AccessDenied` and leaves the program running.
pub(in crate::sched) fn complete_ask(threads: &mut [Thread], index: usize, yes: bool) {
    let Some(thread) = threads.get(index) else {
        return;
    };
    if thread.io_kind.load(Ordering::Acquire) != IO_ASK {
        return;
    }
    if !yes {
        finish_ask_denied(threads, index);
        return;
    }
    let kind = threads[index].io_pipe.load(Ordering::Acquire);
    if kind == ASK_STORAGE {
        finish_storage_ask(threads, index);
    } else if kind == ASK_NETWORK {
        finish_network_ask(threads, index);
    } else {
        finish_ask_denied(threads, index);
    }
}

fn finish_ask_denied(threads: &mut [Thread], index: usize) {
    threads[index].io_kind.store(IO_NONE, Ordering::Release);
    stamp_waiter_frame(&threads[index], SyscallResult::err(SysError::AccessDenied));
    set_running(&threads[index]);
}

fn finish_storage_ask(threads: &mut [Thread], index: usize) {
    let len = threads[index].ask_len as usize;
    let mut raw = [0u8; 96];
    raw[..len].copy_from_slice(&threads[index].ask_buf[..len]);
    let path = core::str::from_utf8(&raw[..len]).unwrap_or("");
    let owner = proc_index(threads, index + 1).unwrap_or(index);
    let root = threads[owner].fs_root;
    let tokens = threads[owner].fs_tokens;
    let opened = galfs::open_file(root, &tokens, path);
    let result = match opened {
        Ok(found) => {
            let Some(slot_i) = threads[owner].files.iter().position(|s| s.is_none()) else {
                finish_ask_err(threads, index, SysError::NoResource);
                return;
            };
            let rights = CapRights::READ.union(CapRights::WRITE);
            threads[owner].files[slot_i] = Some(OpenFile {
                body: FileBody::Galfs(found),
                offset: 0,
                rights,
            });
            SyscallResult::ok(Cap::new(galexy_abi::FILE_CAP_BASE + slot_i as u64, rights).bits())
        }
        Err(err) => SyscallResult::err(err),
    };
    threads[index].io_kind.store(IO_NONE, Ordering::Release);
    stamp_waiter_frame(&threads[index], result);
    set_running(&threads[index]);
}

fn finish_ask_err(threads: &mut [Thread], index: usize, err: SysError) {
    threads[index].io_kind.store(IO_NONE, Ordering::Release);
    stamp_waiter_frame(&threads[index], SyscallResult::err(err));
    set_running(&threads[index]);
}

fn finish_network_ask(threads: &mut [Thread], index: usize) {
    // Network asks are gone. A missing sec rule is a refusal.
    finish_ask_denied(threads, index);
}
