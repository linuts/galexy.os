//! RAM-only login failure cool-downs.
//!
//! The kernel supplies monotonic `now` (milliseconds). Nothing here touches
//! disk, passwords, or hashes — a reboot clears the table.

/// Failed guesses against one actor or one TTY before a cool-down.
pub const LOCKOUT_MAX_FAILS: u8 = 5;

/// Cool-down length in monotonic milliseconds.
pub const LOCKOUT_COOLDOWN_MS: u64 = 5_000;

/// Actor slots. Matches the galfs actor table so every live account fits.
pub const LOCKOUT_ACTORS: usize = 32;

/// Seat slots. Matches the twelve F-key consoles.
pub const LOCKOUT_TTYS: usize = 12;

/// Maximum actor name stored in a slot.
pub const LOCKOUT_NAME: usize = 32;

/// What one failed guess did to the counters.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct FailNote {
    /// Per-actor count after the call. `0` when the name was not tracked.
    pub actor_fails: u8,
    /// Per-TTY count after the call. `0` when `tty` is out of range.
    pub tty_fails: u8,
    /// Absolute cool-down deadline for the actor, or `0` if it is not locked.
    pub actor_until: u64,
    /// Absolute cool-down deadline for the TTY, or `0` if it is not locked.
    pub tty_until: u64,
    /// `true` when this call armed the actor cool-down.
    pub actor_armed: bool,
    /// `true` when this call armed the TTY cool-down.
    pub tty_armed: bool,
    /// `true` when an actor slot recorded this name.
    pub tracked: bool,
}

#[derive(Clone, Copy)]
struct ActorFail {
    name: [u8; LOCKOUT_NAME],
    name_len: u8,
    fails: u8,
    until: u64,
}

impl ActorFail {
    const EMPTY: Self = Self {
        name: [0; LOCKOUT_NAME],
        name_len: 0,
        fails: 0,
        until: 0,
    };
}

#[derive(Clone, Copy)]
struct TtyFail {
    fails: u8,
    until: u64,
}

impl TtyFail {
    const EMPTY: Self = Self { fails: 0, until: 0 };
}

/// Fixed tables of actor and TTY failure state.
#[derive(Clone, Copy)]
pub struct LoginLockout {
    actors: [ActorFail; LOCKOUT_ACTORS],
    ttys: [TtyFail; LOCKOUT_TTYS],
}

impl LoginLockout {
    /// Empty tables: nobody is locked.
    pub const fn new() -> Self {
        Self {
            actors: [ActorFail::EMPTY; LOCKOUT_ACTORS],
            ttys: [TtyFail::EMPTY; LOCKOUT_TTYS],
        }
    }

    /// `true` when `name` or `tty` is inside a cool-down at `now`.
    ///
    /// A locked TTY refuses every name. A locked actor refuses that name
    /// on every TTY. `until == now` has already elapsed (`until > now`).
    pub fn blocked(&self, name: &str, tty: u8, now: u64) -> bool {
        if self.tty_until(tty) > now {
            return true;
        }
        self.actor_until(name) > now
    }

    /// Absolute actor deadline, or `0` when that name has no live slot.
    pub fn actor_until(&self, name: &str) -> u64 {
        self.find_actor(name)
            .map(|i| self.actors[i].until)
            .unwrap_or(0)
    }

    /// Absolute TTY deadline, or `0` when `tty` is out of range or clear.
    pub fn tty_until(&self, tty: u8) -> u64 {
        tty_index(tty).map(|i| self.ttys[i].until).unwrap_or(0)
    }

    /// Actor slots plus TTY slots whose cool-down is still in the future.
    pub fn active(&self, now: u64) -> u64 {
        let actors = self.actors.iter().filter(|a| a.until > now).count();
        let ttys = self.ttys.iter().filter(|t| t.until > now).count();
        (actors + ttys) as u64
    }

    /// Count a failed guess.
    ///
    /// `known_actor` stores the name. Unknown names only advance the TTY,
    /// so a probe cannot fill the actor table. A call during an active
    /// cool-down does not extend it. The guess that reaches
    /// [`LOCKOUT_MAX_FAILS`] arms `now + `[`LOCKOUT_COOLDOWN_MS`].
    pub fn fail(&mut self, name: &str, tty: u8, now: u64, known_actor: bool) -> FailNote {
        let mut note = FailNote::default();
        if let Some(i) = tty_index(tty) {
            let (fails, armed) = bump(&mut self.ttys[i].fails, &mut self.ttys[i].until, now);
            note.tty_fails = fails;
            note.tty_until = self.ttys[i].until;
            note.tty_armed = armed;
        }
        if known_actor {
            if let Some(i) = self.prepare_actor(name, now) {
                let (fails, armed) =
                    bump(&mut self.actors[i].fails, &mut self.actors[i].until, now);
                note.tracked = true;
                note.actor_fails = fails;
                note.actor_until = self.actors[i].until;
                note.actor_armed = armed;
            }
        }
        note
    }

    /// A successful password login clears that actor and that TTY.
    pub fn success(&mut self, name: &str, tty: u8) {
        self.clear_actor(name);
        if let Some(i) = tty_index(tty) {
            self.ttys[i] = TtyFail::EMPTY;
        }
    }

    /// Drop actor state (user deleted, or a successful login).
    pub fn clear_actor(&mut self, name: &str) {
        if let Some(i) = self.find_actor(name) {
            self.actors[i] = ActorFail::EMPTY;
        }
    }

    fn find_actor(&self, name: &str) -> Option<usize> {
        self.actors.iter().position(|a| name_eq(a, name))
    }

    fn prepare_actor(&mut self, name: &str, now: u64) -> Option<usize> {
        if name.is_empty() || name.len() > LOCKOUT_NAME {
            return None;
        }
        if let Some(i) = self.find_actor(name) {
            return Some(i);
        }
        let free = self
            .actors
            .iter()
            .position(|a| a.name_len == 0)
            .or_else(|| {
                self.actors
                    .iter()
                    .position(|a| a.until != 0 && a.until <= now)
            })?;
        self.actors[free] = ActorFail::EMPTY;
        let n = name.len();
        self.actors[free].name[..n].copy_from_slice(name.as_bytes());
        self.actors[free].name_len = n as u8;
        Some(free)
    }
}

impl Default for LoginLockout {
    fn default() -> Self {
        Self::new()
    }
}

fn tty_index(tty: u8) -> Option<usize> {
    let i = tty as usize;
    if i < LOCKOUT_TTYS {
        Some(i)
    } else {
        None
    }
}

fn name_eq(slot: &ActorFail, name: &str) -> bool {
    !name.is_empty()
        && slot.name_len as usize == name.len()
        && &slot.name[..name.len()] == name.as_bytes()
}

/// Advance one counter unless it is already inside a cool-down.
///
/// Returns the count after the call, and whether this call armed the deadline.
fn bump(fails: &mut u8, until: &mut u64, now: u64) -> (u8, bool) {
    if *until > now {
        return (*fails, false);
    }
    if *until != 0 {
        *fails = 0;
        *until = 0;
    }
    *fails = fails.saturating_add(1);
    let armed = *fails >= LOCKOUT_MAX_FAILS;
    if armed {
        *until = now.saturating_add(LOCKOUT_COOLDOWN_MS);
    }
    (*fails, armed)
}
