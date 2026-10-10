//! File, pipe, and channel syscalls for the current task.
//!
//! These run with interrupts off. They borrow the thread table and
//! return [`IoOp`] / [`RecvOp`] when the caller should park.

use super::iowait::*;
use super::spawn::*;
use super::thread::*;

use super::{channel, galfs, loader, pipe};
use crate::arch::mm;
use core::sync::atomic::Ordering;
use galexy_abi::{Cap, CapRights, SysError, SyscallResult};
use x86_64::instructions::interrupts;
use x86_64::structures::paging::{Page, PhysFrame};
use x86_64::{PhysAddr, VirtAddr};

/// Opens a file for the current user task.
///
/// An exact ramdisk name (`banner.txt`, `hello`) grants READ. Any other
/// path is a galfs file and grants READ and WRITE when a token covers it.
/// A directory is `Unsupported`. A path with no token is `AccessDenied`.
pub(crate) fn task_open(name: &str) -> Result<Cap, SysError> {
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
        let thread = threads.get_mut(slot - 1).ok_or(SysError::BadCap)?;
        if !thread.is_user || thread.state.load(Ordering::Acquire) != STATE_RUNNING {
            return Err(SysError::BadCap);
        }
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
        let thread = threads.get_mut(slot - 1).ok_or(SysError::BadCap)?;
        if !thread.is_user || thread.state.load(Ordering::Acquire) != STATE_RUNNING {
            return Err(SysError::BadCap);
        }
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
        IoOp::ParkPipe { .. } => Ok(0),
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
}

pub(in crate::sched) fn task_read_inner(cap: Cap, dst: &mut [u8]) -> Result<IoOp, SysError> {
    let index = file_slot(cap)?;
    let slot = current_slot();
    if slot == 0 {
        return Err(SysError::BadCap);
    }
    interrupts::without_interrupts(|| {
        let mut threads = THREADS.lock();
        let thread = threads.get_mut(slot - 1).ok_or(SysError::BadCap)?;
        let file = thread.files[index].as_mut().ok_or(SysError::BadCap)?;
        let effective = file.rights.intersection(cap.rights());
        if !effective.contains(CapRights::READ) {
            return Err(SysError::AccessDenied);
        }
        if dst.is_empty() {
            return Ok(IoOp::Ready(0));
        }
        let start = file.offset;
        let op = match file.body {
            FileBody::Archive(bytes) => {
                let available = bytes.len().saturating_sub(start);
                let n = dst.len().min(available);
                dst[..n].copy_from_slice(&bytes[start..start + n]);
                IoOp::Ready(n)
            }
            FileBody::Galfs(obj) => {
                IoOp::Ready(galfs::read_at(obj, start, dst).ok_or(SysError::BadCap)?)
            }
            FileBody::Pipe { id, end } => {
                if end != pipe::PipeEnd::Read {
                    return Err(SysError::AccessDenied);
                }
                match pipe::try_read(id, dst)? {
                    pipe::ReadResult::Ready(n) => IoOp::Ready(n),
                    pipe::ReadResult::Eof => IoOp::Ready(0),
                    pipe::ReadResult::WouldBlock => IoOp::ParkPipe { id, read: true },
                }
            }
            FileBody::Channel { .. } => return Err(SysError::Unsupported),
        };
        if let IoOp::Ready(n) = op {
            if !matches!(file.body, FileBody::Pipe { .. }) {
                file.offset = start + n;
            }
        }
        Ok(op)
    })
}

/// Appends `src` to a galfs file. An archive open is `Unsupported`.
///
/// The read cursor stays put, so a later `read` still starts at the
/// beginning. A write that does not fit is short: the count is the bytes
/// copied, and `0` means the buffer is already full.
/// Sync wrapper around [`task_write_ex`]; keep for call sites that cannot park.
#[allow(dead_code)]
pub(crate) fn task_write(cap: Cap, src: &[u8]) -> Result<usize, SysError> {
    match task_write_ex(cap, src)? {
        IoOp::Ready(n) => Ok(n),
        IoOp::ParkPipe { .. } => Ok(0),
    }
}

/// Blocking-aware file write (Milestone 57). Full pipes return [`IoOp::ParkPipe`].
pub(crate) fn task_write_ex(cap: Cap, src: &[u8]) -> Result<IoOp, SysError> {
    let index = file_slot(cap)?;
    let slot = current_slot();
    if slot == 0 {
        return Err(SysError::BadCap);
    }
    let (op, galfs_wrote, wake_pipe) = interrupts::without_interrupts(|| {
        let mut threads = THREADS.lock();
        let thread = threads.get_mut(slot - 1).ok_or(SysError::BadCap)?;
        let must_change = thread.must_change;
        let file = thread.files[index].as_mut().ok_or(SysError::BadCap)?;
        let effective = file.rights.intersection(cap.rights());
        if !effective.contains(CapRights::WRITE) {
            return Err(SysError::AccessDenied);
        }
        if src.is_empty() {
            return Ok((IoOp::Ready(0), false, None));
        }
        if must_change && matches!(file.body, FileBody::Galfs(_)) {
            return Err(SysError::AccessDenied);
        }
        match file.body {
            FileBody::Galfs(obj) => {
                let n = galfs::append(obj, src).ok_or(SysError::BadCap)?;
                Ok((IoOp::Ready(n), n > 0, None))
            }
            FileBody::Pipe { id, end } => {
                if end != pipe::PipeEnd::Write {
                    return Err(SysError::AccessDenied);
                }
                match pipe::try_write(id, src)? {
                    pipe::WriteResult::Ready(n) => Ok((IoOp::Ready(n), false, Some(id))),
                    pipe::WriteResult::WouldBlock => {
                        Ok((IoOp::ParkPipe { id, read: false }, false, None))
                    }
                    pipe::WriteResult::Closed => Err(SysError::Unsupported),
                }
            }
            FileBody::Archive(_) | FileBody::Channel { .. } => Err(SysError::Unsupported),
        }
    })?;
    if galfs_wrote {
        galfs::mark_dirty();
    }
    if let Some(id) = wake_pipe {
        wake_pipe_waiters(id);
    }
    Ok(op)
}

/// Creates a galfs file or directory for the current user task.
///
/// A path ending in `/` is a directory and the returned cap is null.
/// A file cap carries READ and WRITE. `replace` empties an existing
/// file instead of failing. A ramdisk name at `/` cannot be
/// replaced. A name that already exists is `Unsupported`. A missing
/// parent is `NotFound`. A full object table, or a full per-task file
/// table, is `NoResource`. A path with no create token is `AccessDenied`.
pub(crate) fn task_create(name: &str, replace: bool) -> Result<Cap, SysError> {
    let parsed = galfs::parse_path(name)?;
    if parsed.owner.is_none()
        && parsed.n == 1
        && crate::sched::ramdisk::find(parsed.comps[0]).is_some()
    {
        return Err(SysError::Unsupported);
    }
    let slot = current_slot();
    if slot == 0 {
        return Err(SysError::BadCap);
    }
    let cap = interrupts::without_interrupts(|| {
        let mut threads = THREADS.lock();
        let thread = threads.get_mut(slot - 1).ok_or(SysError::BadCap)?;
        if !thread.is_user || thread.state.load(Ordering::Acquire) != STATE_RUNNING {
            return Err(SysError::BadCap);
        }
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
                Ok(Cap::new(galexy_abi::FILE_CAP_BASE + index as u64, rights))
            }
        }
    })?;
    galfs::mark_dirty();
    Ok(cap)
}

/// Deletes a galfs file or an empty directory and frees its slot.
///
/// A ramdisk name at `/` is `Unsupported`. A directory that still has a
/// child is `Unsupported`. A missing path is `NotFound`. Any task's open
/// cap on that slot is dropped, so a later read or write is `BadCap`.
/// A path with no remove token is `AccessDenied`.
pub(crate) fn task_remove(name: &str) -> Result<(), SysError> {
    let parsed = galfs::parse_path(name)?;
    if parsed.owner.is_none()
        && parsed.n == 1
        && crate::sched::ramdisk::find(parsed.comps[0]).is_some()
    {
        return Err(SysError::Unsupported);
    }
    let slot = current_slot();
    if slot == 0 {
        return Err(SysError::BadCap);
    }
    interrupts::without_interrupts(|| {
        let mut threads = THREADS.lock();
        {
            let thread = threads.get(slot - 1).ok_or(SysError::BadCap)?;
            if !thread.is_user || thread.state.load(Ordering::Acquire) != STATE_RUNNING {
                return Err(SysError::BadCap);
            }
            deny_must_change(thread)?;
        }
        let (fs_root, fs_tokens) = {
            let thread = threads.get(slot - 1).ok_or(SysError::BadCap)?;
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
    let old_parsed = galfs::parse_path(old)?;
    let new_parsed = galfs::parse_path(new)?;
    if old_parsed.owner.is_none()
        && old_parsed.n == 1
        && crate::sched::ramdisk::find(old_parsed.comps[0]).is_some()
    {
        return Err(SysError::Unsupported);
    }
    if new_parsed.owner.is_none()
        && new_parsed.n == 1
        && crate::sched::ramdisk::find(new_parsed.comps[0]).is_some()
    {
        return Err(SysError::Unsupported);
    }
    let slot = current_slot();
    if slot == 0 {
        return Err(SysError::BadCap);
    }
    interrupts::without_interrupts(|| {
        let threads = THREADS.lock();
        let thread = threads.get(slot - 1).ok_or(SysError::BadCap)?;
        if !thread.is_user || thread.state.load(Ordering::Acquire) != STATE_RUNNING {
            return Err(SysError::BadCap);
        }
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
    if new_len > galfs::FILE_BYTES as u64 {
        return Err(SysError::BadValue);
    }
    interrupts::without_interrupts(|| {
        let mut threads = THREADS.lock();
        let thread = threads.get_mut(slot - 1).ok_or(SysError::BadCap)?;
        deny_must_change(thread)?;
        let file = thread.files[index].as_mut().ok_or(SysError::BadCap)?;
        let effective = file.rights.intersection(cap.rights());
        if !effective.contains(CapRights::WRITE) {
            return Err(SysError::AccessDenied);
        }
        match file.body {
            FileBody::Galfs(obj) => {
                galfs::truncate(obj, new_len as usize)?;
                if file.offset > new_len as usize {
                    file.offset = new_len as usize;
                }
                Ok(())
            }
            FileBody::Archive(_) | FileBody::Pipe { .. } | FileBody::Channel { .. } => {
                Err(SysError::Unsupported)
            }
        }
    })?;
    galfs::mark_dirty();
    Ok(())
}

/// Writes galfs metadata for `name` into `out`.
pub(crate) fn task_stat(name: &str, out: &mut [u8]) -> Result<usize, SysError> {
    let parsed = galfs::parse_path(name)?;
    if parsed.owner.is_none()
        && parsed.n == 1
        && crate::sched::ramdisk::find(parsed.comps[0]).is_some()
    {
        return Err(SysError::Unsupported);
    }
    let slot = current_slot();
    if slot == 0 {
        return Err(SysError::BadCap);
    }
    interrupts::without_interrupts(|| {
        let threads = THREADS.lock();
        let thread = threads.get(slot - 1).ok_or(SysError::BadCap)?;
        if !thread.is_user || thread.state.load(Ordering::Acquire) != STATE_RUNNING {
            return Err(SysError::BadCap);
        }
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
        let thread = threads.get(slot - 1).ok_or(SysError::BadCap)?;
        if !thread.is_user || thread.state.load(Ordering::Acquire) != STATE_RUNNING {
            return Err(SysError::BadCap);
        }
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
        if let Some(thread) = THREADS.lock().get_mut(slot - 1) {
            thread.heap_pages = thread.heap_pages.saturating_add(mapped as u16);
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
        let thread = threads.get_mut(slot - 1).ok_or(SysError::BadCap)?;
        if !thread.is_user || thread.state.load(Ordering::Acquire) != STATE_RUNNING {
            return Err(SysError::BadCap);
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
        let id = channel::alloc()?;
        if thread.is_init {
            let _ = INIT_CTRL.compare_exchange(0xFF, id, Ordering::AcqRel, Ordering::Acquire);
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
        let thread = threads.get_mut(slot - 1).ok_or(SysError::BadCap)?;
        if !thread.is_user || thread.state.load(Ordering::Acquire) != STATE_RUNNING {
            return Err(SysError::BadCap);
        }
        let (id, end) = channel_end(thread, cap)?;
        let ctrl = INIT_CTRL.load(Ordering::Acquire);
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
        let (admin, tty, debug_id, session_gen) = {
            let thread = threads.get(slot - 1).ok_or(SysError::BadCap)?;
            if !thread.is_user || thread.state.load(Ordering::Acquire) != STATE_RUNNING {
                return Err(SysError::BadCap);
            }
            if thread.fs_root == galfs::NO_OBJECT {
                return Err(SysError::AccessDenied);
            }
            let admin = u8::from(galfs::is_admin_root(thread.fs_root));
            (admin, thread.tty, thread.debug_id, thread.session_gen)
        };
        if threads.iter().any(|t| {
            t.state.load(Ordering::Acquire) == STATE_WAITING
                && t.io_kind.load(Ordering::Acquire) == IO_INIT_RPC
        }) {
            return Err(SysError::NoResource);
        }
        let mut msg = [0u8; galexy_abi::CHAN_MSG_MAX];
        let n = galexy_abi::INIT_RPC_HDR + bytes.len();
        msg[0] = admin;
        msg[1] = tty;
        msg[4..12].copy_from_slice(&debug_id.to_le_bytes());
        msg[12..20].copy_from_slice(&session_gen.to_le_bytes());
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
        let (id, end) = {
            let thread = threads.get(slot - 1).ok_or(SysError::BadCap)?;
            if !thread.is_user || thread.state.load(Ordering::Acquire) != STATE_RUNNING {
                return Err(SysError::BadCap);
            }
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
                    let free = threads[slot - 1]
                        .files
                        .iter()
                        .filter(|s| s.is_none())
                        .count();
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
                        let Some(index) = threads[slot - 1].files.iter().position(|s| s.is_none())
                        else {
                            continue;
                        };
                        let rights = file.rights;
                        threads[slot - 1].files[index] = Some(file);
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
            let thread = threads.get_mut(slot - 1).ok_or(SysError::BadCap)?;
            let Some(_) = thread.procs[pi].take() else {
                return Err(SysError::BadCap);
            };
            Ok(())
        });
    }
    let index = file_slot(cap)?;
    let (wake_pipe, wake_chan) = interrupts::without_interrupts(|| {
        let mut threads = THREADS.lock();
        let thread = threads.get_mut(slot - 1).ok_or(SysError::BadCap)?;
        let Some(file) = thread.files[index].take() else {
            return Err(SysError::BadCap);
        };
        Ok(match file.body {
            FileBody::Pipe { id, end } => {
                pipe::close_end(id, end);
                (Some(id), None)
            }
            FileBody::Channel { id, end } => {
                let held = channel::close_end(id, end);
                for cap in held.into_iter().flatten() {
                    release_file(cap);
                }
                (None, Some(id))
            }
            _ => (None, None),
        })
    })?;
    // Wake outside THREADS — the wake helpers take the same lock.
    if let Some(id) = wake_pipe {
        wake_pipe_waiters(id);
    }
    if let Some(id) = wake_chan {
        wake_channel_waiters(id);
    }
    Ok(())
}
