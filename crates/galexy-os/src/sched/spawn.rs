//! Queued spawn, shells, process Caps, and login sessions.
//!
//! The syscall copies a [`PendingSpawn`] and returns; the main loop
//! drains it. Wait, kill, grant, and the auth syscalls sit with that
//! path because they update the same parent and session fields.

use super::iowait::*;
use super::task::*;
use super::thread::*;

use super::{context, galfs, loader, lockout, pipe, ramdisk};
use crate::serial_println;
use crate::sync::Mutex;
use core::sync::atomic::{AtomicU32, AtomicU8, Ordering};
use galexy_abi::{Cap, CapRights, SysError, SyscallResult, PROC_CAP_BASE};
use x86_64::instructions::interrupts;

/// Argument bytes copied onto a new task's stack. Matches the syscall cap.
pub(crate) const ARG_MAX: usize = 256;

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
        {
            let thread = threads.get(slot - 1).ok_or(SysError::BadCap)?;
            if !thread.is_user || thread.state.load(Ordering::Acquire) != STATE_RUNNING {
                return Err(SysError::BadCap);
            }
            // F-key console names: only init may spawn seats (Milestone 54).
            let seat = is_console_shell_name(name);
            if seat && !thread.is_init {
                return Err(SysError::Unsupported);
            }
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
        let parent_must = threads[slot - 1].must_change;
        let mut parent_fs = galfs::FsCred {
            root: threads[slot - 1].fs_root,
            tokens: threads[slot - 1].fs_tokens,
        };
        // Pre-login seats launch nothing: a task without a session root
        // (logged out, or never logged in) may only be init spawning a
        // seat. The shell refuses earlier; this is the kernel's answer.
        if !seat && parent_fs.root == galfs::NO_OBJECT {
            return Err(SysError::AccessDenied);
        }
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

/// Loads a queued program, if the shell has asked for one.
///
/// Runs from the main loop: that context is the kernel page table, which
/// `spawn_program` clones. Installs a process Cap on the waiter; without
/// `wait_exit` wakes with Cap bits in `rax`, with `wait_exit` parks until
/// the child exits (exit code in `rax`).
pub fn drain_spawn() {
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
    let grants = if seat {
        Grants::pre_login()
    } else if keyboard {
        Grants::console_keyboard(query)
    } else if query {
        Grants::console_query()
    } else {
        Grants::console()
    };
    // Seats share the `shell` ELF under twelve reserved names.
    let elf_name = if seat { "shell" } else { name };
    // Always defer the first instruction. File Caps have to land first,
    // and a `SPAWN_WAIT` parent must be linked (`wait_child_slot`) before
    // the child can exit. Publishing early lets the other CPU run a short
    // program to completion in the gap; the exit then wakes nobody and
    // the parent parks forever (`test-soak`'s `hello`).
    let child_slot = if let Some(bytes) = ramdisk::find(elf_name) {
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
    let wi = waiter_slot as usize - 1;
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
    for thread in threads.iter_mut() {
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
            if pi < MAX_PROC_CAPS {
                if let Some(handle) = thread.procs[pi] {
                    if handle.child_slot == child_slot {
                        thread.procs[pi] = None;
                    }
                }
            }
        }
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
        let Some(thread) = threads.get(slot - 1) else {
            return;
        };
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
        {
            let thread = threads.get(slot - 1).ok_or(SysError::BadCap)?;
            if !thread.is_user || thread.state.load(Ordering::Acquire) != STATE_RUNNING {
                return Err(SysError::BadCap);
            }
        }
        let handle = threads[slot - 1].procs[pi].ok_or(SysError::BadCap)?;
        if !handle.rights.contains(CapRights::PROC_WAIT) {
            return Err(SysError::AccessDenied);
        }
        let ci = handle.child_slot as usize;
        if ci == 0 || ci > threads.len() {
            threads[slot - 1].procs[pi] = None;
            return Err(SysError::BadCap);
        }
        let gen = threads[ci - 1].cap_gen.load(Ordering::Acquire);
        if gen != handle.gen {
            threads[slot - 1].procs[pi] = None;
            return Err(SysError::BadCap);
        }
        let state = threads[ci - 1].state.load(Ordering::Acquire);
        if state == STATE_EXITED || state == STATE_FREED {
            let code = threads[ci - 1].exit_code.load(Ordering::Acquire);
            threads[ci - 1].exit_waited.store(true, Ordering::Release);
            threads[slot - 1].procs[pi] = None;
            return Ok(Some(code));
        }
        if state != STATE_RUNNING && state != STATE_WAITING {
            threads[slot - 1].procs[pi] = None;
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
            thread.procs[pi].ok_or(SysError::BadCap)?
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
            let caller = threads.get(slot - 1).ok_or(SysError::BadCap)?;
            if !caller.is_user || caller.state.load(Ordering::Acquire) != STATE_RUNNING {
                return Err(SysError::BadCap);
            }
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
            let caller = threads.get(slot - 1).ok_or(SysError::BadCap)?;
            if !caller.is_user || caller.state.load(Ordering::Acquire) != STATE_RUNNING {
                return Err(SysError::BadCap);
            }
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
    let parent = waiter_slot as usize - 1;
    let child = child_slot as usize - 1;
    if parent >= threads.len() || child >= threads.len() || parent == child {
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
        let file = {
            let caller = threads.get_mut(slot - 1).ok_or(SysError::BadCap)?;
            if !caller.is_user || caller.state.load(Ordering::Acquire) != STATE_RUNNING {
                return Err(SysError::BadCap);
            }
            caller.files[index].take().ok_or(SysError::BadCap)?
        };
        let Some(ti) = threads.iter().position(|t| {
            t.is_user && t.state.load(Ordering::Acquire) == STATE_RUNNING && t.name() == target
        }) else {
            // Put it back — target missing.
            if let Some(caller) = threads.get_mut(slot - 1) {
                caller.files[index] = Some(file);
            } else {
                release_file(file);
            }
            return Err(SysError::NotFound);
        };
        if ti == slot - 1 {
            threads[ti].files[index] = Some(file);
            return Err(SysError::BadValue);
        }
        let Some(dest) = threads[ti].files.iter().position(|s| s.is_none()) else {
            threads[slot - 1].files[index] = Some(file);
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
        let handle = {
            let caller = threads.get_mut(slot - 1).ok_or(SysError::BadCap)?;
            if !caller.is_user || caller.state.load(Ordering::Acquire) != STATE_RUNNING {
                return Err(SysError::BadCap);
            }
            caller.procs[pi].ok_or(SysError::BadCap)?
        };
        if !handle.rights.contains(CapRights::PROC_TRANSFER) {
            return Err(SysError::AccessDenied);
        }
        let ci = handle.child_slot as usize;
        if ci == 0
            || ci > threads.len()
            || threads[ci - 1].cap_gen.load(Ordering::Acquire) != handle.gen
        {
            threads[slot - 1].procs[pi] = None;
            return Err(SysError::BadCap);
        }
        // Attenuate: intersection of table rights and Cap word.
        let rights = handle.rights.intersection(cap.rights());
        if !rights.contains(CapRights::PROC_TRANSFER) {
            return Err(SysError::AccessDenied);
        }
        let Some(ti) = threads.iter().position(|t| {
            t.is_user && t.state.load(Ordering::Acquire) == STATE_RUNNING && t.name() == target
        }) else {
            return Err(SysError::NotFound);
        };
        if ti == slot - 1 {
            return Err(SysError::BadValue);
        }
        let Some(dest) = threads[ti].procs.iter().position(|s| s.is_none()) else {
            return Err(SysError::NoResource);
        };
        // Take from caller only after the target has a free slot.
        threads[slot - 1].procs[pi] = None;
        threads[ti].procs[dest] = Some(ProcHandle {
            child_slot: handle.child_slot,
            gen: handle.gen,
            rights,
        });
        Ok(Cap::new(PROC_CAP_BASE + dest as u64, rights))
    })
}

/// Sets the read cursor on an archive or galfs open. Returns the new offset.
pub(crate) fn task_seek(cap: Cap, offset: i64, whence: u64) -> Result<u64, SysError> {
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
        let thread = threads.get(slot - 1).ok_or(SysError::BadCap)?;
        if !thread.is_user || thread.state.load(Ordering::Acquire) != STATE_RUNNING {
            return Err(SysError::BadCap);
        }
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
        let thread = threads.get(slot - 1).ok_or(SysError::BadCap)?;
        if !thread.is_user || thread.state.load(Ordering::Acquire) != STATE_RUNNING {
            return Err(SysError::BadCap);
        }
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
        let thread = threads.get(slot - 1).ok_or(SysError::BadCap)?;
        if !thread.is_user || thread.state.load(Ordering::Acquire) != STATE_RUNNING {
            return Err(SysError::BadCap);
        }
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
        let thread = threads.get(slot - 1).ok_or(SysError::BadCap)?;
        if !thread.is_user || thread.state.load(Ordering::Acquire) != STATE_RUNNING {
            return Err(SysError::BadCap);
        }
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
        let thread = threads.get(slot - 1).ok_or(SysError::BadCap)?;
        if !thread.is_user || thread.state.load(Ordering::Acquire) != STATE_RUNNING {
            return Err(SysError::BadCap);
        }
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
        {
            let caller = threads.get(slot - 1).ok_or(SysError::BadCap)?;
            if !caller.is_user || caller.state.load(Ordering::Acquire) != STATE_RUNNING {
                return Err(SysError::BadCap);
            }
        }
        let init_live = init_supervisor_live(&threads);
        let caller = threads.get_mut(slot - 1).ok_or(SysError::BadCap)?;
        let gen = install_session(caller, target, true, init_live)?;
        let debug_id = caller.debug_id;
        serial_println!(
            "[auth] session login user={} gen={} tty={} id={}",
            name,
            gen,
            tty.saturating_add(1),
            debug_id
        );
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
        let caller = threads.get_mut(slot - 1).ok_or(SysError::BadCap)?;
        if !caller.is_user || caller.state.load(Ordering::Acquire) != STATE_RUNNING {
            return Err(SysError::BadCap);
        }
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
        Ok(was_in && !sessions_open(&threads))
    })?;
    if seal {
        galfs::seal_and_lock();
    }
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
        let caller = threads.get(slot - 1).ok_or(SysError::BadCap)?;
        if !caller.is_user || caller.state.load(Ordering::Acquire) != STATE_RUNNING {
            return Err(SysError::BadCap);
        }
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
        let caller = threads.get(slot - 1).ok_or(SysError::BadCap)?;
        if !caller.is_user || caller.state.load(Ordering::Acquire) != STATE_RUNNING {
            return Err(SysError::BadCap);
        }
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

pub(in crate::sched) fn clear_session(caller: &mut Thread) -> u64 {
    caller.fs_root = galfs::NO_OBJECT;
    caller.fs_tokens = [galfs::Token::empty(); galfs::TOKEN_SLOTS];
    caller.born_admin = false;
    caller.must_change = false;
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
        let thread = threads.get(slot - 1).ok_or(SysError::BadCap)?;
        if !thread.is_user || thread.state.load(Ordering::Acquire) != STATE_RUNNING {
            return Err(SysError::BadCap);
        }
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
            if let Some(thread) = threads.get_mut(slot - 1) {
                thread.must_change = must_change;
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
        let thread = threads.get(slot - 1).ok_or(SysError::BadCap)?;
        if !thread.is_user || thread.state.load(Ordering::Acquire) != STATE_RUNNING {
            return Err(SysError::BadCap);
        }
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
        let (fs_root, fs_tokens, born_admin) = {
            let caller = threads.get(slot - 1).ok_or(SysError::BadCap)?;
            if !caller.is_user || caller.state.load(Ordering::Acquire) != STATE_RUNNING {
                return Err(SysError::BadCap);
            }
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
        let caller = threads.get_mut(slot - 1).ok_or(SysError::BadCap)?;
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
        Ok(())
    })
}

/// Records a keystroke on `tty` so idle logout starts from now.
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

/// Logged-in console shells with no keys for [`IDLE_LOGOUT_MS`] are exited
/// (init respawns a logged-out seat). Test kernels pass `all_tasks` so a
/// blob can trip the same path.
pub(in crate::sched) fn idle_scan(all_tasks: bool) {
    let now = crate::arch::timer_ticks();
    let seal = interrupts::without_interrupts(|| {
        let mut threads = THREADS.lock();
        let n = threads.len();
        let mut cleared = false;
        for i in 0..n {
            if !threads[i].is_user {
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
            let last = threads[i].last_input_tick;
            if !idle_due(last, now) {
                continue;
            }
            let tty = threads[i].tty;
            let mut raw = [0u8; NAME_CAP];
            let name_len = threads[i].name_len as usize;
            raw[..name_len].copy_from_slice(&threads[i].name_bytes[..name_len]);
            let name = core::str::from_utf8(&raw[..name_len]).unwrap_or("?");
            serial_println!(
                "[auth] idle logout user={} tty={}",
                name,
                tty.saturating_add(1)
            );
            let _ = clear_session(&mut threads[i]);
            cleared = true;
            if state == STATE_WAITING {
                interrupt_io_waiter(&mut threads, i);
            }
            threads[i].exit_code.store(EXIT_KILLED, Ordering::Release);
            threads[i].state.store(STATE_EXITED, Ordering::Release);
        }
        cleared && !sessions_open(&threads)
    });
    if seal {
        galfs::seal_and_lock();
    }
}

/// Main-loop idle logout for F-key shells.
pub fn poll_idle_logouts() {
    idle_scan(false);
}

/// Same scan, including non-shell tasks. Test kernels only.
pub fn test_poll_idle_all() {
    idle_scan(true);
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
