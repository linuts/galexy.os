//! Login cool-down for the syscall path.
//!
//! State lives in a RAM table ([`galexy_core::LoginLockout`]). The clock is
//! [`crate::arch::timer_ticks`] (~1 ms). The lock is never held together
//! with `THREADS` or the galfs table: callers drop those first.
//!
//! Serial lines name the actor and TTY only — never a password.

use galexy_core::{LoginLockout, LOCKOUT_COOLDOWN_MS, LOCKOUT_MAX_FAILS};
use spin::Mutex;
use x86_64::instructions::interrupts;

use crate::serial_println;

static LOCKOUT: Mutex<LoginLockout> = Mutex::new(LoginLockout::new());

fn with_lockout<R>(f: impl FnOnce(&mut LoginLockout) -> R) -> R {
    interrupts::without_interrupts(|| {
        let mut guard = LOCKOUT.lock();
        f(&mut guard)
    })
}

/// `true` when this actor or this TTY is inside a cool-down.
pub(crate) fn blocked(name: &str, tty: u8) -> bool {
    let now = crate::arch::timer_ticks();
    with_lockout(|table| table.blocked(name, tty, now))
}

/// Count a wrong password (`known_actor`) or an unknown name.
pub(crate) fn record_failure(name: &str, tty: u8, known_actor: bool) {
    let now = crate::arch::timer_ticks();
    let note = with_lockout(|table| table.fail(name, tty, now, known_actor));
    let shown = if note.tracked {
        note.actor_fails.max(note.tty_fails)
    } else {
        note.tty_fails
    };
    serial_println!(
        "[auth] login fail user={} tty={} fails={}",
        audit_user(name),
        tty_number(tty),
        shown
    );
    if note.actor_armed {
        serial_println!(
            "[auth] lockout user={} for {}ms after {} fails",
            audit_user(name),
            LOCKOUT_COOLDOWN_MS,
            LOCKOUT_MAX_FAILS
        );
    }
    if note.tty_armed {
        serial_println!(
            "[auth] lockout tty={} for {}ms after {} fails",
            tty_number(tty),
            LOCKOUT_COOLDOWN_MS,
            LOCKOUT_MAX_FAILS
        );
    }
}

/// A password login succeeded: clear that actor and that TTY.
pub(crate) fn record_success(name: &str, tty: u8) {
    with_lockout(|table| table.success(name, tty));
}

/// Forget actor cool-down state after `userdel`.
pub(crate) fn clear_actor(name: &str) {
    with_lockout(|table| table.clear_actor(name));
}

/// Log a guess refused because a cool-down is already running.
pub(crate) fn note_refused(name: &str, tty: u8) {
    serial_println!(
        "[auth] login refused user={} tty={} locked",
        audit_user(name),
        tty_number(tty)
    );
}

/// Actor slots plus TTY slots still inside a cool-down.
pub fn lockout_active() -> u64 {
    let now = crate::arch::timer_ticks();
    with_lockout(|table| table.active(now))
}

/// Absolute `timer_ticks` deadline for `name`, or `0` when it is not locked.
pub fn lockout_actor_until(name: &str) -> u64 {
    with_lockout(|table| table.actor_until(name))
}

/// Absolute `timer_ticks` deadline for 0-based `tty`, or `0` when it is not locked.
pub fn lockout_tty_until(tty: u8) -> u64 {
    with_lockout(|table| table.tty_until(tty))
}

fn tty_number(tty: u8) -> u8 {
    tty.saturating_add(1)
}

fn audit_user(name: &str) -> &str {
    if !name.is_empty()
        && name.len() <= 32
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'_' || b == b'-')
    {
        name
    } else {
        "?"
    }
}
