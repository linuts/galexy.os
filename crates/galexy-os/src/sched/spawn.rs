//! Queued spawn, shells, process Caps, and login sessions.
//!
//! The syscall copies a [`PendingSpawn`] and returns; the main loop
//! drains it. Wait, kill, grant, and the auth syscalls sit with that
//! path because they update the same parent and session fields.

use super::iowait::*;
use super::thread::*;

use super::{context, galfs, loader, lockout, pipe, ramdisk};
use crate::serial_println;
use crate::sync::Mutex;
use alloc::vec;
use core::sync::atomic::{AtomicU32, AtomicU8, Ordering};
use galexy_abi::{Cap, CapRights, SysError, SyscallResult, PROC_CAP_BASE};
use x86_64::instructions::interrupts;

/// Argument bytes copied onto a new task's stack. Matches the syscall cap.
pub(crate) const ARG_MAX: usize = galexy_abi::SPAWN_ARG_MAX;

/// One queued `spawn`. The syscall path only copies the name and the
/// argument (it runs IF=0); the main loop loads the ELF on the kernel
/// page table. Without [`galexy_abi::SPAWN_WAIT`], the caller wakes once
/// the child is running (with a process Cap in `rax`); with it, the
/// caller wakes when the child exits (exit code in `rax`).
pub(in crate::sched) struct PendingSpawn {
    pub(in crate::sched) name: [u8; 64],
    pub(in crate::sched) len: u8,
    pub(in crate::sched) arg: [u8; ARG_MAX],
    pub(in crate::sched) arg_len: u16,
    pub(in crate::sched) query: bool,
    /// Child may read the seat keyboard while the parent Cap-waits.
    pub(in crate::sched) keyboard: bool,
    /// Park until the child exits (not only until load finishes).
    pub(in crate::sched) wait_exit: bool,
    /// Console the child inherits from the task that asked.
    pub(in crate::sched) tty: u8,
    /// Milestone 54: init spawning an F-key seat (`shell`…`shell12`).
    pub(in crate::sched) seat: bool,
    /// Child inherits the waiter's galfs credentials.
    pub(in crate::sched) fs: galfs::FsCred,
    /// Non-zero: AND inherited token rights with this mask.
    pub(in crate::sched) rights_mask: u8,
    /// Child must change the default password before mutating galfs.
    pub(in crate::sched) must_change: bool,
    /// 1-based slot of the parked parent.
    pub(in crate::sched) waiter_slot: u8,
    /// Parent file-table indexes to move into the child, or `0xFF`.
    pub(in crate::sched) cap0: u8,
    /// Second moved file, or `0xFF`.
    pub(in crate::sched) cap1: u8,
    /// Do not publish the child as the TTY foreground (shell `cmd &`).
    pub(in crate::sched) no_fg: bool,
    /// Child may `spawn` (`rustc` starts `gxld`). Power stays off.
    pub(in crate::sched) loader: bool,
    /// Parent credentials used to read a path spawn. Empty for a label.
    pub(in crate::sched) load_fs: galfs::FsCred,
    pub(in crate::sched) armed: bool,
}

pub(in crate::sched) static PENDING_SPAWN: Mutex<PendingSpawn> = Mutex::new(PendingSpawn {
    name: [0; 64],
    len: 0,
    arg: [0; ARG_MAX],
    arg_len: 0,
    query: false,
    keyboard: false,
    wait_exit: false,
    tty: 0,
    seat: false,
    fs: galfs::FsCred::none(),
    rights_mask: 0,
    must_change: false,
    waiter_slot: 0,
    cap0: 0xFF,
    cap1: 0xFF,
    no_fg: false,
    loader: false,
    load_fs: galfs::FsCred::none(),
    armed: false,
});

/// Queues `name` and parks the current task.
///
/// `arg` is handed to the child. `query` adds the query grant on top of
/// the console. `keyboard` adds the keyboard grant (the parent should
/// Cap-wait so it is not also reading keys). `wait_exit` keeps the
/// caller parked until the child exits. `inherit` copies the parent's
/// galfs tokens (utilities). `rights_mask` ANDs those rights when
/// non-zero. The caller must already be a running user task. Lock
/// order: this takes `PENDING_SPAWN`, then `THREADS`.
#[allow(clippy::too_many_arguments)]
pub(crate) fn task_spawn(
    name: &str,
    arg: &[u8],
    query: bool,
    keyboard: bool,
    wait_exit: bool,
    inherit: bool,
    rights_mask: u8,
    caps: [u8; 2],
    no_fg: bool,
    loader: bool,
) -> Result<(), SysError> {
    if name.len() > 64 || arg.len() > ARG_MAX {
        return Err(SysError::BadValue);
    }
    let slot = current_slot();
    if slot == 0 {
        return Err(SysError::BadCap);
    }
    interrupts::without_interrupts(|| {
        let mut pending = PENDING_SPAWN.lock();
        if pending.armed {
            return Err(SysError::NoResource);
        }
        let mut threads = THREADS.lock();
        // Files, session, and the must-change gate live on the process
        // leader (Milestone 70); the park below is on the calling slot.
        let owner = running_proc_index(&threads, slot)?;
        {
            // F-key console names: only init may spawn seats (Milestone 54).
            let seat = is_console_shell_name(name);
            if seat && !threads[slot - 1].is_init {
                return Err(SysError::Unsupported);
            }
            let thread = &threads[owner];
            let (cap0, cap1) = (caps[0], caps[1]);
            if cap0 != 0xFF {
                let n = thread.files.len();
                if cap0 as usize >= n || thread.files[cap0 as usize].is_none() {
                    return Err(SysError::BadCap);
                }
                if cap1 != 0xFF
                    && (cap1 as usize >= n || cap1 == cap0 || thread.files[cap1 as usize].is_none())
                {
                    return Err(SysError::BadCap);
                }
            }
        }
        let seat = is_console_shell_name(name);
        // One live task per name: two `linger`s on one TTY would fight
        // the console. Wait/kill keys are Caps, not names.
        let name_busy = threads.iter().any(|thread| {
            let state = thread.state.load(Ordering::Acquire);
            thread.name() == name && (state == STATE_RUNNING || state == STATE_WAITING)
        });
        if name_busy {
            return Err(SysError::NoResource);
        }
        let parent_tty = threads[slot - 1].tty;
        let parent_must = threads[owner].must_change;
        let mut parent_fs = galfs::FsCred {
            root: threads[owner].fs_root,
            tokens: threads[owner].fs_tokens,
        };
        // Pre-login seats launch nothing: a task without a session root
        // (logged out, or never logged in) may only be init spawning a
        // seat. The shell refuses earlier; this is the kernel's answer.
        if !seat && parent_fs.root == galfs::NO_OBJECT {
            return Err(SysError::AccessDenied);
        }
        // Snapshot before attenuation. A path spawn reads the ELF with
        // the parent's own tokens even when the child does not inherit.
        let load_fs = if name.contains('/') {
            parent_fs
        } else {
            galfs::FsCred::none()
        };
        if inherit || wait_exit {
            galfs::attenuate_tokens(&mut parent_fs.tokens, rights_mask);
        }
        pending.name[..name.len()].copy_from_slice(name.as_bytes());
        pending.len = name.len() as u8;
        pending.arg[..arg.len()].copy_from_slice(arg);
        pending.arg_len = arg.len() as u16;
        pending.query = query;
        pending.keyboard = keyboard;
        pending.wait_exit = wait_exit;
        // Seat TTY: 1-based index in arg[0] (same as kernel spawn_shell_on).
        pending.tty = if seat {
            arg.first().copied().unwrap_or(1).saturating_sub(1).min(11)
        } else {
            parent_tty
        };
        pending.seat = seat;
        pending.rights_mask = rights_mask;
        pending.must_change = parent_must && (inherit || wait_exit) && !seat;
        pending.waiter_slot = slot as u8;
        pending.cap0 = caps[0];
        pending.cap1 = caps[1];
        pending.no_fg = no_fg;
        pending.loader = loader;
        pending.load_fs = load_fs;
        // Seats start logged out (no cards). Utilities inherit the session.
        // Bare programs keep the root for path context but hold no cards.
        pending.fs = if seat {
            galfs::unauth_cred()
        } else if inherit || wait_exit {
            parent_fs
        } else {
            galfs::FsCred {
                root: parent_fs.root,
                tokens: [galfs::Token::empty(); galfs::TOKEN_SLOTS],
            }
        };
        pending.armed = true;
        // 0 = waiting for load; drain installs Cap then either wakes or
        // sets wait_child_slot to the new child for exit wait.
        let thread = &mut threads[slot - 1];
        thread.wait_child_slot.store(0, Ordering::Relaxed);
        thread
            .wait_proc_index
            .store(WAIT_PROC_NONE, Ordering::Relaxed);
        thread.wait_child_gen.store(0, Ordering::Relaxed);
        thread.wait_for_exit.store(wait_exit, Ordering::Relaxed);
        thread.state.store(STATE_WAITING, Ordering::Release);
        Ok(())
    })
}

/// Largest ELF a path spawn will copy out of galfs.
const SPAWN_FILE_MAX: usize = 16 * 1024 * 1024;

/// Reads `name` for a path spawn.
///
/// Tokens are the caller's. A path outside them is
/// [`SysError::AccessDenied`], not a loader panic. The bytes must be an ELF.
pub fn read_spawn_file(
    root: u16,
    tokens: &[galfs::Token; galfs::TOKEN_SLOTS],
    name: &str,
) -> Result<alloc::vec::Vec<u8>, SysError> {
    let id = galfs::open_file(root, tokens, name)?;
    let len = galfs::object_len(id).ok_or(SysError::NotFound)?;
    if len == 0 || len > SPAWN_FILE_MAX {
        return Err(SysError::BadValue);
    }
    let mut buf = alloc::vec![0u8; len];
    let n = galfs::read_file_bytes(id, &mut buf).ok_or(SysError::NotFound)?;
    if n != len {
        return Err(SysError::BadValue);
    }
    loader::validate_elf(&buf)?;
    Ok(buf)
}

/// Loads a queued program, if the shell has asked for one.
///
/// Runs from the main loop: that context is the kernel page table, which
/// `spawn_program` clones. Installs a process Cap on the waiter; without
/// `wait_exit` wakes with Cap bits in `rax`, with `wait_exit` parks until
/// the child exits (exit code in `rax`).
pub fn drain_spawn() {
    // Threads queue beside programs; every main loop that drains one
    // drains the other.
    drain_thread_spawn();
    let queued = interrupts::without_interrupts(|| {
        let mut pending = PENDING_SPAWN.lock();
        if !pending.armed {
            return None;
        }
        let len = pending.len as usize;
        let mut name = [0u8; 64];
        name[..len].copy_from_slice(&pending.name[..len]);
        let arg_len = pending.arg_len as usize;
        let mut arg = [0u8; ARG_MAX];
        arg[..arg_len].copy_from_slice(&pending.arg[..arg_len]);
        let query = pending.query;
        let keyboard = pending.keyboard;
        let wait_exit = pending.wait_exit;
        let tty = pending.tty;
        let seat = pending.seat;
        let fs = pending.fs;
        let must_change = pending.must_change;
        let waiter_slot = pending.waiter_slot;
        let cap0 = pending.cap0;
        let cap1 = pending.cap1;
        let no_fg = pending.no_fg;
        let loader = pending.loader;
        let load_fs = pending.load_fs;
        pending.armed = false;
        Some((
            len,
            name,
            arg_len,
            arg,
            query,
            keyboard,
            wait_exit,
            tty,
            seat,
            fs,
            must_change,
            waiter_slot,
            cap0,
            cap1,
            no_fg,
            loader,
            load_fs,
        ))
    });
    let Some((
        len,
        name_raw,
        arg_len,
        arg,
        query,
        keyboard,
        wait_exit,
        tty,
        seat,
        fs,
        must_change,
        waiter_slot,
        cap0,
        cap1,
        no_fg,
        loader,
        load_fs,
    )) = queued
    else {
        return;
    };
    let name = core::str::from_utf8(&name_raw[..len]).unwrap_or("");
    if !spawn_frames_available() {
        serial_println!(
            "[sched] spawn '{}': low frames ({} < {}); waking NoResource",
            name,
            crate::arch::mm::free_frames(),
            SPAWN_FRAME_RESERVE
        );
        interrupts::without_interrupts(|| {
            let mut threads = THREADS.lock();
            wake_spawn_waiter(
                &mut threads,
                waiter_slot,
                SyscallResult::err(SysError::NoResource),
            );
        });
        return;
    }
    let mut grants = if seat {
        Grants::pre_login()
    } else if keyboard {
        Grants::console_keyboard(query)
    } else if query {
        Grants::console_query()
    } else {
        Grants::console()
    };
    // A seat stays pre-login. A named program may receive the loader
    // so it can start a helper (`rustc` → `gxld`). Power is not copied.
    if loader && !seat {
        grants.loader = true;
    }
    // Seats share the `shell` ELF under twelve reserved names.
    let elf_name = if seat { "shell" } else { name };
    // Always defer the first instruction. File Caps have to land first,
    // and a `SPAWN_WAIT` parent must be linked (`wait_child_slot`) before
    // the child can exit. Publishing early lets the other CPU run a short
    // program to completion in the gap; the exit then wakes nobody and
    // the parent parks forever (`test-soak`'s `hello`).
    let path_bytes = if name.contains('/') {
        Some(read_spawn_file(load_fs.root, &load_fs.tokens, name))
    } else {
        None
    };
    let child_slot = if let Some(loaded) = path_bytes {
        match loaded {
            Ok(bytes) => match loader::spawn_launched(
                name,
                &bytes,
                grants,
                &arg[..arg_len],
                tty,
                fs,
                waiter_slot,
                true,
            ) {
                Ok(slot) => Some(Ok(slot)),
                Err(err) => {
                    serial_println!("[sched] spawn '{}' refused ({:?})", name, err);
                    Some(Err(err))
                }
            },
            Err(err) => {
                serial_println!("[sched] spawn '{}' path refused ({:?})", name, err);
                Some(Err(err))
            }
        }
    } else if let Some(bytes) = ramdisk::find(elf_name) {
        let spawned = if seat {
            loader::spawn_launched_seat(
                name,
                bytes,
                grants,
                &arg[..arg_len],
                tty,
                fs,
                waiter_slot,
                true,
            )
        } else {
            loader::spawn_launched(
                name,
                bytes,
                grants,
                &arg[..arg_len],
                tty,
                fs,
                waiter_slot,
                true,
            )
        };
        match spawned {
            Ok(slot) => Some(Ok(slot)),
            Err(err) => {
                serial_println!("[sched] spawn '{}' refused ({:?})", name, err);
                Some(Err(err))
            }
        }
    } else {
        serial_println!(
            "[sched] spawn '{}' elf='{}' seat={} missing at drain",
            name,
            elf_name,
            seat
        );
        None
    };
    interrupts::without_interrupts(|| {
        let mut threads = THREADS.lock();
        let Some(child_slot) = child_slot else {
            wake_spawn_waiter(
                &mut threads,
                waiter_slot,
                SyscallResult::err(SysError::NotFound),
            );
            return;
        };
        let child_slot = match child_slot {
            Ok(slot) => slot,
            Err(err) => {
                wake_spawn_waiter(&mut threads, waiter_slot, SyscallResult::err(err));
                return;
            }
        };
        if must_change {
            if let Some(child) = threads.get_mut(child_slot as usize - 1) {
                child.must_change = true;
            }
        }
        if cap0 != 0xFF
            && move_spawn_caps(&mut threads, waiter_slot, child_slot, cap0, cap1).is_err()
        {
            release_deferred(&mut threads, child_slot);
            wake_spawn_waiter(
                &mut threads,
                waiter_slot,
                SyscallResult::err(SysError::BadCap),
            );
            return;
        }
        let Some(cap_bits) = install_proc_cap(&mut threads, waiter_slot, child_slot) else {
            release_deferred(&mut threads, child_slot);
            wake_spawn_waiter(
                &mut threads,
                waiter_slot,
                SyscallResult::err(SysError::NoResource),
            );
            return;
        };
        // Milestone 55: seat (or any) spawn makes the child the TTY's
        // foreground job Cap target for Ctrl-C. `SPAWN_NO_FG` skips that
        // so a background job is not what Ctrl-C kills.
        if !no_fg {
            if let Some(w) = threads.get(waiter_slot as usize - 1) {
                let tty = w.tty as usize;
                if tty < FG_SLOTS.len() {
                    let gen = threads[child_slot as usize - 1]
                        .cap_gen
                        .load(Ordering::Acquire);
                    FG_SLOTS[tty].store(child_slot, Ordering::Release);
                    FG_GENS[tty].store(gen, Ordering::Release);
                }
            }
        }
        if wait_exit {
            if let Some(w) = threads.get_mut(waiter_slot as usize - 1) {
                w.wait_child_slot.store(child_slot, Ordering::Release);
                w.wait_proc_index.store(WAIT_PROC_NONE, Ordering::Relaxed);
                w.wait_for_exit.store(true, Ordering::Release);
            }
        } else {
            wake_spawn_waiter(&mut threads, waiter_slot, SyscallResult::ok(cap_bits));
        }
        // Caps are in the child. Publish it only after that move so the
        // first instruction sees FILE_CAP_BASE.
        release_deferred(&mut threads, child_slot);
    });
}

/// One queued [`galexy_abi::Syscall::ThreadSpawn`] (Milestone 70). The
/// syscall records the request and parks the caller; the main loop maps
/// the stack and builds the record, since neither may allocate under the
/// syscall's IRQ gate.
pub(in crate::sched) struct PendingThread {
    pub(in crate::sched) entry: u64,
    pub(in crate::sched) arg: u64,
    pub(in crate::sched) pages: u16,
    /// 1-based slot of the parked caller (leader or sibling thread).
    pub(in crate::sched) waiter_slot: u8,
    /// 1-based slot of the process leader the thread joins.
    pub(in crate::sched) leader_slot: u8,
    pub(in crate::sched) armed: bool,
}

pub(in crate::sched) static PENDING_THREAD: Mutex<PendingThread> = Mutex::new(PendingThread {
    entry: 0,
    arg: 0,
    pages: 0,
    waiter_slot: 0,
    leader_slot: 0,
    armed: false,
});

/// Start of the per-slot thread stack arena inside a task's tree: the
/// image at `+0`, heap at `+512 MiB`, main stack at `+1 GiB`, then one
/// [`THREAD_STACK_SLOT`] window per thread slot from `+2 GiB`. All of it
/// sits under the task's single P4 entry, so `free_user_tree` reclaims
/// the thread stacks with the rest.
pub(in crate::sched) const THREAD_STACK_ARENA: u64 = 2 << 30;
/// Window reserved per thread slot (the stack grows down from its top; the
/// unmapped remainder is the guard). 32 MiB so a 17 MiB stack
/// ([`galexy_abi::THREAD_STACK_PAGES_MAX`]) keeps a guard page-run.
pub(in crate::sched) const THREAD_STACK_SLOT: u64 = 32 << 20;

/// Live records (running or parked) one address space may hold, leader
/// included. Zombies waiting for a join do not count.
pub(in crate::sched) const THREADS_PER_PROCESS: usize = galexy_abi::THREADS_PER_PROCESS as usize;

/// Top of the user stack for thread slot `slot` of a task whose image is
/// at [`galexy_abi::USER_IMAGE_BASE`].
pub(in crate::sched) fn thread_stack_top(slot: u8) -> u64 {
    galexy_abi::USER_IMAGE_BASE + THREAD_STACK_ARENA + u64::from(slot) * THREAD_STACK_SLOT
}

/// Queues a thread of the caller's process and parks the caller.
///
/// `entry` must lie inside the image window; `pages` is the stack size
/// (`0` = [`galexy_abi::THREAD_STACK_PAGES_DEFAULT`]). The caller wakes
/// with [`CapRights::PROC_PARENT`] Cap bits on the **leader's** process
/// table (so any thread may `Wait` on it), or an error. Lock order:
/// `PENDING_THREAD`, then `THREADS`.
pub(crate) fn task_thread_spawn(entry: u64, arg: u64, pages: u64) -> Result<(), SysError> {
    let pages = if pages == 0 {
        galexy_abi::THREAD_STACK_PAGES_DEFAULT
    } else {
        pages
    };
    if pages > galexy_abi::THREAD_STACK_PAGES_MAX {
        return Err(SysError::BadValue);
    }
    let image = galexy_abi::USER_IMAGE_BASE;
    if entry < image || entry >= image + loader::USER_IMAGE_WINDOW {
        return Err(SysError::BadValue);
    }
    let slot = current_slot();
    if slot == 0 {
        return Err(SysError::BadCap);
    }
    interrupts::without_interrupts(|| {
        let mut pending = PENDING_THREAD.lock();
        if pending.armed {
            return Err(SysError::NoResource);
        }
        let mut threads = THREADS.lock();
        let owner = running_proc_index(&threads, slot)?;
        let cr3 = threads[owner].cr3.load(Ordering::Acquire);
        let live = threads
            .iter()
            .filter(|t| {
                let state = t.state.load(Ordering::Acquire);
                t.cr3.load(Ordering::Acquire) == cr3
                    && (state == STATE_RUNNING || state == STATE_WAITING)
            })
            .count();
        if live >= THREADS_PER_PROCESS {
            return Err(SysError::NoResource);
        }
        pending.entry = entry;
        pending.arg = arg;
        pending.pages = pages as u16;
        pending.waiter_slot = slot as u8;
        pending.leader_slot = (owner + 1) as u8;
        pending.armed = true;
        let thread = &mut threads[slot - 1];
        clear_wait_fields(thread);
        thread.state.store(STATE_WAITING, Ordering::Release);
        Ok(())
    })
}

/// Builds the queued thread, if any (main-loop context: kernel tree).
///
/// Maps `pages` stack pages at the slot's arena window in the process
/// tree (pages a previous occupant of the slot left mapped are zeroed and
/// reused), fabricates the ring-3 frame with `RDI = arg`, installs a join
/// Cap on the leader, wakes the caller with the Cap bits, and releases
/// the thread.
pub fn drain_thread_spawn() {
    let queued = interrupts::without_interrupts(|| {
        let mut pending = PENDING_THREAD.lock();
        if !pending.armed {
            return None;
        }
        pending.armed = false;
        Some((
            pending.entry,
            pending.arg,
            pending.pages,
            pending.waiter_slot,
            pending.leader_slot,
        ))
    });
    let Some((entry, arg, pages, waiter_slot, leader_slot)) = queued else {
        return;
    };
    let pages = pages as usize;
    let leader = interrupts::without_interrupts(|| {
        let mut threads = THREADS.lock();
        let wi = waiter_slot as usize - 1;
        let li = leader_slot as usize - 1;
        if wi >= threads.len() || li >= threads.len() {
            return None;
        }
        // The process died (exit, kill, fault) between the syscall and
        // this drain: the caller was tombstoned with it and wakes nobody.
        if threads[wi].state.load(Ordering::Acquire) != STATE_WAITING {
            return None;
        }
        let leader_state = threads[li].state.load(Ordering::Acquire);
        if leader_state != STATE_RUNNING && leader_state != STATE_WAITING {
            return None;
        }
        let occupied = threads
            .iter()
            .filter(|t| t.state.load(Ordering::Acquire) != STATE_FREED)
            .count();
        if occupied + 2 > MAX_THREADS || !spawn_frames_available() {
            wake_spawn_waiter(
                &mut threads,
                waiter_slot,
                SyscallResult::err(SysError::NoResource),
            );
            return None;
        }
        Some(LeaderSnapshot::of(&threads, li))
    });
    let Some(leader) = leader else {
        return;
    };
    // Heap growth can broadcast a TLB shootdown: allocate before the gate.
    let kstack = vec![0u8; THREAD_STACK_SIZE];
    let slot = register_thread(&leader, kstack);
    let stack_top = thread_stack_top(slot);
    let stack_base = stack_top - (pages as u64) * 4096;
    let ctx = interrupts::without_interrupts(|| {
        assert!(
            crate::arch::mm::on_kernel_tree(),
            "drain_thread_spawn: must run on the kernel tree (main-loop context)"
        );
        use x86_64::structures::paging::{Mapper, Page, PageTableFlags, PhysFrame, Translate};
        use x86_64::{PhysAddr, VirtAddr};
        let root = PhysFrame::from_start_address(PhysAddr::new(leader.cr3))
            .expect("drain_thread_spawn: corrupt leader CR3");
        let flags = PageTableFlags::PRESENT
            | PageTableFlags::WRITABLE
            | PageTableFlags::USER_ACCESSIBLE
            | PageTableFlags::NO_EXECUTE;
        let mut top_frame: Option<PhysFrame> = None;
        let mut short = false;
        // SAFETY: the leader's tree is a FreshL4 root; the main loop runs
        // on the kernel tree, so it is not active here. Every frame we
        // touch is either freshly allocated or already owned by this tree.
        unsafe {
            crate::arch::mm::with_table(root, |mapper| {
                for i in 0..pages {
                    let addr = VirtAddr::new(stack_base + (i as u64) * 4096);
                    let frame = match mapper.translate_addr(addr) {
                        Some(phys) => PhysFrame::containing_address(phys),
                        None => {
                            let Some(frame) = crate::arch::mm::allocate_frame() else {
                                short = true;
                                return;
                            };
                            let page = Page::containing_address(addr);
                            match mapper.map_to(
                                page,
                                frame,
                                flags,
                                &mut crate::arch::mm::TaskFrameAlloc,
                            ) {
                                Ok(flush) => flush.flush(),
                                Err(_) => {
                                    short = true;
                                    return;
                                }
                            }
                            frame
                        }
                    };
                    core::ptr::write_bytes(
                        crate::arch::mm::frame_virt(frame.start_address()).as_mut_ptr::<u64>(),
                        0,
                        512,
                    );
                    top_frame = Some(frame);
                }
            });
        }
        let (Some(top), false) = (top_frame, short) else {
            return None;
        };
        let fab = crate::arch::mm::frame_virt(top.start_address()) + 4096u64;
        let (cs, ss) = context::user_cs_ss();
        // SAFETY: `fab` is the phys-map image of the fresh top stack page;
        // `entry` was range-checked against the image window (a bad
        // address faults in ring 3 and takes only this process down).
        Some(unsafe {
            context::init_user_frame(fab.as_u64(), stack_top - 512, entry, arg, 0, cs, ss)
        })
    });
    interrupts::without_interrupts(|| {
        let mut threads = THREADS.lock();
        let ti = slot as usize - 1;
        let abandon = |threads: &mut [Thread], err: SysError| {
            // Never ran: tombstone it waited so the reaper takes it
            // without a join, and leave the shared tree alone.
            threads[ti].exit_waited.store(true, Ordering::Release);
            threads[ti].state.store(STATE_EXITED, Ordering::Release);
            wake_spawn_waiter(threads, waiter_slot, SyscallResult::err(err));
        };
        let Some(ctx) = ctx else {
            abandon(&mut threads, SysError::NoResource);
            return;
        };
        threads[ti].ctx.store(ctx, Ordering::Release);
        let Some(cap_bits) = install_proc_cap(&mut threads, leader_slot, slot) else {
            abandon(&mut threads, SysError::NoResource);
            return;
        };
        serial_println!(
            "[sched] thread '{}' of '{}' ready (slot {}, {} stack page(s), entry {:#x})",
            threads[ti].name(),
            leader.name(),
            slot,
            pages,
            entry
        );
        wake_spawn_waiter(&mut threads, waiter_slot, SyscallResult::ok(cap_bits));
        release_deferred(&mut threads, slot);
    });
}

/// Names of the twelve shells. F1 keeps `shell` so a faulted shell is
/// still the task the restart log and the typing tests already know.
pub(in crate::sched) const SHELL_NAMES: [&str; 12] = [
    "shell", "shell2", "shell3", "shell4", "shell5", "shell6", "shell7", "shell8", "shell9",
    "shell10", "shell11", "shell12",
];

/// True when `name` is reserved for an F-key console shell.
pub fn is_console_shell_name(name: &str) -> bool {
    SHELL_NAMES.contains(&name)
}

/// True when a task named `name` is running or parked on a load.
pub(in crate::sched) fn named_is_live(name: &str) -> bool {
    interrupts::without_interrupts(|| {
        THREADS.lock().iter().any(|thread| {
            let state = thread.state.load(Ordering::Acquire);
            thread.name() == name && (state == STATE_RUNNING || state == STATE_WAITING)
        })
    })
}

/// True when a task named `shell` is running or parked on a load.
pub fn shell_is_live() -> bool {
    named_is_live("shell")
}

/// True when every F-key seat name is live (Milestone 54 boot gate).
pub fn seats_are_live() -> bool {
    SHELL_NAMES.iter().all(|name| named_is_live(name))
}

/// Per-TTY foreground job (Milestone 55): child slot + cap_gen.
pub(in crate::sched) const FG_COUNT: usize = crate::drivers::keyboard::TTY_COUNT;
pub(in crate::sched) static FG_SLOTS: [AtomicU8; FG_COUNT] = [const { AtomicU8::new(0) }; FG_COUNT];
pub(in crate::sched) static FG_GENS: [AtomicU32; FG_COUNT] =
    [const { AtomicU32::new(0) }; FG_COUNT];

/// Test helper: pin TTY `tty`'s foreground job to `child_slot`.
pub fn set_foreground_for_test(tty: u8, child_slot: u8) {
    let tty = tty as usize;
    if tty >= FG_COUNT || child_slot == 0 {
        return;
    }
    interrupts::without_interrupts(|| {
        let threads = THREADS.lock();
        let Some(t) = threads.get(child_slot as usize - 1) else {
            return;
        };
        let gen = t.cap_gen.load(Ordering::Acquire);
        FG_SLOTS[tty].store(child_slot, Ordering::Release);
        FG_GENS[tty].store(gen, Ordering::Release);
    });
}

/// Ctrl-C / signals-lite: kill the foreground job on `tty` if live.
///
/// Returns true when a foreground task was stopped (caller should not
/// deliver `^C` into the keyboard ring). The process Cap holder Cap-waits
/// exit status `137`.
pub fn interrupt_foreground(tty: u8) -> bool {
    let tty = tty as usize;
    if tty >= FG_COUNT {
        return false;
    }
    interrupts::without_interrupts(|| {
        let mut threads = THREADS.lock();
        let slot = FG_SLOTS[tty].load(Ordering::Acquire);
        let gen = FG_GENS[tty].load(Ordering::Acquire);
        if slot == 0 || slot as usize > threads.len() {
            return false;
        }
        let i = slot as usize - 1;
        if threads[i].cap_gen.load(Ordering::Acquire) != gen {
            FG_SLOTS[tty].store(0, Ordering::Release);
            return false;
        }
        if threads[i].is_init {
            return false;
        }
        let state = threads[i].state.load(Ordering::Acquire);
        if state != STATE_RUNNING && state != STATE_WAITING {
            FG_SLOTS[tty].store(0, Ordering::Release);
            return false;
        }
        if state == STATE_WAITING {
            interrupt_io_waiter(&mut threads, i);
        }
        threads[i].exit_code.store(EXIT_KILLED, Ordering::Release);
        threads[i].state.store(STATE_EXITED, Ordering::Release);
        let mut raw = [0u8; NAME_CAP];
        let n = threads[i].name_len as usize;
        raw[..n].copy_from_slice(&threads[i].name_bytes[..n]);
        let name = core::str::from_utf8(&raw[..n]).unwrap_or("");
        serial_println!(
            "[sched] fg job '{}' killed by Ctrl-C on tty{} id={}",
            name,
            tty + 1,
            threads[i].debug_id
        );
        wake_exit_waiters(&mut threads, slot, EXIT_KILLED);
        if threads[i].leader_slot == 0 {
            exit_siblings(&mut threads, slot as usize, EXIT_KILLED);
        }
        FG_SLOTS[tty].store(0, Ordering::Release);
        true
    })
}

/// True when a user `spawn` is queued (single-slot PENDING_SPAWN).
pub fn spawn_is_pending() -> bool {
    interrupts::without_interrupts(|| PENDING_SPAWN.lock().armed)
}

/// Loads one interactive shell on `tty` when that name is not already live.
pub(in crate::sched) fn ensure_one_shell(name: &str, tty: u8) {
    if named_is_live(name) {
        return;
    }
    let Some(bytes) = ramdisk::find("shell") else {
        return;
    };
    if name == "shell" {
        serial_println!("[sched] shell is gone; loading it again");
    } else {
        serial_println!("[sched] {} is gone; loading it again", name);
    }
    // Ramdisk `shell` is a build input. Refusal is a kernel bug.
    loader::spawn_shell_on(name, bytes, tty).expect("ramdisk shell");
}

/// Loads every F-key shell that has exited.
///
/// Other tasks are left alone. Each new shell starts logged out (pre-login
/// grants) on the console it had. A missing ramdisk entry does nothing.
pub fn ensure_shell() {
    for (tty, name) in SHELL_NAMES.iter().enumerate() {
        ensure_one_shell(name, tty as u8);
    }
}

/// Starts one shell on every F-key before the main loop reports ready.
pub fn spawn_all_shells() {
    let Some(bytes) = ramdisk::find("shell") else {
        return;
    };
    for (tty, name) in SHELL_NAMES.iter().enumerate() {
        loader::spawn_shell_on(name, bytes, tty as u8).expect("ramdisk shell");
    }
}

/// True when a live init task exists (Cap-kill must return AccessDenied).
pub fn init_unkillable() -> bool {
    init_slot().is_some()
}

/// Live slot of userspace init, if any.
pub fn init_slot() -> Option<u8> {
    interrupts::without_interrupts(|| {
        THREADS.lock().iter().enumerate().find_map(|(i, t)| {
            if !t.is_init {
                return None;
            }
            let state = t.state.load(Ordering::Acquire);
            if state == STATE_RUNNING || state == STATE_WAITING {
                Some((i + 1) as u8)
            } else {
                None
            }
        })
    })
}

/// Loads ramdisk `init` once (Milestone 53). Returns false if missing.
pub fn spawn_init() -> bool {
    let Some(bytes) = ramdisk::find("init") else {
        return false;
    };
    if init_slot().is_some() {
        return true;
    }
    loader::spawn_init(bytes).expect("ramdisk init");
    serial_println!("[sched] init loaded (orphan root)");
    true
}

/// Moves process Caps from a reaped parent to init and reparents children.
pub(in crate::sched) fn transfer_orphans_to_init(
    threads: &mut [Thread],
    dead_slot: u8,
    dead_index: usize,
) {
    let init_slot = threads.iter().enumerate().find_map(|(i, t)| {
        if t.is_init {
            let state = t.state.load(Ordering::Acquire);
            if state == STATE_RUNNING || state == STATE_WAITING {
                return Some((i + 1) as u8);
            }
        }
        None
    });
    let Some(init_slot) = init_slot else {
        for t in threads.iter_mut() {
            if t.parent_slot == dead_slot {
                t.parent_slot = 0;
            }
        }
        return;
    };
    // Take the whole array (Copy): no heap allocation under THREADS.
    let moved = core::mem::replace(&mut threads[dead_index].procs, [None; MAX_PROC_CAPS]);
    let ii = init_slot as usize - 1;
    for handle in moved.into_iter().flatten() {
        let ci = handle.child_slot as usize;
        if ci == 0 || ci > threads.len() {
            continue;
        }
        if threads[ci - 1].cap_gen.load(Ordering::Acquire) != handle.gen {
            continue;
        }
        let state = threads[ci - 1].state.load(Ordering::Acquire);
        if state == STATE_FREED {
            continue;
        }
        if let Some(empty) = threads[ii].procs.iter().position(|p| p.is_none()) {
            threads[ii].procs[empty] = Some(handle);
        }
    }
    for t in threads.iter_mut() {
        if t.parent_slot == dead_slot {
            t.parent_slot = init_slot;
        }
    }
}

/// Installs a [`PROC_PARENT`] Cap on `waiter_slot` for `child_slot`.
/// Returns Cap bits, or `None` if the parent's process-Cap table is full.
pub(in crate::sched) fn install_proc_cap(
    threads: &mut [Thread],
    waiter_slot: u8,
    child_slot: u8,
) -> Option<u64> {
    if waiter_slot == 0 || child_slot == 0 {
        return None;
    }
    // A thread's process Caps land on its leader's table.
    let wi = proc_index(threads, waiter_slot as usize)?;
    let ci = child_slot as usize - 1;
    let gen = threads.get(ci)?.cap_gen.load(Ordering::Acquire);
    let parent = threads.get_mut(wi)?;
    let slot = parent.procs.iter().position(|p| p.is_none())?;
    parent.procs[slot] = Some(ProcHandle {
        child_slot,
        gen,
        rights: CapRights::PROC_PARENT,
    });
    Some(Cap::new(PROC_CAP_BASE + slot as u64, CapRights::PROC_PARENT).bits())
}

/// Stamps `result` into a parked waiter's saved frame and marks it runnable.
pub(in crate::sched) fn wake_spawn_waiter(
    threads: &mut [Thread],
    waiter_slot: u8,
    result: SyscallResult,
) {
    if waiter_slot == 0 {
        return;
    }
    let Some(thread) = threads.get_mut(waiter_slot as usize - 1) else {
        return;
    };
    if thread.state.load(Ordering::Acquire) != STATE_WAITING {
        return;
    }
    clear_wait_fields(thread);
    stamp_waiter_frame(thread, result);
    set_running(thread);
}

/// Writes syscall result registers into a parked task's saved context.
pub(in crate::sched) fn stamp_waiter_frame(thread: &Thread, result: SyscallResult) {
    let ctx_ptr = thread.ctx.load(Ordering::Acquire);
    if ctx_ptr == 0 {
        return;
    }
    // SAFETY: waiter is STATE_WAITING; ctx points at its saved syscall frame.
    let ctx = ctx_ptr as *mut context::Context;
    unsafe {
        (*ctx).rax = result.value;
        (*ctx).rdx = if result.ok { 1 } else { 0 };
    }
}

/// Wakes every task Cap-waiting on `child_slot` for exit, stamping exit codes.
pub(in crate::sched) fn wake_exit_waiters(threads: &mut [Thread], child_slot: u8, exit_code: u64) {
    let mut any = false;
    for i in 0..threads.len() {
        let thread = &threads[i];
        if thread.wait_child_slot.load(Ordering::Acquire) != child_slot {
            continue;
        }
        if !thread.wait_for_exit.load(Ordering::Acquire) {
            continue;
        }
        if thread.state.load(Ordering::Acquire) != STATE_WAITING {
            continue;
        }
        let pi = thread.wait_proc_index.load(Ordering::Acquire);
        if pi != WAIT_PROC_NONE {
            let pi = pi as usize;
            // The Cap table is the waiter's process table (its leader's
            // when the waiter is a thread).
            if let Some(owner) = proc_index(threads, i + 1) {
                if pi < MAX_PROC_CAPS {
                    if let Some(handle) = threads[owner].procs[pi] {
                        if handle.child_slot == child_slot {
                            threads[owner].procs[pi] = None;
                        }
                    }
                }
            }
        }
        let thread = &threads[i];
        clear_wait_fields(thread);
        stamp_waiter_frame(thread, SyscallResult::ok(exit_code));
        set_running(thread);
        any = true;
    }
    // Only mark waited when a Cap-waiter (or SPAWN_WAIT) collected the
    // status. Kill alone must leave a zombie until Wait / Cap drop.
    if any {
        if let Some(child) = threads.get(child_slot as usize - 1) {
            child.exit_waited.store(true, Ordering::Release);
        }
    }
}

/// Calls `each` with every galfs path the current task may list
/// (`Desktop/`, `dan@Desktop/notes`). The bytes are only valid inside
/// the callback.
pub(crate) fn for_each_scratch_path(mut each: impl FnMut(&[u8])) {
    let slot = current_slot();
    if slot == 0 {
        return;
    }
    interrupts::without_interrupts(|| {
        let threads = THREADS.lock();
        let Some(owner) = proc_index(&threads, slot) else {
            return;
        };
        let thread = &threads[owner];
        galfs::for_each_visible(thread.fs_root, &thread.fs_tokens, |path| each(path));
    });
}

/// Exit status stamped when a task is stopped by [`task_kill`].
pub(in crate::sched) const EXIT_KILLED: u64 = 137;

/// Parks until the process Cap's child exits; returns the exit code.
///
/// On success the Cap slot is cleared (stale). If the child has already
/// exited, returns immediately (`Some(code)`). Otherwise parks and returns
/// `None` (caller must hand off the CPU). The Cap stays in the table until
/// the exit wake, so an init control wake can return
/// [`SysError::Interrupted`] and the caller can wait on the same bits.
pub(crate) fn task_wait(cap: Cap, poll: bool) -> Result<Option<u64>, SysError> {
    if !cap.rights().contains(CapRights::PROC_WAIT) {
        return Err(SysError::AccessDenied);
    }
    let pi = proc_slot(cap)?;
    let slot = current_slot();
    if slot == 0 {
        return Err(SysError::BadCap);
    }
    interrupts::without_interrupts(|| {
        let mut threads = THREADS.lock();
        let owner = running_proc_index(&threads, slot)?;
        let handle = threads[owner].procs[pi].ok_or(SysError::BadCap)?;
        if !handle.rights.contains(CapRights::PROC_WAIT) {
            return Err(SysError::AccessDenied);
        }
        let ci = handle.child_slot as usize;
        if ci == 0 || ci > threads.len() {
            threads[owner].procs[pi] = None;
            return Err(SysError::BadCap);
        }
        // A thread joining itself would park forever.
        if ci == slot {
            return Err(SysError::BadValue);
        }
        let gen = threads[ci - 1].cap_gen.load(Ordering::Acquire);
        if gen != handle.gen {
            threads[owner].procs[pi] = None;
            return Err(SysError::BadCap);
        }
        let state = threads[ci - 1].state.load(Ordering::Acquire);
        if state == STATE_EXITED || state == STATE_FREED {
            let code = threads[ci - 1].exit_code.load(Ordering::Acquire);
            threads[ci - 1].exit_waited.store(true, Ordering::Release);
            threads[owner].procs[pi] = None;
            return Ok(Some(code));
        }
        if state != STATE_RUNNING && state != STATE_WAITING {
            threads[owner].procs[pi] = None;
            return Err(SysError::BadCap);
        }
        // Still alive. A poll returns without parking so a supervisor can
        // reap a different child that has already exited.
        if poll {
            return Err(SysError::NoResource);
        }
        // A control message that arrived while init was still running
        // must not sit behind this park. The send path only wakes a
        // task that is already WAITING, so the park itself checks the
        // queue under `THREADS` (enqueue takes that lock first).
        if threads[slot - 1].is_init && init_ctrl_queued() {
            return Err(SysError::Interrupted);
        }
        // The Cap stays installed until an exit wake clears it. A control
        // wake (init only) returns Interrupted and the caller waits again
        // on the same bits. Orphan adoption must not reuse this slot.
        let child_slot = handle.child_slot;
        let tty = threads[slot - 1].tty as usize;
        if tty < FG_COUNT {
            // `fg` (and any Cap-wait) publishes this child for Ctrl-C.
            FG_SLOTS[tty].store(child_slot, Ordering::Release);
            FG_GENS[tty].store(gen, Ordering::Release);
        }
        let waiter = &mut threads[slot - 1];
        waiter.wait_child_slot.store(child_slot, Ordering::Release);
        waiter.wait_proc_index.store(pi as u8, Ordering::Release);
        waiter.wait_child_gen.store(gen, Ordering::Release);
        waiter.wait_for_exit.store(true, Ordering::Release);
        waiter.state.store(STATE_WAITING, Ordering::Release);
        Ok(None)
    })
}

/// Stops the task named by a process Cap (`PROC_KILL`).
///
/// Idempotent if the child has already exited. Does not reap — the
/// holder still [`task_wait`]s (or drops the Cap) for zombie cleanup.
pub(crate) fn task_kill(cap: Cap) -> Result<(), SysError> {
    if !cap.rights().contains(CapRights::PROC_KILL) {
        return Err(SysError::AccessDenied);
    }
    let pi = proc_slot(cap)?;
    let slot = current_slot();
    if slot == 0 {
        return Err(SysError::BadCap);
    }
    interrupts::without_interrupts(|| {
        let mut threads = THREADS.lock();
        let handle = {
            let thread = threads.get(slot - 1).ok_or(SysError::BadCap)?;
            if !thread.is_user {
                return Err(SysError::BadCap);
            }
            let owner = proc_index(&threads, slot).ok_or(SysError::BadCap)?;
            threads[owner].procs[pi].ok_or(SysError::BadCap)?
        };
        if !handle.rights.contains(CapRights::PROC_KILL) {
            return Err(SysError::AccessDenied);
        }
        let ci = handle.child_slot as usize;
        if ci == 0 || ci > threads.len() {
            return Err(SysError::BadCap);
        }
        if threads[ci - 1].cap_gen.load(Ordering::Acquire) != handle.gen {
            return Err(SysError::BadCap);
        }
        let state = threads[ci - 1].state.load(Ordering::Acquire);
        if state == STATE_EXITED || state == STATE_FREED {
            return Ok(());
        }
        if state != STATE_RUNNING && state != STATE_WAITING {
            return Err(SysError::BadCap);
        }
        // Milestone 53: init is immortal to user kill.
        if threads[ci - 1].is_init {
            return Err(SysError::AccessDenied);
        }
        // Milestone 57: cancel sleep / I/O parks with Interrupted so a
        // kill of a blocked task never leaves a waiter stranded; Cap-wait
        // still observes EXIT_KILLED below.
        if state == STATE_WAITING {
            interrupt_io_waiter(&mut threads, ci - 1);
        }
        threads[ci - 1]
            .exit_code
            .store(EXIT_KILLED, Ordering::Release);
        threads[ci - 1].state.store(STATE_EXITED, Ordering::Release);
        let mut raw = [0u8; NAME_CAP];
        let n = threads[ci - 1].name_len as usize;
        raw[..n].copy_from_slice(&threads[ci - 1].name_bytes[..n]);
        let debug_id = threads[ci - 1].debug_id;
        let name = core::str::from_utf8(&raw[..n]).unwrap_or("");
        serial_println!(
            "[sched] task '{}' killed code={} id={}",
            name,
            EXIT_KILLED,
            debug_id
        );
        wake_exit_waiters(&mut threads, handle.child_slot, EXIT_KILLED);
        // Killing a process leader ends its threads; killing one thread
        // (by its join Cap) leaves the rest of the process alone.
        if threads[ci - 1].leader_slot == 0 {
            exit_siblings(&mut threads, ci, EXIT_KILLED);
        }
        Ok(())
    })
}

/// Test helper: install `rights` on `object` for a live user task by name.
/// Callable from the kernel main loop (slot 0) — no caller-card check.
pub fn test_push_token(target: &str, object: u16, rights: u8) -> Result<(), SysError> {
    interrupts::without_interrupts(|| {
        let mut threads = THREADS.lock();
        let Some(ti) = threads.iter().position(|t| {
            t.is_user && t.state.load(Ordering::Acquire) == STATE_RUNNING && t.name() == target
        }) else {
            return Err(SysError::NotFound);
        };
        galfs::push_token(&mut threads[ti].fs_tokens, object, rights)
    })
}

/// Test helper: drop `rights` on `object` for a live user task by name.
pub fn test_revoke_token(target: &str, object: u16, rights: u8) -> Result<(), SysError> {
    interrupts::without_interrupts(|| {
        let mut threads = THREADS.lock();
        let Some(ti) = threads.iter().position(|t| {
            t.is_user && t.state.load(Ordering::Acquire) == STATE_RUNNING && t.name() == target
        }) else {
            return Err(SysError::NotFound);
        };
        galfs::revoke_token(&mut threads[ti].fs_tokens, object, rights)
    })
}

/// Installs a galfs token on a live user task named `target`.
///
/// The current task must already hold every bit in `rights` on the
/// resolved object. Lock order: [`THREADS`] then the galfs table.
pub(crate) fn task_grant(path: &str, rights: u8, target: &str) -> Result<(), SysError> {
    let slot = current_slot();
    if slot == 0 {
        return Err(SysError::BadCap);
    }
    let mut actor_buf = [0u8; 32];
    let mut actor_len = 0usize;
    let result = interrupts::without_interrupts(|| {
        let mut threads = THREADS.lock();
        let (fs_root, fs_tokens) = {
            let caller = &threads[running_proc_index(&threads, slot)?];
            deny_must_change(caller)?;
            (caller.fs_root, caller.fs_tokens)
        };
        actor_len = galfs::name_of_root(fs_root, &mut actor_buf).unwrap_or(0);
        let need = rights & galfs::RIGHT_ALL;
        if need == 0 {
            return Err(SysError::BadValue);
        }
        let object = galfs::resolve_and_check(fs_root, &fs_tokens, path, need)?;
        let Some(ti) = threads.iter().position(|t| {
            t.is_user && t.state.load(Ordering::Acquire) == STATE_RUNNING && t.name() == target
        }) else {
            return Err(SysError::NotFound);
        };
        let ti = proc_index(&threads, ti + 1).ok_or(SysError::NotFound)?;
        galfs::push_token(&mut threads[ti].fs_tokens, object, rights)
    });
    log_token(
        "grant", &actor_buf, actor_len, path, rights, target, &result,
    );
    result
}

/// Drops galfs token rights on a live user task named `target`.
pub(crate) fn task_revoke(path: &str, rights: u8, target: &str) -> Result<(), SysError> {
    let slot = current_slot();
    if slot == 0 {
        return Err(SysError::BadCap);
    }
    let mut actor_buf = [0u8; 32];
    let mut actor_len = 0usize;
    let result = interrupts::without_interrupts(|| {
        let mut threads = THREADS.lock();
        let (fs_root, fs_tokens) = {
            let caller = &threads[running_proc_index(&threads, slot)?];
            deny_must_change(caller)?;
            (caller.fs_root, caller.fs_tokens)
        };
        actor_len = galfs::name_of_root(fs_root, &mut actor_buf).unwrap_or(0);
        let need = rights & galfs::RIGHT_ALL;
        if need == 0 {
            return Err(SysError::BadValue);
        }
        let object = galfs::resolve_and_check(fs_root, &fs_tokens, path, need)?;
        let Some(ti) = threads.iter().position(|t| {
            t.is_user && t.state.load(Ordering::Acquire) == STATE_RUNNING && t.name() == target
        }) else {
            return Err(SysError::NotFound);
        };
        let ti = proc_index(&threads, ti + 1).ok_or(SysError::NotFound)?;
        galfs::revoke_token(&mut threads[ti].fs_tokens, object, rights)
    });
    log_token(
        "revoke", &actor_buf, actor_len, path, rights, target, &result,
    );
    result
}

/// Serial line for grant/revoke. No password bytes. Rights are the raw
/// mask (`RIGHT_READ` = 1 … `RIGHT_ONCE` = 128).
pub(in crate::sched) fn log_token(
    op: &str,
    actor_buf: &[u8],
    actor_len: usize,
    path: &str,
    rights: u8,
    target: &str,
    result: &Result<(), SysError>,
) {
    let actor = core::str::from_utf8(&actor_buf[..actor_len]).unwrap_or("-");
    let actor = if actor.is_empty() { "-" } else { actor };
    match result {
        Ok(()) => serial_println!(
            "[auth] {} actor={} path={} rights={:#x} target={}",
            op,
            actor,
            path,
            rights,
            target
        ),
        Err(err) => serial_println!(
            "[auth] {} fail actor={} path={} rights={:#x} target={} err={}",
            op,
            actor,
            path,
            rights,
            target,
            *err as u64
        ),
    }
}

/// Creates a pipe and installs both ends in the caller's file table.
/// Returns `(read_cap, write_cap)`.
pub(crate) fn task_pipe() -> Result<(Cap, Cap), SysError> {
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
        let id = pipe::alloc()?;
        let ri = free[0];
        let wi = free[1];
        let read_rights = CapRights::READ;
        let write_rights = CapRights::WRITE;
        thread.files[ri] = Some(OpenFile {
            body: FileBody::Pipe {
                id,
                end: pipe::PipeEnd::Read,
            },
            offset: 0,
            rights: read_rights,
        });
        thread.files[wi] = Some(OpenFile {
            body: FileBody::Pipe {
                id,
                end: pipe::PipeEnd::Write,
            },
            offset: 0,
            rights: write_rights,
        });
        Ok((
            Cap::new(galexy_abi::FILE_CAP_BASE + ri as u64, read_rights),
            Cap::new(galexy_abi::FILE_CAP_BASE + wi as u64, write_rights),
        ))
    })
}

/// Moves up to two of the parent's file slots into the child's first
/// slots. `cap1 == 0xFF` moves only `cap0`. The child sees them at
/// `FILE_CAP_BASE` upward, before its first instruction.
pub(in crate::sched) fn move_spawn_caps(
    threads: &mut [Thread],
    waiter_slot: u8,
    child_slot: u8,
    cap0: u8,
    cap1: u8,
) -> Result<(), SysError> {
    let parent = proc_index(threads, waiter_slot as usize).ok_or(SysError::BadCap)?;
    let child = child_slot as usize - 1;
    if child >= threads.len() || parent == child {
        return Err(SysError::BadCap);
    }
    let mut srcs = [cap0, cap1];
    if cap1 == 0xFF {
        srcs[1] = 0xFF;
    }
    for (dest, src) in srcs.into_iter().enumerate() {
        if src == 0xFF {
            continue;
        }
        let src = src as usize;
        if src >= threads[parent].files.len() || dest >= threads[child].files.len() {
            return Err(SysError::BadCap);
        }
        let Some(file) = threads[parent].files[src].take() else {
            return Err(SysError::BadCap);
        };
        if threads[child].files[dest].is_some() {
            threads[parent].files[src] = Some(file);
            return Err(SysError::NoResource);
        }
        threads[child].files[dest] = Some(file);
    }
    Ok(())
}

/// Moves an open file/pipe **or process Cap** from the caller to `target`.
/// Returns the new Cap (rights attenuated for process Caps).
pub(crate) fn task_give(cap: Cap, target: &str) -> Result<Cap, SysError> {
    if proc_slot(cap).is_ok() {
        return task_give_proc(cap, target);
    }
    let index = file_slot(cap)?;
    let slot = current_slot();
    if slot == 0 {
        return Err(SysError::BadCap);
    }
    interrupts::without_interrupts(|| {
        let mut threads = THREADS.lock();
        let owner = running_proc_index(&threads, slot)?;
        let file = threads[owner].files[index].take().ok_or(SysError::BadCap)?;
        let target_index = threads
            .iter()
            .position(|t| {
                t.is_user && t.state.load(Ordering::Acquire) == STATE_RUNNING && t.name() == target
            })
            .and_then(|ti| proc_index(&threads, ti + 1));
        let Some(ti) = target_index else {
            // Put it back — target missing.
            threads[owner].files[index] = Some(file);
            return Err(SysError::NotFound);
        };
        if ti == owner {
            threads[ti].files[index] = Some(file);
            return Err(SysError::BadValue);
        }
        let Some(dest) = threads[ti].files.iter().position(|s| s.is_none()) else {
            threads[owner].files[index] = Some(file);
            return Err(SysError::NoResource);
        };
        let rights = file.rights;
        threads[ti].files[dest] = Some(file);
        Ok(Cap::new(galexy_abi::FILE_CAP_BASE + dest as u64, rights))
    })
}

/// Moves a process Cap to `target`. Requires [`CapRights::PROC_TRANSFER`].
pub(in crate::sched) fn task_give_proc(cap: Cap, target: &str) -> Result<Cap, SysError> {
    if !cap.rights().contains(CapRights::PROC_TRANSFER) {
        return Err(SysError::AccessDenied);
    }
    let pi = proc_slot(cap)?;
    let slot = current_slot();
    if slot == 0 {
        return Err(SysError::BadCap);
    }
    interrupts::without_interrupts(|| {
        let mut threads = THREADS.lock();
        let owner = running_proc_index(&threads, slot)?;
        let handle = threads[owner].procs[pi].ok_or(SysError::BadCap)?;
        if !handle.rights.contains(CapRights::PROC_TRANSFER) {
            return Err(SysError::AccessDenied);
        }
        let ci = handle.child_slot as usize;
        if ci == 0
            || ci > threads.len()
            || threads[ci - 1].cap_gen.load(Ordering::Acquire) != handle.gen
        {
            threads[owner].procs[pi] = None;
            return Err(SysError::BadCap);
        }
        // Attenuate: intersection of table rights and Cap word.
        let rights = handle.rights.intersection(cap.rights());
        if !rights.contains(CapRights::PROC_TRANSFER) {
            return Err(SysError::AccessDenied);
        }
        let target_index = threads
            .iter()
            .position(|t| {
                t.is_user && t.state.load(Ordering::Acquire) == STATE_RUNNING && t.name() == target
            })
            .and_then(|ti| proc_index(&threads, ti + 1));
        let Some(ti) = target_index else {
            return Err(SysError::NotFound);
        };
        if ti == owner {
            return Err(SysError::BadValue);
        }
        let Some(dest) = threads[ti].procs.iter().position(|s| s.is_none()) else {
            return Err(SysError::NoResource);
        };
        // Take from caller only after the target has a free slot.
        threads[owner].procs[pi] = None;
        threads[ti].procs[dest] = Some(ProcHandle {
            child_slot: handle.child_slot,
            gen: handle.gen,
            rights,
        });
        Ok(Cap::new(PROC_CAP_BASE + dest as u64, rights))
    })
}

/// Sets the file cursor on an archive or galfs open. `read` and galfs
/// `write` share it. Returns the new offset.
pub(crate) fn task_seek(cap: Cap, offset: i64, whence: u64) -> Result<u64, SysError> {
    let index = file_slot(cap)?;
    let slot = current_slot();
    if slot == 0 {
        return Err(SysError::BadCap);
    }
    interrupts::without_interrupts(|| {
        let mut threads = THREADS.lock();
        let owner = running_proc_index(&threads, slot)?;
        let thread = &mut threads[owner];
        let file = thread.files[index].as_mut().ok_or(SysError::BadCap)?;
        let effective = file.rights.intersection(cap.rights());
        if !effective.contains(CapRights::READ) && !effective.contains(CapRights::WRITE) {
            return Err(SysError::AccessDenied);
        }
        let len = match file.body {
            FileBody::Archive(bytes) => bytes.len(),
            FileBody::Galfs(obj) => {
                galfs::with_file(obj, |o| o.len as usize).ok_or(SysError::BadCap)?
            }
            FileBody::Pipe { .. } | FileBody::Channel { .. } => return Err(SysError::Unsupported),
        };
        let base = match whence {
            galexy_abi::SEEK_SET => 0i64,
            galexy_abi::SEEK_CUR => file.offset as i64,
            galexy_abi::SEEK_END => len as i64,
            _ => return Err(SysError::BadValue),
        };
        let Some(raw) = base.checked_add(offset) else {
            return Err(SysError::BadValue);
        };
        let new = if raw < 0 {
            0usize
        } else if raw as usize > len {
            len
        } else {
            raw as usize
        };
        file.offset = new;
        Ok(new as u64)
    })
}

pub(in crate::sched) fn admin_caller(
    fs_root: u16,
    _tokens: &[galfs::Token; galfs::TOKEN_SLOTS],
) -> bool {
    galfs::is_admin_root(fs_root)
}

/// Filesystem and account mutations stay closed until `passwd` clears the flag.
pub(in crate::sched) fn deny_must_change(thread: &Thread) -> Result<(), SysError> {
    if thread.must_change {
        Err(SysError::AccessDenied)
    } else {
        Ok(())
    }
}

/// Writes the current actor name into `out`.
pub(crate) fn task_whoami(out: &mut [u8]) -> Result<usize, SysError> {
    let slot = current_slot();
    if slot == 0 {
        return Err(SysError::BadCap);
    }
    interrupts::without_interrupts(|| {
        let threads = THREADS.lock();
        let thread = &threads[running_proc_index(&threads, slot)?];
        galfs::name_of_root(thread.fs_root, out)
    })
}

/// Writes every actor name, one per line, into `out`.
pub(crate) fn task_users(out: &mut [u8]) -> Result<usize, SysError> {
    let mut n = 0usize;
    let mut overflow = false;
    galfs::for_each_actor(|name| {
        if overflow {
            return;
        }
        let need = name.len() + 1;
        if n + need > out.len() {
            overflow = true;
            return;
        }
        out[n..n + name.len()].copy_from_slice(name);
        out[n + name.len()] = b'\n';
        n += need;
    });
    if overflow {
        return Err(SysError::BadBuffer);
    }
    Ok(n)
}

/// Writes the caller's galfs tokens into `out`.
pub(crate) fn task_tokens(out: &mut [u8]) -> Result<usize, SysError> {
    let slot = current_slot();
    if slot == 0 {
        return Err(SysError::BadCap);
    }
    interrupts::without_interrupts(|| {
        let threads = THREADS.lock();
        let thread = &threads[running_proc_index(&threads, slot)?];
        if thread.fs_root == galfs::NO_OBJECT {
            return Err(SysError::AccessDenied);
        }
        Ok(galfs::format_tokens(thread.fs_root, &thread.fs_tokens, out))
    })
}

/// Explicit galfs flush (`Syscall::Sync`).
pub(crate) fn task_sync() -> Result<(), SysError> {
    galfs::sync_explicit()
}

/// Writes a [`galexy_abi::QUOTA_LEN`] record for `name` (None = caller's actor).
pub(crate) fn task_quota(out: &mut [u8], name: Option<&str>) -> Result<usize, SysError> {
    let slot = current_slot();
    if slot == 0 {
        return Err(SysError::BadCap);
    }
    let (used_o, max_o, used_b, max_b) = interrupts::without_interrupts(|| {
        let threads = THREADS.lock();
        let thread = &threads[running_proc_index(&threads, slot)?];
        if thread.fs_root == galfs::NO_OBJECT {
            return Err(SysError::AccessDenied);
        }
        match name {
            Some(n) if !n.is_empty() => galfs::actor_quota(n),
            _ => galfs::root_quota(thread.fs_root),
        }
    })?;
    galfs::format_quota_record(used_o, max_o, used_b, max_b, out)
}

/// Sets durable quotas for `name`. Caller must be admin.
pub(crate) fn task_setquota(name: &str, max_objects: u16, max_bytes: u32) -> Result<(), SysError> {
    let slot = current_slot();
    if slot == 0 {
        return Err(SysError::BadCap);
    }
    interrupts::without_interrupts(|| {
        let threads = THREADS.lock();
        let thread = &threads[running_proc_index(&threads, slot)?];
        if !admin_caller(thread.fs_root, &thread.fs_tokens) {
            return Err(SysError::AccessDenied);
        }
        deny_must_change(thread)?;
        Ok(())
    })?;
    galfs::set_actor_quota(name, max_objects, max_bytes)
}

/// Creates an actor + Desktop with `password`. Caller must be admin.
pub(crate) fn task_useradd(name: &str, password: &[u8]) -> Result<(), SysError> {
    let slot = current_slot();
    if slot == 0 {
        return Err(SysError::BadCap);
    }
    let added = interrupts::without_interrupts(|| {
        let threads = THREADS.lock();
        let thread = &threads[running_proc_index(&threads, slot)?];
        if !admin_caller(thread.fs_root, &thread.fs_tokens) {
            return Err(SysError::AccessDenied);
        }
        deny_must_change(thread)?;
        let _ = galfs::add_user(name, password)?;
        Ok(())
    });
    match added {
        Ok(()) => {
            serial_println!("[auth] useradd user={}", name);
            galfs::mark_dirty();
            Ok(())
        }
        Err(err) => {
            serial_println!("[auth] useradd fail user={} err={}", name, err as u64);
            Err(err)
        }
    }
}

/// Password login: replace the caller's session with `ALL` on `name`'s root.
///
/// After `LOCKOUT_MAX_FAILS` misses on this actor or this TTY, further
/// attempts return [`SysError::Locked`] until the monotonic cool-down
/// elapses. That check runs before the password KDF.
pub(crate) fn task_login(name: &str, password: &[u8]) -> Result<(), SysError> {
    let slot = current_slot();
    if slot == 0 {
        return Err(SysError::BadCap);
    }
    let tty = current_tty();
    if lockout::blocked(name, tty) {
        lockout::note_refused(name, tty);
        return Err(SysError::Locked);
    }
    match galfs::verify_password(name, password) {
        Ok(true) => {}
        Ok(false) => {
            lockout::record_failure(name, tty, true);
            return Err(SysError::AccessDenied);
        }
        Err(SysError::NotFound) => {
            lockout::record_failure(name, tty, false);
            return Err(SysError::NotFound);
        }
        Err(err) => return Err(err),
    }
    let target = galfs::root_named(name)?;
    interrupts::without_interrupts(|| {
        let mut threads = THREADS.lock();
        let owner = running_proc_index(&threads, slot)?;
        let init_live = init_supervisor_live(&threads);
        let caller = &mut threads[owner];
        let gen = install_session(caller, target, true, init_live)?;
        let debug_id = caller.debug_id;
        serial_println!(
            "[auth] session login user={} gen={} tty={} id={}",
            name,
            gen,
            tty.saturating_add(1),
            debug_id
        );
        sync_session_to_threads(&mut threads, owner);
        Ok(())
    })?;
    lockout::record_success(name, tty);
    Ok(())
}

/// Clear the caller's session (logged out / pre-login).
///
/// The last live session also seals and wipes the volume key so the next
/// login screen prompts again.
pub(crate) fn task_logout() -> Result<(), SysError> {
    let slot = current_slot();
    if slot == 0 {
        return Err(SysError::BadCap);
    }
    let seal = interrupts::without_interrupts(|| {
        let mut threads = THREADS.lock();
        let owner = running_proc_index(&threads, slot)?;
        let caller = &mut threads[owner];
        let tty = caller.tty;
        let was_in = caller.fs_root != galfs::NO_OBJECT;
        let debug_id = caller.debug_id;
        let gen = clear_session(caller);
        serial_println!(
            "[auth] session logout gen={} tty={} id={}",
            gen,
            tty.saturating_add(1),
            debug_id
        );
        sync_session_to_threads(&mut threads, owner);
        Ok(was_in && !sessions_open(&threads))
    })?;
    if seal {
        galfs::seal_and_lock();
    }
    Ok(())
}

/// Freeze `caller` in place. Tokens and the actor stay. Returns false
/// when there is no session or the seat is already locked.
pub(in crate::sched) fn apply_lock(caller: &mut Thread) -> bool {
    if caller.fs_root == galfs::NO_OBJECT || caller.locked {
        return false;
    }
    caller.locked = true;
    caller.last_input_tick = 0;
    true
}

/// Freeze the caller's session (`lock`, or the idle scan).
pub(crate) fn task_lock() -> Result<(), SysError> {
    let slot = current_slot();
    if slot == 0 {
        return Err(SysError::BadCap);
    }
    interrupts::without_interrupts(|| {
        let mut threads = THREADS.lock();
        let owner = running_proc_index(&threads, slot)?;
        let tty = threads[owner].tty;
        let debug_id = threads[owner].debug_id;
        if threads[owner].fs_root == galfs::NO_OBJECT {
            return Err(SysError::AccessDenied);
        }
        if !apply_lock(&mut threads[owner]) {
            return Ok(());
        }
        let mut name_buf = [0u8; 32];
        let name_len = galfs::name_of_root(threads[owner].fs_root, &mut name_buf).unwrap_or(0);
        let name = core::str::from_utf8(&name_buf[..name_len]).unwrap_or("?");
        serial_println!(
            "[auth] session lock user={} tty={} id={}",
            name,
            tty.saturating_add(1),
            debug_id
        );
        sync_session_to_threads(&mut threads, owner);
        Ok(())
    })
}

/// `rax = 1` when the caller's session is locked.
pub(crate) fn task_locked() -> Result<u64, SysError> {
    let slot = current_slot();
    if slot == 0 {
        return Err(SysError::BadCap);
    }
    interrupts::without_interrupts(|| {
        let threads = THREADS.lock();
        let thread = &threads[running_proc_index(&threads, slot)?];
        if thread.fs_root == galfs::NO_OBJECT {
            return Err(SysError::AccessDenied);
        }
        Ok(u64::from(thread.locked))
    })
}

/// Resume a locked session after the actor's password checks out.
///
/// The KDF runs outside [`THREADS`], same as login. A cool-down returns
/// [`SysError::Locked`] before the KDF.
pub(crate) fn task_unlock_session(password: &[u8]) -> Result<(), SysError> {
    let slot = current_slot();
    if slot == 0 {
        return Err(SysError::BadCap);
    }
    let tty = current_tty();
    let mut name_buf = [0u8; 32];
    let (name_len, gen, debug_id) = interrupts::without_interrupts(|| {
        let threads = THREADS.lock();
        let thread = &threads[running_proc_index(&threads, slot)?];
        if thread.fs_root == galfs::NO_OBJECT || !thread.locked {
            return Err(SysError::AccessDenied);
        }
        let n = galfs::name_of_root(thread.fs_root, &mut name_buf)?;
        Ok((n, thread.session_gen, thread.debug_id))
    })?;
    let name = core::str::from_utf8(&name_buf[..name_len]).unwrap_or("");
    if lockout::blocked(name, tty) {
        lockout::note_refused(name, tty);
        return Err(SysError::Locked);
    }
    match galfs::verify_password(name, password) {
        Ok(true) => {}
        Ok(false) => {
            lockout::record_failure(name, tty, true);
            return Err(SysError::AccessDenied);
        }
        Err(SysError::NotFound) => {
            lockout::record_failure(name, tty, false);
            return Err(SysError::NotFound);
        }
        Err(err) => return Err(err),
    }
    interrupts::without_interrupts(|| {
        let mut threads = THREADS.lock();
        let owner = running_proc_index(&threads, slot)?;
        let caller = &mut threads[owner];
        if caller.session_gen != gen || !caller.locked || caller.fs_root == galfs::NO_OBJECT {
            return Err(SysError::AccessDenied);
        }
        caller.locked = false;
        caller.last_input_tick = crate::arch::timer_ticks();
        serial_println!(
            "[auth] session unlock user={} tty={} id={}",
            name,
            tty.saturating_add(1),
            debug_id
        );
        sync_session_to_threads(&mut threads, owner);
        Ok(())
    })?;
    lockout::record_success(name, tty);
    Ok(())
}

/// Install `ALL` on `target`, durable home shares, and grants for that actor.
///
/// `from_login` sets [`Thread::born_admin`] from the target (password
/// identity). `su` passes `false` so switching to a non-admin actor does
/// **not** clear born-admin — the seat can `su admin` to return (AUTH.md).
pub(in crate::sched) fn install_session(
    caller: &mut Thread,
    target: u16,
    from_login: bool,
    init_live: bool,
) -> Result<u64, SysError> {
    caller.fs_root = target;
    caller.fs_tokens = [galfs::Token::empty(); galfs::TOKEN_SLOTS];
    galfs::push_token(&mut caller.fs_tokens, target, galfs::RIGHT_ALL)?;
    galfs::apply_shares(target, &mut caller.fs_tokens)?;
    let admin = galfs::is_admin_root(target);
    if from_login {
        caller.born_admin = admin;
    }
    caller.must_change = galfs::actor_must_change(target);
    caller.locked = false;
    caller.last_input_tick = crate::arch::timer_ticks();
    let gen = NEXT_SESSION.fetch_add(1, Ordering::Relaxed);
    caller.session_gen = gen;
    // Power stays on the admin seat only when init is not the supervisor.
    // Kernel test launchers still use [`Grants::launcher`] directly.
    caller.grants = if admin && !init_live {
        Grants::launcher()
    } else {
        Grants::session()
    };
    Ok(gen)
}

/// True when the orphan-root init task is still running or parked.
pub(in crate::sched) fn init_supervisor_live(threads: &[Thread]) -> bool {
    threads.iter().any(|t| {
        if !t.is_init {
            return false;
        }
        let state = t.state.load(Ordering::Acquire);
        state == STATE_RUNNING || state == STATE_WAITING
    })
}

/// Durable home share for actor `grantee` (survives logout; reapplied at login).
pub(crate) fn task_share(path: &str, rights: u8, grantee: &str) -> Result<(), SysError> {
    let slot = current_slot();
    if slot == 0 {
        return Err(SysError::BadCap);
    }
    let (fs_root, fs_tokens) = interrupts::without_interrupts(|| {
        let threads = THREADS.lock();
        let caller = &threads[running_proc_index(&threads, slot)?];
        deny_must_change(caller)?;
        Ok((caller.fs_root, caller.fs_tokens))
    })?;
    galfs::add_share(fs_root, &fs_tokens, path, rights, grantee)
}

/// Clears durable share rights for actor `grantee`.
pub(crate) fn task_unshare(path: &str, rights: u8, grantee: &str) -> Result<(), SysError> {
    let slot = current_slot();
    if slot == 0 {
        return Err(SysError::BadCap);
    }
    let (fs_root, fs_tokens) = interrupts::without_interrupts(|| {
        let threads = THREADS.lock();
        let caller = &threads[running_proc_index(&threads, slot)?];
        deny_must_change(caller)?;
        Ok((caller.fs_root, caller.fs_tokens))
    })?;
    galfs::remove_share(fs_root, &fs_tokens, path, rights, grantee)
}

/// Another user task still holds a logged-in root.
pub(in crate::sched) fn sessions_open(threads: &[Thread]) -> bool {
    threads.iter().any(|t| {
        if !t.is_user || t.fs_root == galfs::NO_OBJECT {
            return false;
        }
        let state = t.state.load(Ordering::Acquire);
        state == STATE_RUNNING || state == STATE_WAITING
    })
}

/// Copies the session fields a thread record carries (grants, root,
/// born-admin, generation) from the leader at `owner` to every thread of
/// that process (Milestone 70), so a login, logout, or `su` from any
/// thread is what the dispatcher's grant checks see on all of them.
pub(in crate::sched) fn sync_session_to_threads(threads: &mut [Thread], owner: usize) {
    let leader_slot = (owner + 1) as u8;
    let cr3 = threads[owner].cr3.load(Ordering::Acquire);
    let grants = threads[owner].grants;
    let fs_root = threads[owner].fs_root;
    let born_admin = threads[owner].born_admin;
    let session_gen = threads[owner].session_gen;
    let locked = threads[owner].locked;
    for t in threads.iter_mut() {
        if t.leader_slot != leader_slot || t.cr3.load(Ordering::Acquire) != cr3 {
            continue;
        }
        t.grants = grants;
        t.fs_root = fs_root;
        t.born_admin = born_admin;
        t.session_gen = session_gen;
        t.locked = locked;
    }
}

pub(in crate::sched) fn clear_session(caller: &mut Thread) -> u64 {
    caller.fs_root = galfs::NO_OBJECT;
    caller.fs_tokens = [galfs::Token::empty(); galfs::TOKEN_SLOTS];
    caller.born_admin = false;
    caller.must_change = false;
    caller.locked = false;
    caller.last_input_tick = 0;
    let gen = NEXT_SESSION.fetch_add(1, Ordering::Relaxed);
    caller.session_gen = gen;
    caller.grants = Grants::pre_login();
    gen
}

/// Sets a password. Admin may set any account; others only their own.
pub(crate) fn task_passwd(name: Option<&str>, password: &[u8]) -> Result<(), SysError> {
    let slot = current_slot();
    if slot == 0 {
        return Err(SysError::BadCap);
    }
    let mut name_buf = [0u8; 32];
    // Resolve the target and whether it is the caller's own account under
    // one THREADS hold. The caller's root is fixed for the life of the
    // session, so the flag read here is still valid after the KDF.
    let (name_len, own_account, fs_root) = interrupts::without_interrupts(|| {
        let threads = THREADS.lock();
        let thread = &threads[running_proc_index(&threads, slot)?];
        if thread.fs_root == galfs::NO_OBJECT {
            return Err(SysError::AccessDenied);
        }
        let is_admin = admin_caller(thread.fs_root, &thread.fs_tokens);
        let mut self_name = [0u8; 32];
        let self_len = galfs::name_of_root(thread.fs_root, &mut self_name)?;
        if let Some(n) = name {
            let own = &self_name[..self_len] == n.as_bytes();
            if !is_admin && !own {
                return Err(SysError::AccessDenied);
            }
            if n.len() > name_buf.len() {
                return Err(SysError::BadValue);
            }
            name_buf[..n.len()].copy_from_slice(n.as_bytes());
            Ok((n.len(), own, thread.fs_root))
        } else {
            name_buf[..self_len].copy_from_slice(&self_name[..self_len]);
            Ok((self_len, true, thread.fs_root))
        }
    })?;
    let name_str = core::str::from_utf8(&name_buf[..name_len]).map_err(|_| SysError::BadValue)?;
    if let Err(err) = galfs::set_password(name_str, password) {
        serial_println!("[auth] passwd fail user={} err={}", name_str, err as u64);
        return Err(err);
    }
    serial_println!("[auth] passwd user={}", name_str);
    if own_account {
        // Clearing the gate re-reads galfs (TABLE) BEFORE taking THREADS:
        // one lock at a time on the IF=0 syscall path.
        let must_change = galfs::actor_must_change(fs_root);
        interrupts::without_interrupts(|| {
            let mut threads = THREADS.lock();
            if let Some(owner) = proc_index(&threads, slot) {
                threads[owner].must_change = must_change;
            }
        });
    }
    Ok(())
}

/// Deletes an empty actor. Refuses admin and roots still in use.
///
/// Also refuses when any task still holds an open galfs cap on that
/// actor's objects. Tokens naming those objects are cleared on success.
pub(crate) fn task_userdel(name: &str) -> Result<(), SysError> {
    let slot = current_slot();
    if slot == 0 {
        return Err(SysError::BadCap);
    }
    let removed = interrupts::without_interrupts(|| {
        let mut threads = THREADS.lock();
        let thread = &threads[running_proc_index(&threads, slot)?];
        if !admin_caller(thread.fs_root, &thread.fs_tokens) {
            return Err(SysError::AccessDenied);
        }
        deny_must_change(thread)?;
        let root = galfs::root_named(name)?;
        let live = threads.iter().any(|t| {
            t.is_user && t.fs_root == root && {
                let s = t.state.load(Ordering::Acquire);
                s == STATE_RUNNING || s == STATE_WAITING
            }
        });
        if live {
            return Err(SysError::Unsupported);
        }
        let mut objs = [galfs::NO_OBJECT; 8];
        let n = galfs::collect_actor_objects(root, &mut objs);
        let objs = &objs[..n];
        for thread in threads.iter() {
            for open in &thread.files {
                let Some(file) = open else { continue };
                if let FileBody::Galfs(obj) = file.body {
                    if objs.contains(&obj) {
                        return Err(SysError::Unsupported);
                    }
                }
            }
        }
        galfs::remove_user(name)?;
        for thread in threads.iter_mut() {
            galfs::drop_tokens_on(&mut thread.fs_tokens, objs);
        }
        Ok(())
    });
    match removed {
        Ok(()) => {
            serial_println!("[auth] userdel user={}", name);
            lockout::clear_actor(name);
            galfs::mark_dirty();
            Ok(())
        }
        Err(err) => {
            serial_println!("[auth] userdel fail user={} err={}", name, err as u64);
            Err(err)
        }
    }
}

/// Card-based identity switch: replace tokens with `ALL` on `name`'s root.
///
/// Allowed for an admin session, a born-admin seat returning to admin, or
/// a holder of `ALL` on the target root (access card). Password login is
/// [`task_login`].
pub(crate) fn task_su(name: &str) -> Result<(), SysError> {
    let slot = current_slot();
    if slot == 0 {
        return Err(SysError::BadCap);
    }
    interrupts::without_interrupts(|| {
        let mut threads = THREADS.lock();
        let owner = running_proc_index(&threads, slot)?;
        let (fs_root, fs_tokens, born_admin) = {
            let caller = &threads[owner];
            deny_must_change(caller)?;
            (caller.fs_root, caller.fs_tokens, caller.born_admin)
        };
        let target = galfs::root_named(name)?;
        let to_admin = galfs::is_admin_root(target);
        let operator = galfs::is_admin_root(fs_root) || (born_admin && to_admin);
        let allowed = operator || galfs::holds_all(fs_root, &fs_tokens, target);
        if !allowed {
            return Err(SysError::AccessDenied);
        }
        if !operator {
            let mut cards = fs_tokens;
            if galfs::consume_once(&mut cards, target) {
                serial_println!("[auth] card once user={} revoked", name);
            }
        }
        let init_live = init_supervisor_live(&threads);
        let caller = &mut threads[owner];
        let tty = caller.tty;
        let gen = install_session(caller, target, false, init_live)?;
        let debug_id = caller.debug_id;
        serial_println!(
            "[auth] session su user={} gen={} tty={} id={}",
            name,
            gen,
            tty.saturating_add(1),
            debug_id
        );
        sync_session_to_threads(&mut threads, owner);
        Ok(())
    })
}

/// Records a keystroke on `tty` so idle lock starts from now.
pub fn note_tty_input(tty: u8) {
    let now = crate::arch::timer_ticks();
    interrupts::without_interrupts(|| {
        let mut threads = THREADS.lock();
        for thread in threads.iter_mut() {
            if thread.is_user && thread.tty == tty {
                thread.last_input_tick = now;
            }
        }
    });
}

/// Logged-in console shells with no keys for [`IDLE_LOGOUT_MS`] are
/// locked. The actor and tokens stay; the password opens the same seat.
/// A keyboard read in progress returns [`SysError::Locked`] so the shell
/// can draw the lock screen. A Cap-wait is left alone.
///
/// A shell Cap-waiting on a foreground child pauses that clock until the
/// child exits, or until [`FOREGROUND_IDLE_MS`] with no keys. Test kernels
/// pass `all_tasks` so a blob can trip the same path.
pub(in crate::sched) fn idle_scan(all_tasks: bool) {
    let now = crate::arch::timer_ticks();
    interrupts::without_interrupts(|| {
        let mut threads = THREADS.lock();
        let n = threads.len();
        for i in 0..n {
            if !threads[i].is_user || threads[i].locked {
                continue;
            }
            let state = threads[i].state.load(Ordering::Acquire);
            if state != STATE_RUNNING && state != STATE_WAITING {
                continue;
            }
            if threads[i].fs_root == galfs::NO_OBJECT || threads[i].is_init {
                continue;
            }
            if !all_tasks && !is_console_shell_name(threads[i].name()) {
                continue;
            }
            // A foreground child (`rustc`, `nano`) keeps the seat, up to
            // [`FOREGROUND_IDLE_MS`] after the last key. The clock
            // restarts when the Cap-wait ends, so the prompt is not
            // already inside the 5 min window. Keys during the wait
            // (nano) move `last_input_tick` and extend the pause.
            let holding =
                state == STATE_WAITING && threads[i].wait_for_exit.load(Ordering::Acquire);
            if holding {
                if !foreground_idle_due(threads[i].last_input_tick, now) {
                    threads[i].idle_hold = true;
                    continue;
                }
            } else if threads[i].idle_hold {
                threads[i].idle_hold = false;
                threads[i].last_input_tick = now;
            }
            let last = threads[i].last_input_tick;
            if !idle_due(last, now) {
                continue;
            }
            let Some(owner) = proc_index(&threads, i + 1) else {
                continue;
            };
            if !apply_lock(&mut threads[owner]) {
                continue;
            }
            let tty = threads[i].tty;
            let mut name_buf = [0u8; 32];
            let name_len = galfs::name_of_root(threads[owner].fs_root, &mut name_buf).unwrap_or(0);
            let name = core::str::from_utf8(&name_buf[..name_len]).unwrap_or("?");
            serial_println!(
                "[auth] idle lock user={} tty={}",
                name,
                tty.saturating_add(1)
            );
            sync_session_to_threads(&mut threads, owner);
            // The prompt is a keyboard read. Hand it `Locked` so the shell
            // draws the lock screen instead of taking the next key as a
            // command. A Cap-wait (and a sleep) stays parked.
            if state == STATE_WAITING && threads[i].io_kind.load(Ordering::Acquire) == IO_KEYBOARD {
                clear_wait_fields(&threads[i]);
                stamp_waiter_frame(&threads[i], SyscallResult::err(SysError::Locked));
                set_running(&threads[i]);
            }
        }
    });
}

/// Main-loop idle lock for F-key shells.
pub fn poll_idle_locks() {
    idle_scan(false);
}

/// Same scan, including non-shell tasks. Test kernels only.
pub fn test_poll_idle_all() {
    idle_scan(true);
}

/// True when a live user task named `name` has a locked session.
pub fn test_session_locked(name: &str) -> bool {
    interrupts::without_interrupts(|| {
        let threads = THREADS.lock();
        threads.iter().any(|t| {
            let state = t.state.load(Ordering::Acquire);
            t.is_user
                && t.name() == name
                && t.locked
                && (state == STATE_RUNNING || state == STATE_WAITING)
        })
    })
}

/// Moves `name`'s last keystroke `ticks` into the past (test tick injection).
pub fn test_backdate_input(name: &str, ticks: u64) {
    let now = crate::arch::timer_ticks();
    interrupts::without_interrupts(|| {
        let mut threads = THREADS.lock();
        if let Some(thread) = threads.iter_mut().find(|t| t.is_user && t.name() == name) {
            thread.last_input_tick = now.saturating_sub(ticks);
        }
    });
}

/// `(session_gen, must_change)` for a live user task named `name`.
pub fn test_auth_flags(name: &str) -> Option<(u64, bool)> {
    interrupts::without_interrupts(|| {
        let threads = THREADS.lock();
        threads.iter().find_map(|t| {
            let state = t.state.load(Ordering::Acquire);
            if t.is_user && t.name() == name && (state == STATE_RUNNING || state == STATE_WAITING) {
                Some((t.session_gen, t.must_change))
            } else {
                None
            }
        })
    })
}

pub(in crate::sched) fn idle_limit() -> u64 {
    let over = IDLE_LIMIT_OVERRIDE.load(Ordering::Relaxed);
    if over == 0 {
        IDLE_LOGOUT_MS
    } else {
        over
    }
}

/// Shrinks the idle window for tests. `0` restores [`IDLE_LOGOUT_MS`].
pub fn test_set_idle_limit(ms: u64) {
    IDLE_LIMIT_OVERRIDE.store(ms, Ordering::Relaxed);
}

/// True when `last` is set and `now` is at least the idle limit later.
pub fn idle_due(last: u64, now: u64) -> bool {
    last != 0 && now.saturating_sub(last) >= idle_limit()
}

/// True when a Cap-waited child has had no keys for [`FOREGROUND_IDLE_MS`].
///
/// This cap does not follow [`test_set_idle_limit`]. The prompt window
/// shrinks in tests; a foreground job does not.
fn foreground_idle_due(last: u64, now: u64) -> bool {
    last != 0 && now.saturating_sub(last) >= FOREGROUND_IDLE_MS
}

/// Spawns `bytes` paused, installs galfs objects as file caps in order,
/// then lets the child run.
///
/// The child never opens a path. `caps` is `(object index, rights)` and
/// lands at [`galexy_abi::FILE_CAP_BASE`] + slot. Milestone 71 ring-3
/// `gxld` receives its inputs and its output this way.
pub fn spawn_with_given_files(
    name: &str,
    bytes: &[u8],
    caps: &[(u16, CapRights)],
) -> Result<u8, SysError> {
    if caps.len() > MAX_OPEN_FILES {
        return Err(SysError::NoResource);
    }
    let slot = loader::spawn_launched(
        name,
        bytes,
        Grants::console(),
        b"",
        0,
        galfs::admin_cred(),
        0,
        true,
    )?;
    interrupts::without_interrupts(|| {
        let mut threads = THREADS.lock();
        let i = slot as usize - 1;
        for (n, (obj, rights)) in caps.iter().enumerate() {
            threads[i].files[n] = Some(OpenFile {
                body: FileBody::Galfs(*obj),
                offset: 0,
                rights: *rights,
            });
        }
        release_deferred(&mut threads, slot);
    });
    Ok(slot)
}
