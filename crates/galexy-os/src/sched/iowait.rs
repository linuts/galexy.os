//! Timer handoff, parked I/O, and the per-tick console budget.
//!
//! A console write commits only a prefix that ends outside an escape
//! sequence, so a budget cut cannot tear CSI on the shared COM1 line.

use super::spawn::*;
use super::task::*;
use super::thread::*;

use super::{channel, context, pipe};
use crate::serial_println;
use core::sync::atomic::{AtomicBool, Ordering};
use galexy_abi::{Cap, SysError, SyscallResult, PROC_CAP_BASE};
use x86_64::instructions::interrupts;
use x86_64::structures::paging::{Mapper, PhysFrame};
use x86_64::{PhysAddr, VirtAddr};

/// Console bytes a task may emit per timer tick before further writes
/// return a short success (0). Stops a tight loop from pinning COM1.
pub(in crate::sched) const CONSOLE_BUDGET_PER_TICK: u32 = 512;

/// Bytes the current task may still write to the console on this tick.
///
/// Does not charge the budget. [`console_take_budget`] spends what a write
/// actually commits.
pub(crate) fn console_budget_room() -> usize {
    let slot = current_slot();
    if slot == 0 {
        return usize::MAX;
    }
    let tick = crate::arch::timer_ticks();
    interrupts::without_interrupts(|| {
        let threads = THREADS.lock();
        let Some(thread) = threads.get(slot - 1) else {
            return usize::MAX;
        };
        if thread.console_budget_tick != tick {
            return CONSOLE_BUDGET_PER_TICK as usize;
        }
        CONSOLE_BUDGET_PER_TICK.saturating_sub(thread.console_budget_used) as usize
    })
}

/// Takes up to `want` bytes from the current task's console budget.
pub(crate) fn console_take_budget(want: usize) -> usize {
    let slot = current_slot();
    if slot == 0 || want == 0 {
        return want;
    }
    let tick = crate::arch::timer_ticks();
    interrupts::without_interrupts(|| {
        let mut threads = THREADS.lock();
        let Some(thread) = threads.get_mut(slot - 1) else {
            return want;
        };
        if thread.console_budget_tick != tick {
            thread.console_budget_tick = tick;
            thread.console_budget_used = 0;
        }
        let room = CONSOLE_BUDGET_PER_TICK.saturating_sub(thread.console_budget_used) as usize;
        let n = want.min(room);
        thread.console_budget_used = thread.console_budget_used.saturating_add(n as u32);
        n
    })
}

/// File-table index for a user cap, or `BadCap` when it is not a file index.
pub(in crate::sched) fn file_slot(cap: Cap) -> Result<usize, SysError> {
    let index = cap.index();
    if index < galexy_abi::FILE_CAP_BASE {
        return Err(SysError::BadCap);
    }
    let raw = index - galexy_abi::FILE_CAP_BASE;
    if raw >= MAX_OPEN_FILES as u64 {
        return Err(SysError::BadCap);
    }
    let slot = crate::arch::cpu::spectre_mask(raw, MAX_OPEN_FILES as u64) as usize;
    Ok(slot)
}

/// Process-Cap table index, or `BadCap` when it is not a process index.
pub(in crate::sched) fn proc_slot(cap: Cap) -> Result<usize, SysError> {
    let index = cap.index();
    if index < PROC_CAP_BASE {
        return Err(SysError::BadCap);
    }
    let raw = index - PROC_CAP_BASE;
    if raw >= MAX_PROC_CAPS as u64 {
        return Err(SysError::BadCap);
    }
    let slot = crate::arch::cpu::spectre_mask(raw, MAX_PROC_CAPS as u64) as usize;
    Ok(slot)
}

/// The current rotation slot (0 = main loop; otherwise thread index + 1).
pub fn current_slot() -> usize {
    cpu_sched().current.load(Ordering::Relaxed)
}

/// Copies the faulting task's name into `out`.
///
/// Uses `try_lock` so a fault that already holds [`THREADS`] still
/// reports. The main loop is `"main"`.
pub fn fault_task_name(out: &mut [u8]) -> &str {
    let slot = current_slot();
    if slot == 0 {
        return "main";
    }
    let Some(threads) = THREADS.try_lock() else {
        return "busy";
    };
    let Some(thread) = threads.get(slot - 1) else {
        return "?";
    };
    let n = (thread.name_len as usize).min(out.len());
    if n == 0 {
        return "?";
    }
    out[..n].copy_from_slice(&thread.name_bytes[..n]);
    drop(threads);
    core::str::from_utf8(&out[..n]).unwrap_or("?")
}

/// True when the running task is userspace init.
///
/// Init shares TTY 0 with the F1 seat. Its console writes go to the
/// serial log only, so a supervisor line cannot land in the login prompt.
pub fn current_is_init() -> bool {
    let slot = current_slot();
    if slot == 0 {
        return false;
    }
    interrupts::without_interrupts(|| {
        THREADS
            .lock()
            .get(slot - 1)
            .is_some_and(|thread| thread.is_init)
    })
}

/// Console the running task writes. The main loop is TTY 0.
///
/// The lock is dropped before return, so the caller can take the screen
/// lock afterwards.
pub fn current_tty() -> u8 {
    let slot = current_slot();
    if slot == 0 {
        return 0;
    }
    interrupts::without_interrupts(|| {
        THREADS
            .lock()
            .get(slot - 1)
            .map(|thread| thread.tty)
            .unwrap_or(0)
    })
}

/// Whether the current task was granted `grant` at spawn.
///
/// The lock is dropped before return, so the caller can take another lock
/// afterwards. A kernel slot (0) holds nothing.
pub(crate) fn task_granted(grant: Grant) -> bool {
    let slot = current_slot();
    if slot == 0 {
        return false;
    }
    interrupts::without_interrupts(|| {
        let threads = THREADS.lock();
        threads
            .get(slot - 1)
            .is_some_and(|thread| thread.grants.allows(grant))
    })
}

/// Is the CURRENT slot a ring-3 task? (`slot` per [`current_slot`].)
pub fn slot_is_user(slot: usize) -> bool {
    if slot == 0 {
        return false;
    }
    interrupts::without_interrupts(|| {
        let threads = THREADS.lock();
        slot <= threads.len() && threads[slot - 1].is_user
    })
}

/// Syscall-side context handoff (called from `arch`'s syscall dispatch on
/// the syscalling task's kernel stack, while its uniform frame is at `rsp`).
///
/// - `exit=false` (yield): the task stays schedulable, hand over the CPU
///   now — the real round-robin switch from inside a syscall.
/// - `exit=true` (exit): tombstone the task; the reaper frees its stacks.
///
/// Returns the context pointer to enter (guarantees a switch: main is
/// always the fallback — the caller is never main). The incoming RSP0 /
/// kstack registry are updated like the timer switch does. NOT irq-gated on
/// entry (the naked syscall entry runs with IF=0); the internal gate keeps
/// the lock-audit rule.
///
/// # Safety
///
/// `frame` must be the CURRENT task's uniform context frame on its kernel
/// stack, exactly as built by the syscall entry.
pub unsafe fn syscall_handoff(
    frame: *mut context::Context,
    exit: bool,
    reason: &'static str,
) -> u64 {
    crate::arch::cpu::note_switch();
    // No-switch paths (there are none once we return) must not leave a
    // stale departed slot for the naked tail to publish.
    crate::arch::cpu::set_departed_slot(0);
    let me = cpu_sched();
    let slot = me.current.load(Ordering::Relaxed);
    assert!(slot != 0, "syscall_handoff: no task current (cpl bug?)");
    let pending_ctx = interrupts::without_interrupts(|| {
        let mut threads = THREADS.lock();

        // Save the outgoing task's context + FPU state into its slot
        // (yield keeps it schedulable; exit tombstones it).
        {
            let t = &threads[slot - 1];
            t.ctx.store(frame as u64, Ordering::Relaxed);
            context::fx_save(t.fx as *mut u8);
        }
        if exit {
            if threads[slot - 1].is_init {
                panic!("init exited ({reason}) — no orphan root");
            }
            let mut raw = [0u8; NAME_CAP];
            let n = threads[slot - 1].name_len as usize;
            raw[..n].copy_from_slice(&threads[slot - 1].name_bytes[..n]);
            let debug_id = threads[slot - 1].debug_id;
            // Exit code: syscall Exit puts it in rdi before handoff; faults use 0.
            let code = if reason == "syscall" {
                // SAFETY: frame is the exiting task's uniform context.
                unsafe { (*frame).rdi }
            } else {
                0
            };
            threads[slot - 1].exit_code.store(code, Ordering::Release);
            threads[slot - 1]
                .state
                .store(STATE_EXITED, Ordering::Release);
            let name = core::str::from_utf8(&raw[..n]).unwrap_or("");
            serial_println!(
                "[sched] task '{}' exited ({}) code={} id={}",
                name,
                reason,
                code,
                debug_id
            );
            wake_exit_waiters(&mut threads, slot as u8, code);
        }

        // Advance the rotation: first eligible slot strictly after the
        // outgoing one (main at slot 0 is always the fallback, and the
        // caller is never main — so this scan ALWAYS finds a switch).
        // SMP: only THIS CPU's own threads are eligible; foreign threads
        // are skipped like tombstones (they rotate on their owner).
        let my_cpu = crate::arch::cpu::current_index() as u8;
        let n = threads.len();
        let mut cand = if slot >= n { 0 } else { slot + 1 };
        let mut scans = n + 1;
        while scans > 0 {
            let eligible = cand == 0
                || (threads[cand - 1].owner == my_cpu
                    && threads[cand - 1].state.load(Ordering::Acquire) == STATE_RUNNING);
            if eligible {
                break;
            }
            cand = if cand + 1 > n { 0 } else { cand + 1 };
            scans -= 1;
        }
        debug_assert!(scans > 0, "handoff rotation scan unwound without main");
        me.last_served.store(cand, Ordering::Relaxed);

        let (who, ctx, rsp0, cr3) = match cand {
            0 => (0usize, me.main_ctx.load(Ordering::Relaxed), None, 0),
            s => {
                let t2 = &threads[s - 1];
                let rsp0 = t2.is_user.then_some(t2.kstack_top);
                (
                    s,
                    t2.ctx.load(Ordering::Relaxed),
                    rsp0,
                    t2.cr3.load(Ordering::Relaxed),
                )
            }
        };
        assert!(ctx != 0, "handoff: entering a task with no saved context");
        crate::arch::syscall::set_task_kstack(rsp0.unwrap_or(0));
        if let Some(top) = rsp0 {
            crate::arch::set_tss_rsp0(VirtAddr::new(top));
        }
        enter_task_cr3(cr3);
        me.current.store(who, Ordering::Relaxed);
        // Outgoing thread's tail is still on its stack until `mov rsp`.
        // The naked tail publishes CTX_STABLE from gs:[40] after that.
        claim_incoming(who);
        crate::arch::cpu::set_departed_slot(slot as u64);
        let fx_ptr = match cand {
            0 => (&*me.main_fx.lock()) as *const FxArea as u64,
            s => threads[s - 1].fx as u64,
        };
        context::fx_restore(fx_ptr as *const u8);
        ctx
    });
    pending_ctx
}

/// The timer switch: saves the outgoing task's context + FPU state (main
/// loop or thread), picks the next thread round-robin, and returns its
/// context pointer — or 0 to resume the outgoing task untouched.
///
/// Called ONLY from the naked timer wrapper (IRQ context, IF=0). Locks are
/// never held across the actual switch: all save/decide/restore happens
/// under the IRQ-gated lock, then the lock is dropped before the naked code
/// swaps RSP.
///
/// # Safety
///
/// `frame` must be the outgoing task's context block (the naked wrapper's
/// RSP).
pub unsafe fn on_timer_tick(frame: *mut context::Context) -> u64 {
    crate::arch::cpu::note_switch();
    if crate::drivers::virtio_blk::completion_ready() {
        wake_io_block();
    }
    watchdog_observe();
    crate::arch::cpu::set_departed_slot(0);
    // Tickless: advance by the one-shot duration that just fired.
    let elapsed = crate::arch::apic::take_armed_ms();
    crate::arch::timer::tick_by(elapsed);
    let now = crate::arch::timer_ticks();

    let me = cpu_sched();
    let next_ctx = interrupts::without_interrupts(|| {
        let mut threads = THREADS.lock();
        // Sleep deadlines use monotonic ticks advanced just above.
        wake_due_sleepers(&mut threads, now);
        let my_cpu = crate::arch::cpu::current_index() as u8;
        let current = me.current.load(Ordering::Relaxed);

        // CPU-time attribution: charge the armed window to whoever ran.
        match current {
            0 => {
                me.main_ticks.fetch_add(elapsed, Ordering::Relaxed);
            }
            i => {
                threads[i - 1].ticks.fetch_add(elapsed, Ordering::Relaxed);
            }
        }

        // Save the outgoing task's context + FPU state.
        match current {
            0 => {
                let mut fx = me.main_fx.lock();
                context::fx_save(&mut *fx as *mut FxArea as *mut u8);
                me.main_ctx.store(frame as u64, Ordering::Relaxed);
            }
            i => {
                let t = &threads[i - 1];
                context::fx_save(t.fx as *mut u8);
                t.ctx.store(frame as u64, Ordering::Relaxed);
            }
        }

        // Unified round-robin over THIS CPU's participants: its main
        // (slot 0), then the threads it OWNS (slots 1..=n; foreign-owned
        // slots are skipped like tombstones — SMP M18). Exited/freed slots
        // are skipped (tombstones; see STATE_* docs). Switching "to main" =
        // returning the per-CPU main context.
        let n = threads.len();
        if n == 0 {
            return None;
        }
        let last = me.last_served.load(Ordering::Relaxed);
        let mut next_slot = if last == usize::MAX {
            1 // first tick ever: serve the first thread
        } else if last + 1 > n {
            0 // wrap to main
        } else {
            last + 1
        };
        // Skip tombstones AND foreign-owned slots; main (slot 0) is always
        // eligible, so the scan terminates after at most n+1 steps.
        let mut scans = n + 1;
        while scans > 0 {
            let eligible = match next_slot {
                0 => true,
                s => {
                    threads[s - 1].owner == my_cpu
                        && threads[s - 1].state.load(Ordering::Acquire) == STATE_RUNNING
                }
            };
            if eligible {
                break;
            }
            next_slot = if next_slot + 1 > n { 0 } else { next_slot + 1 };
            scans -= 1;
        }
        debug_assert!(scans > 0, "rotation scan terminated without main");
        me.last_served.store(next_slot, Ordering::Relaxed);

        // IDLE-PASS WORK STEALING (SMP M19): the scan is about to serve
        // main. That alone is NOT "idle" (the alternate-with-main pattern
        // hits main every other tick even with runnable threads) — so the
        // steal attempt is gated on NO runnable thread being owned by this
        // CPU. Steal = flip the owner (all under the THREADS lock, which
        // both this switch and the victim's switch serialize on); the
        // ENTRY IS DEFERRED TO THIS CPU'S NEXT TICK (the rotation scan
        // picks it up as own+RUNNING). The steal itself requires
        // CTX_STABLE: the victim publishes that only AFTER `mov rsp` off
        // the thread stack. `current != slot` is not enough — it is stored
        // before the lock drops, while the naked tail is still on that
        // stack, and a host-starved victim can miss the stealer's next
        // guest tick (both CPUs then pop one frame). The cooldown
        // (stolen_at) keeps a hot thread from ping-ponging between idle
        // CPUs: it stays put for a while.
        if next_slot == 0 {
            let own_runnable = threads
                .iter()
                .any(|t| t.owner == my_cpu && t.state.load(Ordering::Acquire) == STATE_RUNNING);
            if !own_runnable {
                let now = crate::arch::timer_ticks();
                for (i, t) in threads.iter_mut().enumerate() {
                    let slot_no = i + 1;
                    let stolen_at = t.stolen_at.load(Ordering::Relaxed);
                    if t.owner == my_cpu
                        || t.state.load(Ordering::Acquire) != STATE_RUNNING
                        || t.no_steal
                        || now < stolen_at + STEAL_COOLDOWN_TICKS
                        || CPU_SCHED[t.owner as usize].current.load(Ordering::Relaxed) == slot_no
                        || !CTX_STABLE[i].load(Ordering::Acquire)
                    {
                        continue; // own / dead / resident / cooling / current / tail still on its stack
                    }
                    let victim = t.owner;
                    t.owner = my_cpu;
                    t.stolen_at.store(now, Ordering::Relaxed);
                    STEALS.fetch_add(1, Ordering::Relaxed);
                    // Production serial stays quiet. `verbose-sched` is
                    // the trace; `test-smpstress` proves the owner flip
                    // without this line.
                    #[cfg(feature = "verbose-sched")]
                    serial_println!(
                        "[sched] cpu {} stole '{}' (slot {}) from cpu {} (enters next tick)",
                        my_cpu,
                        t.name(),
                        slot_no,
                        victim
                    );
                    #[cfg(not(feature = "verbose-sched"))]
                    let _ = (my_cpu, victim, slot_no);
                    break;
                }
            }
        }

        // A virtio-blk waiter is halted on this CPU while holding the
        // device lock (and, for galfs, the sector buffer). Leave it
        // there; another thread on this CPU would spin on those locks.
        if crate::drivers::virtio_blk::xfer_wait_cpu() == Some(usize::from(my_cpu)) {
            next_slot = me.current.load(Ordering::Relaxed);
        }

        // Switching to ourselves (all other slots dead or foreign, and no
        // steal landed) = no switch.
        let current = me.current.load(Ordering::Relaxed);
        if next_slot == current {
            return None;
        }

        let (who, ctx, fx_ptr, rsp0, cr3) = match next_slot {
            0 => (
                0usize,
                me.main_ctx.load(Ordering::Relaxed),
                (&*me.main_fx.lock()) as *const FxArea as u64,
                None,
                0,
            ),
            s => {
                let t = &threads[s - 1];
                let rsp0 = t.is_user.then_some(t.kstack_top);
                (
                    s,
                    t.ctx.load(Ordering::Relaxed),
                    t.fx as u64,
                    rsp0,
                    t.cr3.load(Ordering::Relaxed),
                )
            }
        };
        // Ring-3 readiness BEFORE entering the chosen task: a user task's
        // ring 3→0 crossings (timer IRQ via per-CPU TSS.RSP0, later the
        // syscall entry via the per-CPU kstack slot gs:[8]) must push onto
        // ITS OWN kernel stack. Kernel threads/main reset the registry.
        // (Step B: CR3 is installed for the incoming task — the kernel half
        // is shared by every task table, so the switch is safe mid-flight;
        // CR3 itself is per-CPU hardware.)
        crate::arch::syscall::set_task_kstack(rsp0.unwrap_or(0));
        if let Some(top) = rsp0 {
            crate::arch::set_tss_rsp0(VirtAddr::new(top));
        }
        enter_task_cr3(cr3);
        me.current.store(who, Ordering::Relaxed);
        // `current` here is the outgoing slot (reloaded above). Its tail
        // still owns the stack until the naked `mov rsp`; gs:[40] tells
        // that tail which CTX_STABLE byte to set.
        claim_incoming(who);
        crate::arch::cpu::set_departed_slot(current as u64);
        context::fx_restore(fx_ptr as *const u8);
        Some(ctx)
    });

    // Always re-arm a preempt quantum from the IRQ path so boot / busy
    // main work keeps ~1 ms cadence. The idle loop stretches the deadline
    // to the next second via [`arm_timer_for_load`] immediately before `hlt`.
    crate::arch::apic::arm_oneshot_ms(crate::arch::apic::quantum_ms());

    // EOI before entering the next task (or returning to this one).
    crate::arch::end_timer_interrupt();

    next_ctx.unwrap_or(0)
}

/// True when THIS CPU owns at least one `RUNNING` thread.
pub(in crate::sched) fn cpu_has_runnable() -> bool {
    let my_cpu = crate::arch::cpu::current_index() as u8;
    interrupts::without_interrupts(|| {
        THREADS
            .lock()
            .iter()
            .any(|t| t.owner == my_cpu && t.state.load(Ordering::Acquire) == STATE_RUNNING)
    })
}

/// Park the current user task until `timer_ticks` reaches `now + ms`.
///
/// Returns `Ok(())` after marking the task `WAITING` (caller must hand off).
/// No Cap required. `ms` is clamped to `1..=SLEEP_MS_MAX`.
pub(crate) fn task_sleep(ms: u64) -> Result<(), SysError> {
    let ms = ms.clamp(1, galexy_abi::SLEEP_MS_MAX);
    let slot = current_slot();
    if slot == 0 {
        return Err(SysError::BadCap);
    }
    let now = crate::arch::timer_ticks();
    let deadline = now.saturating_add(ms);
    interrupts::without_interrupts(|| {
        let mut threads = THREADS.lock();
        let thread = threads.get_mut(slot - 1).ok_or(SysError::BadCap)?;
        if !thread.is_user || thread.state.load(Ordering::Acquire) != STATE_RUNNING {
            return Err(SysError::BadCap);
        }
        // Same race as Cap-wait: a queued control message must win over
        // a backoff sleep. Checked before WAITING so the send path's
        // wake and this park cannot miss each other.
        if thread.is_init && init_ctrl_queued() {
            return Err(SysError::Interrupted);
        }
        thread.wait_child_slot.store(0, Ordering::Relaxed);
        thread
            .wait_proc_index
            .store(WAIT_PROC_NONE, Ordering::Relaxed);
        thread.wait_child_gen.store(0, Ordering::Relaxed);
        thread.wait_for_exit.store(false, Ordering::Relaxed);
        thread.sleep_deadline.store(deadline, Ordering::Release);
        thread.state.store(STATE_WAITING, Ordering::Release);
        Ok(())
    })
}

/// True when init's control channel already holds a message for end 0.
///
/// Caller holds [`THREADS`]. Channel lock is taken second.
pub(in crate::sched) fn init_ctrl_queued() -> bool {
    let id = INIT_CTRL.load(Ordering::Acquire);
    if id == 0xFF {
        return false;
    }
    channel::queued_for(id, 0)
}

/// Wake sleepers whose deadline is due. Call under the timer path after
/// `timer_ticks` advances.
pub(in crate::sched) fn wake_due_sleepers(threads: &mut [Thread], now: u64) {
    for thread in threads.iter_mut() {
        if thread.state.load(Ordering::Acquire) != STATE_WAITING {
            continue;
        }
        let deadline = thread.sleep_deadline.load(Ordering::Acquire);
        if deadline == 0 || now < deadline {
            continue;
        }
        clear_wait_fields(thread);
        stamp_waiter_frame(thread, SyscallResult::ok(0));
        set_running(thread);
    }
}

/// Marks `thread` runnable and kicks its owner out of `hlt` when that
/// owner is another CPU. The kick handler only EOIs; the idle loop then
/// sees the runnable thread and arms a quantum.
pub(in crate::sched) fn set_running(thread: &Thread) {
    thread.state.store(STATE_RUNNING, Ordering::Release);
    poke_owner(thread.owner);
}

/// Runs a child that was registered with `defer_run`. No-op once it is
/// already runnable. Caller holds [`THREADS`].
pub(in crate::sched) fn release_deferred(threads: &mut [Thread], slot: u8) {
    let Some(thread) = threads.get(slot as usize - 1) else {
        return;
    };
    if thread.state.load(Ordering::Acquire) == STATE_WAITING {
        set_running(thread);
    }
}

/// Wake `owner` if it is another CPU. `kick` is a no-op for the caller
/// and for a CPU that is not online yet, so spawn during BSP bring-up
/// is safe. The thread must already be visible as `RUNNING`.
pub(in crate::sched) fn poke_owner(owner: u8) {
    crate::arch::cpu::kick(owner as usize);
}

pub(in crate::sched) const IO_NONE: u8 = 0;
pub(in crate::sched) const IO_KEYBOARD: u8 = 1;
pub(in crate::sched) const IO_PIPE_READ: u8 = 2;
pub(in crate::sched) const IO_PIPE_WRITE: u8 = 3;
/// Virtio-blk request waiting for the used-ring interrupt.
pub(in crate::sched) const IO_BLOCK: u8 = 4;
/// Channel `recv` parked on an empty endpoint.
pub(in crate::sched) const IO_CHAN_RECV: u8 = 5;
/// Init control `send` parked until init replies.
pub(in crate::sched) const IO_INIT_RPC: u8 = 6;

pub(in crate::sched) fn clear_wait_fields(thread: &Thread) {
    thread.wait_child_slot.store(0, Ordering::Relaxed);
    thread
        .wait_proc_index
        .store(WAIT_PROC_NONE, Ordering::Relaxed);
    thread.wait_child_gen.store(0, Ordering::Relaxed);
    thread.wait_for_exit.store(false, Ordering::Relaxed);
    thread.sleep_deadline.store(0, Ordering::Relaxed);
    thread.io_kind.store(IO_NONE, Ordering::Relaxed);
    thread.io_pipe.store(0, Ordering::Relaxed);
    thread.io_addr.store(0, Ordering::Relaxed);
    thread.io_len.store(0, Ordering::Relaxed);
    thread.io_cap.store(0, Ordering::Relaxed);
    thread.io_extra.store(0, Ordering::Relaxed);
}

pub(in crate::sched) fn park_io(
    threads: &mut [Thread],
    slot: usize,
    kind: u8,
    pipe_id: u8,
    addr: u64,
    len: u32,
    cap_bits: u64,
) {
    let thread = &mut threads[slot - 1];
    thread.wait_child_slot.store(0, Ordering::Relaxed);
    thread
        .wait_proc_index
        .store(WAIT_PROC_NONE, Ordering::Relaxed);
    thread.wait_child_gen.store(0, Ordering::Relaxed);
    thread.wait_for_exit.store(false, Ordering::Relaxed);
    thread.sleep_deadline.store(0, Ordering::Relaxed);
    thread.io_kind.store(kind, Ordering::Release);
    thread.io_pipe.store(pipe_id, Ordering::Relaxed);
    thread.io_addr.store(addr, Ordering::Relaxed);
    thread.io_len.store(len, Ordering::Relaxed);
    thread.io_cap.store(cap_bits, Ordering::Relaxed);
    thread.io_extra.store(0, Ordering::Relaxed);
    thread.state.store(STATE_WAITING, Ordering::Release);
}

/// Wake keyboard readers parked on `tty` (Milestone 57). Completes the
/// pending read into their user buffer when keys are available.
pub fn wake_keyboard_waiters(tty: u8) {
    interrupts::without_interrupts(|| {
        let mut threads = THREADS.lock();
        for i in 0..threads.len() {
            if threads[i].state.load(Ordering::Acquire) != STATE_WAITING {
                continue;
            }
            if threads[i].io_kind.load(Ordering::Acquire) != IO_KEYBOARD {
                continue;
            }
            if threads[i].tty != tty {
                continue;
            }
            let addr = threads[i].io_addr.load(Ordering::Acquire);
            let len = threads[i].io_len.load(Ordering::Acquire) as usize;
            if len == 0 || addr == 0 {
                clear_wait_fields(&threads[i]);
                stamp_waiter_frame(&threads[i], SyscallResult::ok(0));
                set_running(&threads[i]);
                continue;
            }
            // Fill from the keyboard queue while the waiter is still parked
            // (its CR3 is not current — copy via phys map / user walk of
            // the waiter's tree). Use the same staging path as the syscall:
            // temporarily enter the waiter's CR3 is heavy; instead re-stamp
            // "retry" by rewinding is avoided — we fill via with_table.
            let cr3 = threads[i].cr3.load(Ordering::Acquire);
            let n = fill_keyboard_into_user(tty, cr3, addr, len);
            if n == 0 {
                // Still empty (spurious wake) — stay parked.
                continue;
            }
            threads[i].last_input_tick = crate::arch::timer_ticks();
            clear_wait_fields(&threads[i]);
            stamp_waiter_frame(&threads[i], SyscallResult::ok(n as u64));
            set_running(&threads[i]);
        }
    });
}

pub(in crate::sched) fn fill_keyboard_into_user(tty: u8, cr3: u64, addr: u64, len: usize) -> usize {
    let mut staged = [0u8; 256];
    let max = len.min(staged.len());
    let mut filled = 0usize;
    while filled < max {
        let Some(c) = crate::drivers::keyboard::pop_key_tty(tty) else {
            break;
        };
        let mut tmp = [0u8; 4];
        let encoded = c.encode_utf8(&mut tmp);
        if filled + encoded.len() > max {
            crate::drivers::keyboard::unget_key_tty(tty, c);
            break;
        }
        staged[filled..filled + encoded.len()].copy_from_slice(encoded.as_bytes());
        filled += encoded.len();
    }
    if filled == 0 {
        return 0;
    }
    // Copy into the waiter's address space via its page tables.
    if cr3 == 0 {
        return 0;
    }
    // Kernel invariant: the slot was `STATE_WAITING` with the CR3 recorded
    // at park. A corrupt root panics — it is not a user error.
    debug_assert_ne!(cr3, 0, "waiter cr3: STATE_WAITING slot has no root");
    let root = PhysFrame::from_start_address(PhysAddr::new(cr3)).expect("waiter cr3");
    // SAFETY: waiter FreshL4 root; the task is parked so the tree is not
    // CR3-active on this CPU. The copy goes through the phys map.
    let ok = unsafe {
        crate::arch::mm::with_table(root, |mapper| {
            copy_to_user_via(mapper, addr, &staged[..filled])
        })
    };
    if ok {
        filled
    } else {
        0
    }
}

pub(in crate::sched) fn copy_to_user_via(
    mapper: &mut x86_64::structures::paging::OffsetPageTable<'_>,
    addr: u64,
    src: &[u8],
) -> bool {
    use x86_64::structures::paging::{Page, Size4KiB};
    let mut done = 0usize;
    while done < src.len() {
        let va = VirtAddr::new(addr + done as u64);
        let page = Page::<Size4KiB>::containing_address(va);
        let Ok(frame) = mapper.translate_page(page) else {
            return false;
        };
        let off = (va.as_u64() as usize) & 0xFFF;
        let room = (0x1000 - off).min(src.len() - done);
        let dst = crate::arch::mm::frame_virt(frame.start_address()).as_mut_ptr::<u8>();
        // SAFETY: frame backing a present user mapping; exclusive while waiter parks.
        unsafe {
            core::ptr::copy_nonoverlapping(src[done..].as_ptr(), dst.add(off), room);
        }
        done += room;
    }
    true
}

/// Wake tasks parked in `recv` on channel `id`.
pub fn wake_channel_waiters(id: u8) {
    interrupts::without_interrupts(|| {
        let mut threads = THREADS.lock();
        let n = threads.len();
        for i in 0..n {
            if threads[i].state.load(Ordering::Acquire) != STATE_WAITING {
                continue;
            }
            if threads[i].io_kind.load(Ordering::Acquire) != IO_CHAN_RECV {
                continue;
            }
            if threads[i].io_pipe.load(Ordering::Acquire) != id {
                continue;
            }
            complete_chan_recv(&mut threads, i);
        }
    });
}

/// One step of [`complete_chan_recv`] after the channel lock drops.
///
/// The payload stays inline. Boxing it would allocate on the wake path.
#[allow(clippy::large_enum_variant)]
pub(in crate::sched) enum ChanStep {
    /// Queue still empty and the peer is open.
    Stay,
    /// Stamp this result and mark the waiter runnable.
    Done(SyscallResult),
    /// Copy `n` payload bytes and the two Cap words, then stamp `n`.
    Payload {
        n: usize,
        bytes: [u8; galexy_abi::CHAN_MSG_MAX],
        caps: [u64; 2],
    },
}

pub(in crate::sched) fn finish_chan_waiter(thread: &Thread, result: SyscallResult) {
    clear_wait_fields(thread);
    stamp_waiter_frame(thread, result);
    set_running(thread);
}

pub(in crate::sched) fn complete_chan_recv(threads: &mut [Thread], index: usize) {
    let addr = threads[index].io_addr.load(Ordering::Acquire);
    let len = threads[index].io_len.load(Ordering::Acquire) as usize;
    let id = threads[index].io_pipe.load(Ordering::Acquire);
    let cap = Cap::from_bits(threads[index].io_cap.load(Ordering::Acquire));
    let caps_out = threads[index].io_extra.load(Ordering::Acquire);
    let cr3 = threads[index].cr3.load(Ordering::Acquire);
    let end = match channel_end(&threads[index], cap) {
        Ok((cid, end)) if cid == id => end,
        _ => {
            finish_chan_waiter(&threads[index], SyscallResult::err(SysError::BadCap));
            return;
        }
    };
    let step = match channel::with_mut(id, |ch| -> Result<ChanStep, SysError> {
        match channel::pull(ch, end)? {
            Ok(delivery) => {
                let need = delivery.caps.iter().filter(|c| c.is_some()).count();
                if need > 0 && caps_out == 0 {
                    let _ = channel::unpull(ch, delivery);
                    return Ok(ChanStep::Done(SyscallResult::err(SysError::BadBuffer)));
                }
                let free = threads[index].files.iter().filter(|s| s.is_none()).count();
                if need > free {
                    let _ = channel::unpull(ch, delivery);
                    return Ok(ChanStep::Done(SyscallResult::err(SysError::NoResource)));
                }
                let n = delivery.len.min(len);
                let mut bytes = [0u8; galexy_abi::CHAN_MSG_MAX];
                bytes[..n].copy_from_slice(&delivery.data[..n]);
                let mut caps = [0u64; 2];
                for (i, file) in delivery.caps.into_iter().enumerate() {
                    let Some(file) = file else { continue };
                    let Some(slot) = threads[index].files.iter().position(|s| s.is_none()) else {
                        continue;
                    };
                    let rights = file.rights;
                    threads[index].files[slot] = Some(file);
                    caps[i] = Cap::new(galexy_abi::FILE_CAP_BASE + slot as u64, rights).bits();
                }
                Ok(ChanStep::Payload { n, bytes, caps })
            }
            Err(channel::Empty::Eof) => Ok(ChanStep::Done(SyscallResult::ok(0))),
            Err(channel::Empty::Wait) => Ok(ChanStep::Stay),
        }
    }) {
        Ok(Ok(step)) => step,
        Ok(Err(err)) | Err(err) => ChanStep::Done(SyscallResult::err(err)),
    };
    match step {
        ChanStep::Stay => {}
        ChanStep::Done(result) => finish_chan_waiter(&threads[index], result),
        ChanStep::Payload { n, bytes, caps } => {
            let mut raw = [0u8; 16];
            raw[..8].copy_from_slice(&caps[0].to_le_bytes());
            raw[8..].copy_from_slice(&caps[1].to_le_bytes());
            let root = PhysFrame::from_start_address(PhysAddr::new(cr3)).expect("waiter cr3");
            // SAFETY: parked waiter's tree, not CR3-active. Phys-map copy.
            let ok = unsafe {
                crate::arch::mm::with_table(root, |mapper| {
                    let data_ok = n == 0 || copy_to_user_via(mapper, addr, &bytes[..n]);
                    let caps_ok = caps_out == 0 || copy_to_user_via(mapper, caps_out, &raw);
                    data_ok && caps_ok
                })
            };
            let result = if ok {
                SyscallResult::ok(n as u64)
            } else {
                SyscallResult::err(SysError::BadBuffer)
            };
            finish_chan_waiter(&threads[index], result);
        }
    }
}

/// Wake pipe readers (data or EOF) / writers (space or closed).
pub fn wake_pipe_waiters(id: u8) {
    interrupts::without_interrupts(|| {
        let mut threads = THREADS.lock();
        // Alternate reader/writer passes: a completed write frees data for
        // readers, and a completed read frees space for writers.
        // Fixed-size waiter lists: this runs under THREADS on the IF=0
        // syscall path, where a heap allocation must not happen (it can
        // grow the heap and broadcast while the lock is held).
        for _ in 0..MAX_THREADS {
            let mut readers = [0usize; MAX_THREADS];
            let mut writers = [0usize; MAX_THREADS];
            let mut nr = 0usize;
            let mut nw = 0usize;
            for (i, t) in threads.iter().enumerate().take(MAX_THREADS) {
                if t.state.load(Ordering::Acquire) != STATE_WAITING {
                    continue;
                }
                if t.io_pipe.load(Ordering::Acquire) != id {
                    continue;
                }
                match t.io_kind.load(Ordering::Acquire) {
                    IO_PIPE_READ => {
                        readers[nr] = i;
                        nr += 1;
                    }
                    IO_PIPE_WRITE => {
                        writers[nw] = i;
                        nw += 1;
                    }
                    _ => {}
                }
            }
            let before = nr + nw;
            if before == 0 {
                break;
            }
            for &i in &readers[..nr] {
                complete_pipe_read(&mut threads, i);
            }
            for &i in &writers[..nw] {
                complete_pipe_write(&mut threads, i);
            }
            let mut still = 0usize;
            for t in threads.iter() {
                if t.state.load(Ordering::Acquire) != STATE_WAITING {
                    continue;
                }
                if t.io_pipe.load(Ordering::Acquire) != id {
                    continue;
                }
                let k = t.io_kind.load(Ordering::Acquire);
                if k == IO_PIPE_READ || k == IO_PIPE_WRITE {
                    still += 1;
                }
            }
            if still >= before {
                break; // no forward progress (still WouldBlock)
            }
        }
    });
}

pub(in crate::sched) fn complete_pipe_read(threads: &mut [Thread], index: usize) {
    let addr = threads[index].io_addr.load(Ordering::Acquire);
    let len = threads[index].io_len.load(Ordering::Acquire) as usize;
    let id = threads[index].io_pipe.load(Ordering::Acquire);
    let cap_bits = threads[index].io_cap.load(Ordering::Acquire);
    let cr3 = threads[index].cr3.load(Ordering::Acquire);
    let mut staged = [0u8; 256];
    let max = len.min(staged.len());
    let result = match pipe::try_read(id, &mut staged[..max]) {
        Ok(pipe::ReadResult::Ready(0)) => SyscallResult::ok(0),
        Ok(pipe::ReadResult::Ready(n)) => {
            debug_assert_eq!(
                threads[index].state.load(Ordering::Acquire),
                STATE_WAITING,
                "pipe read completion is a kernel invariant: slot is WAITING"
            );
            // Kernel invariant (DESIGN): a parked waiter's CR3 is the root
            // recorded at park. Corrupt means the scheduler broke, so panic.
            let root = PhysFrame::from_start_address(PhysAddr::new(cr3)).expect("cr3");
            // SAFETY: parked waiter's tree, not CR3-active. Phys-map copy.
            let ok = unsafe {
                crate::arch::mm::with_table(root, |mapper| {
                    copy_to_user_via(mapper, addr, &staged[..n])
                })
            };
            if ok {
                SyscallResult::ok(n as u64)
            } else {
                SyscallResult::err(SysError::BadBuffer)
            }
        }
        Ok(pipe::ReadResult::Eof) => SyscallResult::ok(0),
        Ok(pipe::ReadResult::WouldBlock) => return, // stay parked
        Err(e) => SyscallResult::err(e),
    };
    let _ = cap_bits;
    clear_wait_fields(&threads[index]);
    stamp_waiter_frame(&threads[index], result);
    set_running(&threads[index]);
}

pub(in crate::sched) fn complete_pipe_write(threads: &mut [Thread], index: usize) {
    let addr = threads[index].io_addr.load(Ordering::Acquire);
    let len = threads[index].io_len.load(Ordering::Acquire) as usize;
    let id = threads[index].io_pipe.load(Ordering::Acquire);
    let cr3 = threads[index].cr3.load(Ordering::Acquire);
    let mut staged = [0u8; 256];
    let max = len.min(staged.len());
    // Copy FROM user into staging.
    debug_assert_eq!(
        threads[index].state.load(Ordering::Acquire),
        STATE_WAITING,
        "pipe write completion is a kernel invariant: slot is WAITING"
    );
    // Kernel invariant (DESIGN): same as the pipe-read path.
    let root = PhysFrame::from_start_address(PhysAddr::new(cr3)).expect("cr3");
    // SAFETY: parked waiter's tree, not CR3-active. Phys-map copy.
    let ok = unsafe {
        crate::arch::mm::with_table(root, |mapper| {
            copy_from_user_via(mapper, addr, &mut staged[..max])
        })
    };
    if !ok {
        clear_wait_fields(&threads[index]);
        stamp_waiter_frame(&threads[index], SyscallResult::err(SysError::BadBuffer));
        set_running(&threads[index]);
        return;
    }
    let result = match pipe::try_write(id, &staged[..max]) {
        Ok(pipe::WriteResult::Ready(n)) => SyscallResult::ok(n as u64),
        Ok(pipe::WriteResult::WouldBlock) => return,
        Ok(pipe::WriteResult::Closed) => SyscallResult::err(SysError::Unsupported),
        Err(e) => SyscallResult::err(e),
    };
    clear_wait_fields(&threads[index]);
    stamp_waiter_frame(&threads[index], result);
    set_running(&threads[index]);
}

pub(in crate::sched) fn copy_from_user_via(
    mapper: &mut x86_64::structures::paging::OffsetPageTable<'_>,
    addr: u64,
    dst: &mut [u8],
) -> bool {
    use x86_64::structures::paging::{Page, Size4KiB};
    let mut done = 0usize;
    while done < dst.len() {
        let va = VirtAddr::new(addr + done as u64);
        let page = Page::<Size4KiB>::containing_address(va);
        let Ok(frame) = mapper.translate_page(page) else {
            return false;
        };
        let off = (va.as_u64() as usize) & 0xFFF;
        let room = (0x1000 - off).min(dst.len() - done);
        let src = crate::arch::mm::frame_virt(frame.start_address()).as_ptr::<u8>();
        // SAFETY: `src` is the phys-map image of a present user page; the
        // waiter is parked so the frame is not written by its task.
        unsafe {
            core::ptr::copy_nonoverlapping(src.add(off), dst[done..].as_mut_ptr(), room);
        }
        done += room;
    }
    true
}

/// Park the current task on an empty keyboard read. Caller must hand off.
pub(crate) fn task_park_keyboard(cap_bits: u64, addr: u64, len: u32) -> Result<(), SysError> {
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
        park_io(&mut threads, slot, IO_KEYBOARD, 0, addr, len, cap_bits);
        Ok(())
    })
}

/// Park the current user task until virtio-blk's used ring advances.
pub fn park_io_block() -> Result<(), SysError> {
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
        park_io(&mut threads, slot, IO_BLOCK, 0, 0, 0, 0);
        Ok(())
    })
}

/// True while this task is still parked on a block request.
pub fn io_block_waiting() -> bool {
    let slot = current_slot();
    if slot == 0 {
        return false;
    }
    interrupts::without_interrupts(|| {
        THREADS.lock().get(slot - 1).is_some_and(|thread| {
            thread.state.load(Ordering::Acquire) == STATE_WAITING
                && thread.io_kind.load(Ordering::Acquire) == IO_BLOCK
        })
    })
}

/// Clears an `IO_BLOCK` park. The IRQ may already have set `RUNNING`.
pub fn clear_io_block() {
    let slot = current_slot();
    if slot == 0 {
        return;
    }
    interrupts::without_interrupts(|| {
        let mut threads = THREADS.lock();
        let Some(thread) = threads.get_mut(slot - 1) else {
            return;
        };
        if thread.io_kind.load(Ordering::Acquire) != IO_BLOCK {
            return;
        }
        thread.io_kind.store(IO_NONE, Ordering::Release);
        if thread.state.load(Ordering::Acquire) == STATE_WAITING {
            set_running(thread);
        }
    });
}

/// Wakes every task parked on virtio-blk. Called from the INTx handler
/// and from the timer if the used ring moved without a delivery.
pub fn wake_io_block() {
    interrupts::without_interrupts(|| {
        let mut threads = THREADS.lock();
        for thread in threads.iter_mut() {
            if thread.io_kind.load(Ordering::Acquire) == IO_BLOCK
                && thread.state.load(Ordering::Acquire) == STATE_WAITING
            {
                set_running(thread);
            }
        }
    });
}

/// Park on a pipe end. `read` selects reader vs writer wait.
pub(crate) fn task_park_pipe(
    pipe_id: u8,
    read: bool,
    cap_bits: u64,
    addr: u64,
    len: u32,
) -> Result<(), SysError> {
    let slot = current_slot();
    if slot == 0 {
        return Err(SysError::BadCap);
    }
    let kind = if read { IO_PIPE_READ } else { IO_PIPE_WRITE };
    interrupts::without_interrupts(|| {
        let mut threads = THREADS.lock();
        let thread = threads.get_mut(slot - 1).ok_or(SysError::BadCap)?;
        if !thread.is_user || thread.state.load(Ordering::Acquire) != STATE_RUNNING {
            return Err(SysError::BadCap);
        }
        park_io(&mut threads, slot, kind, pipe_id, addr, len, cap_bits);
        Ok(())
    })
}

/// Cancel I/O / sleep waits on a slot with [`SysError::Interrupted`].
pub(in crate::sched) fn interrupt_io_waiter(threads: &mut [Thread], index: usize) {
    if threads[index].state.load(Ordering::Acquire) != STATE_WAITING {
        return;
    }
    let io = threads[index].io_kind.load(Ordering::Acquire);
    let sleeping = threads[index].sleep_deadline.load(Ordering::Acquire) != 0;
    if io == IO_NONE && !sleeping {
        return; // Cap-wait / spawn — leave for exit wake
    }
    clear_wait_fields(&threads[index]);
    stamp_waiter_frame(&threads[index], SyscallResult::err(SysError::Interrupted));
    set_running(&threads[index]);
}

/// Milliseconds until the nearest sleep deadline, if any sleeper exists.
pub(in crate::sched) fn ms_until_next_sleep() -> Option<u32> {
    let now = crate::arch::timer_ticks();
    interrupts::without_interrupts(|| {
        let threads = THREADS.lock();
        let mut best: Option<u64> = None;
        for thread in threads.iter() {
            if thread.state.load(Ordering::Acquire) != STATE_WAITING {
                continue;
            }
            let deadline = thread.sleep_deadline.load(Ordering::Acquire);
            if deadline == 0 {
                continue;
            }
            let remain = if deadline <= now { 1 } else { deadline - now };
            best = Some(match best {
                Some(b) => b.min(remain),
                None => remain,
            });
        }
        best.map(|ms| ms.min(u64::from(crate::arch::apic::IDLE_MAX_MS)) as u32)
    })
}

/// Another CPU with runnable threads that has not entered the scheduler
/// for this long is dumped once. Idle `hlt` (no runnable thread) is not
/// a stall — the tickless deadline is at most one second.
pub(in crate::sched) const WATCHDOG_MS: u64 = 2_000;
pub(in crate::sched) static WATCHDOG_DUMPED: [AtomicBool; crate::arch::cpu::MAX_CPUS] =
    [const { AtomicBool::new(false) }; crate::arch::cpu::MAX_CPUS];

pub(in crate::sched) fn watchdog_observe() {
    let now = crate::arch::timer_ticks();
    let me = crate::arch::cpu::current_index();
    let online = crate::arch::cpu::online();
    let Some(threads) = THREADS.try_lock() else {
        return;
    };
    let mut stalled: Option<usize> = None;
    for (cpu, dumped) in WATCHDOG_DUMPED.iter().enumerate().take(online) {
        if cpu == me {
            continue;
        }
        let last = crate::arch::cpu::last_switch_tick(cpu);
        if last == 0 || now.saturating_sub(last) <= WATCHDOG_MS {
            continue;
        }
        let runnable = threads
            .iter()
            .any(|t| t.owner == cpu as u8 && t.state.load(Ordering::Acquire) == STATE_RUNNING);
        if !runnable {
            continue;
        }
        if dumped
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Relaxed)
            .is_err()
        {
            continue;
        }
        stalled = Some(cpu);
        break;
    }
    let mut snap = [(0usize, 0usize, 0u64, 0u32); crate::arch::cpu::MAX_CPUS];
    let n = online.min(snap.len());
    for (cpu, slot) in snap.iter_mut().enumerate().take(n) {
        let sched = &CPU_SCHED[cpu];
        *slot = (
            sched.current.load(Ordering::Relaxed),
            sched.last_served.load(Ordering::Relaxed),
            crate::arch::cpu::last_switch_tick(cpu),
            crate::arch::apic::armed_ms(cpu),
        );
    }
    drop(threads);
    let Some(cpu) = stalled else {
        return;
    };
    serial_println!(
        "[watchdog] cpu {} stalled {} ms with runnable work (observer cpu {})",
        cpu,
        now.saturating_sub(snap[cpu].2),
        me
    );
    for (i, row) in snap.iter().enumerate().take(n) {
        serial_println!(
            "[watchdog] cpu {} current {} last_served {} armed_ms {} last_switch {}",
            i,
            row.0,
            row.1,
            row.3,
            row.2
        );
    }
    crate::arch::mm::shootdown::log_mailbox();
}

/// Reprogram the local LAPIC for the current load (call before `hlt`).
///
/// Busy → preempt quantum; idle → min(next whole second, next sleeper).
/// Device IRQs still wake the CPU early; the next halt re-arms. The IRQ
/// path itself always re-arms a quantum (preempt fairness).
pub fn arm_timer_for_load() {
    arm_timer_capped(u32::MAX);
}

/// Like [`arm_timer_for_load`], but an idle CPU also wakes within `cap_ms`.
///
/// The BSP main loop passes the time until the next 500 ms cursor edge so
/// the underscore can blink while the seat is idle. Busy CPUs ignore the
/// cap and keep the preempt quantum.
pub fn arm_timer_capped(cap_ms: u32) {
    if cpu_has_runnable() {
        crate::arch::apic::arm_oneshot_ms(crate::arch::apic::quantum_ms());
    } else {
        let idle = crate::arch::apic::idle_deadline_ms().min(cap_ms.max(1));
        let ms = match ms_until_next_sleep() {
            Some(s) => idle.min(s).max(1),
            None => idle,
        };
        crate::arch::apic::arm_oneshot_ms(ms);
    }
    // A wake can land while the idle deadline is being programmed (the
    // other CPU's exit handoff, or a kick that has not been taken yet).
    // Re-arm a quantum if work appeared, so `hlt` is not a full second
    // with a runnable thread already owned here.
    if cpu_has_runnable() {
        crate::arch::apic::arm_oneshot_ms(crate::arch::apic::quantum_ms());
    }
}
