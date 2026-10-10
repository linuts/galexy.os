//! Capability channels: one queued message, optional Cap payload.
//!
//! The design lives in `docs/PROCESS.md` (Milestone 66). This module is
//! the table. The scheduler installs Caps and parks receivers.

use crate::sync::Mutex;

use galexy_abi::{SysError, CHAN_MSG_MAX};

use super::OpenFile;

/// Channels the kernel will hold.
pub const CHAN_SLOTS: usize = 8;

pub(crate) struct Chan {
    used: bool,
    /// Endpoint `0` and `1` still open.
    open: [bool; 2],
    pending: bool,
    /// Endpoint that queued [`Self::pending`].
    from: u8,
    len: u16,
    data: [u8; CHAN_MSG_MAX],
    caps: [Option<OpenFile>; 2],
}

impl Chan {
    const fn empty() -> Self {
        Self {
            used: false,
            open: [false, false],
            pending: false,
            from: 0,
            len: 0,
            data: [0; CHAN_MSG_MAX],
            caps: [None, None],
        }
    }
}

/// A message taken off a channel. Caps have left the slot.
pub(crate) struct Delivery {
    /// Endpoint that sent it (`0` or `1`).
    pub from: u8,
    /// Payload length.
    pub len: usize,
    /// Payload bytes. Only `len` are meaningful.
    pub data: [u8; CHAN_MSG_MAX],
    /// Moved file Caps, if the sender attached any.
    pub caps: [Option<OpenFile>; 2],
}

/// Why `take` did not return a message.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Empty {
    /// Peer is still open. The caller should park.
    Wait,
    /// Peer is closed and nothing is queued.
    Eof,
}

static CHANS: Mutex<[Chan; CHAN_SLOTS]> = Mutex::new([const { Chan::empty() }; CHAN_SLOTS]);

/// Channels currently allocated.
pub fn in_use() -> usize {
    CHANS.lock().iter().filter(|c| c.used).count()
}

/// Allocates a channel with both endpoints open.
pub fn alloc() -> Result<u8, SysError> {
    let mut chans = CHANS.lock();
    let Some(id) = chans.iter().position(|c| !c.used) else {
        return Err(SysError::NoResource);
    };
    chans[id] = Chan {
        used: true,
        open: [true, true],
        pending: false,
        from: 0,
        len: 0,
        data: [0; CHAN_MSG_MAX],
        caps: [None, None],
    };
    Ok(id as u8)
}

/// Queues `data` from `from`. `Full` leaves the caller's Caps untouched
/// (the caller still holds them). `Closed` means the other end is gone.
pub(crate) fn enqueue(
    id: u8,
    from: u8,
    data: &[u8],
    caps: [Option<OpenFile>; 2],
) -> Result<(), SysError> {
    if from > 1 || data.len() > CHAN_MSG_MAX {
        return Err(SysError::BadValue);
    }
    let mut chans = CHANS.lock();
    let Some(ch) = chans.get_mut(id as usize) else {
        return Err(SysError::BadCap);
    };
    if !ch.used || !ch.open[from as usize] {
        return Err(SysError::BadCap);
    }
    let peer = 1 - from as usize;
    if !ch.open[peer] {
        return Err(SysError::Unsupported);
    }
    if ch.pending {
        return Err(SysError::NoResource);
    }
    ch.data[..data.len()].copy_from_slice(data);
    ch.len = data.len() as u16;
    ch.from = from;
    ch.caps = caps;
    ch.pending = true;
    Ok(())
}

/// True when a message is queued for `end` (the sender was the other end).
///
/// Callers that also hold `THREADS` must take that lock first.
pub(crate) fn queued_for(id: u8, end: u8) -> bool {
    if end > 1 {
        return false;
    }
    let chans = CHANS.lock();
    let Some(ch) = chans.get(id as usize) else {
        return false;
    };
    ch.used && ch.open[end as usize] && ch.pending && ch.from != end
}

/// Runs `f` while the channel row is locked. Callers that also hold
/// `THREADS` must take that lock first.
pub(crate) fn with_mut<R>(id: u8, f: impl FnOnce(&mut Chan) -> R) -> Result<R, SysError> {
    let mut chans = CHANS.lock();
    let Some(ch) = chans.get_mut(id as usize) else {
        return Err(SysError::BadCap);
    };
    if !ch.used {
        return Err(SysError::BadCap);
    }
    Ok(f(ch))
}

/// Takes a message addressed to `end`, or says why there isn't one.
/// Does not lock; [`with_mut`] is the lock.
pub(crate) fn pull(ch: &mut Chan, end: u8) -> Result<Result<Delivery, Empty>, SysError> {
    if end > 1 || !ch.open[end as usize] {
        return Err(SysError::BadCap);
    }
    if ch.pending && ch.from != end {
        let len = ch.len as usize;
        let mut data = [0u8; CHAN_MSG_MAX];
        data[..len].copy_from_slice(&ch.data[..len]);
        let caps = ch.caps;
        ch.caps = [None, None];
        ch.pending = false;
        ch.len = 0;
        let from = ch.from;
        return Ok(Ok(Delivery {
            from,
            len,
            data,
            caps,
        }));
    }
    let peer = 1 - (end as usize);
    if !ch.open[peer] && !ch.pending {
        return Ok(Err(Empty::Eof));
    }
    Ok(Err(Empty::Wait))
}

/// Puts a delivery back onto `ch` without locking.
pub(crate) fn unpull(ch: &mut Chan, delivery: Delivery) -> bool {
    if ch.pending {
        return false;
    }
    let n = delivery.len.min(CHAN_MSG_MAX);
    ch.data[..n].copy_from_slice(&delivery.data[..n]);
    ch.len = n as u16;
    ch.from = delivery.from;
    ch.caps = delivery.caps;
    ch.pending = true;
    true
}

/// Puts a delivery back when the caller does not already hold the channel
/// lock. Receivers that are inside [`with_mut`] use [`unpull`] instead.
#[allow(dead_code)]
/// Puts a delivery back. Used when the receiver cannot install Caps.
/// Fails with `NoResource` if another message landed; the caller must
/// not drop `delivery`'s Caps in that case — this only happens if the
/// slot was refilled, which the single-message design forbids while
/// the caller still holds the old payload. Treated as a kernel bug.
pub(crate) fn restore(id: u8, delivery: Delivery) -> Result<(), SysError> {
    let mut chans = CHANS.lock();
    let Some(ch) = chans.get_mut(id as usize) else {
        return Err(SysError::BadCap);
    };
    if !ch.used {
        return Err(SysError::BadCap);
    }
    if ch.pending {
        return Err(SysError::NoResource);
    }
    let n = delivery.len.min(CHAN_MSG_MAX);
    ch.data[..n].copy_from_slice(&delivery.data[..n]);
    ch.len = n as u16;
    ch.from = delivery.from;
    ch.caps = delivery.caps;
    ch.pending = true;
    Ok(())
}

/// Closes one endpoint. When both are gone, returns any queued Caps so
/// the caller can release them (the lock is not held across that).
pub(crate) fn close_end(id: u8, end: u8) -> [Option<OpenFile>; 2] {
    if end > 1 {
        return [None, None];
    }
    let mut chans = CHANS.lock();
    let Some(ch) = chans.get_mut(id as usize) else {
        return [None, None];
    };
    if !ch.used {
        return [None, None];
    }
    ch.open[end as usize] = false;
    if ch.open[0] || ch.open[1] {
        return [None, None];
    }
    let caps = ch.caps;
    *ch = Chan::empty();
    caps
}
