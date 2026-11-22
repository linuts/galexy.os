//! Byte lanes between `net` and one other task.
//!
//! A lane is not a pipe. Each direction holds 16 KiB. `read` of an empty
//! open lane parks; `0` is end of file, which is the other end closed and
//! the buffer empty. The table is static so the syscall path does not
//! allocate. Closing the last end wipes the bytes.

use galexy_abi::{SysError, LANE_BYTES, LANE_SLOTS};

use crate::sync::Mutex;

pub enum ReadResult {
    Ready(usize),
    Eof,
    WouldBlock,
}

pub enum WriteResult {
    Ready(usize),
    Closed,
    WouldBlock,
}

struct Dir {
    buf: [u8; LANE_BYTES],
    head: usize,
    len: usize,
    /// The writer of this direction still holds its end.
    writer: bool,
}

impl Dir {
    const fn empty() -> Self {
        Self {
            buf: [0; LANE_BYTES],
            head: 0,
            len: 0,
            writer: true,
        }
    }
}

struct Lane {
    used: bool,
    /// `dir[e]` is written by end `e` and read by the other end.
    dir: [Dir; 2],
}

impl Lane {
    const fn empty() -> Self {
        Self {
            used: false,
            dir: [Dir::empty(), Dir::empty()],
        }
    }
}

static LANES: Mutex<[Lane; LANE_SLOTS]> = Mutex::new([const { Lane::empty() }; LANE_SLOTS]);

/// Allocates one lane. Both ends start open.
pub fn alloc() -> Result<u8, SysError> {
    let mut lanes = LANES.lock();
    let Some(id) = lanes.iter().position(|lane| !lane.used) else {
        return Err(SysError::NoResource);
    };
    // Reset in place. A `Lane` is 32 KiB and does not fit on a kernel stack.
    let lane = &mut lanes[id];
    lane.used = true;
    for dir in &mut lane.dir {
        dir.head = 0;
        dir.len = 0;
        dir.writer = true;
        dir.buf.fill(0);
    }
    Ok(id as u8)
}

pub fn try_read(id: u8, end: u8, dst: &mut [u8]) -> Result<ReadResult, SysError> {
    let mut lanes = LANES.lock();
    let lane = lane_mut(&mut lanes, id)?;
    if end > 1 {
        return Err(SysError::BadValue);
    }
    // End `end` reads what the other end wrote.
    let src = 1 - end as usize;
    let dir = &mut lane.dir[src];
    if dir.len == 0 {
        return Ok(if dir.writer {
            ReadResult::WouldBlock
        } else {
            ReadResult::Eof
        });
    }
    let n = dst.len().min(dir.len);
    copy_out(dir, &mut dst[..n]);
    Ok(ReadResult::Ready(n))
}

pub fn try_write(id: u8, end: u8, src: &[u8]) -> Result<WriteResult, SysError> {
    let mut lanes = LANES.lock();
    let lane = lane_mut(&mut lanes, id)?;
    if end > 1 {
        return Err(SysError::BadValue);
    }
    let reader_open = lane.dir[1 - end as usize].writer;
    let dir = &mut lane.dir[end as usize];
    if !reader_open && dir.len == LANE_BYTES {
        return Ok(WriteResult::Closed);
    }
    if !reader_open && dir.len == 0 {
        return Ok(WriteResult::Closed);
    }
    let room = LANE_BYTES - dir.len;
    if room == 0 {
        return Ok(if reader_open {
            WriteResult::WouldBlock
        } else {
            WriteResult::Closed
        });
    }
    let n = src.len().min(room);
    copy_in(dir, &src[..n]);
    Ok(WriteResult::Ready(n))
}

/// Drops one end. The last close wipes both directions.
pub fn close_end(id: u8, end: u8) {
    let mut lanes = LANES.lock();
    let Ok(lane) = lane_mut(&mut lanes, id) else {
        return;
    };
    if end > 1 {
        return;
    }
    // Closing end `end` stops it writing `dir[end]` and stops it reading
    // the other direction, which is that direction's reader.
    lane.dir[end as usize].writer = false;
    if !lane.dir[0].writer && !lane.dir[1].writer {
        wipe(lane);
    }
}

fn lane_mut(lanes: &mut [Lane; LANE_SLOTS], id: u8) -> Result<&mut Lane, SysError> {
    let lane = lanes.get_mut(id as usize).ok_or(SysError::BadCap)?;
    if !lane.used {
        return Err(SysError::BadCap);
    }
    Ok(lane)
}

fn copy_out(dir: &mut Dir, dst: &mut [u8]) {
    let mut left = dst.len();
    let mut at = 0;
    while left > 0 {
        let pos = (dir.head) % LANE_BYTES;
        let chunk = left.min(LANE_BYTES - pos);
        dst[at..at + chunk].copy_from_slice(&dir.buf[pos..pos + chunk]);
        dir.head = (dir.head + chunk) % LANE_BYTES;
        dir.len -= chunk;
        at += chunk;
        left -= chunk;
    }
}

fn copy_in(dir: &mut Dir, src: &[u8]) {
    let mut left = src.len();
    let mut at = 0;
    let mut pos = (dir.head + dir.len) % LANE_BYTES;
    while left > 0 {
        let chunk = left.min(LANE_BYTES - pos);
        dir.buf[pos..pos + chunk].copy_from_slice(&src[at..at + chunk]);
        pos = (pos + chunk) % LANE_BYTES;
        at += chunk;
        left -= chunk;
    }
    dir.len += src.len();
}

fn wipe(lane: &mut Lane) {
    for dir in &mut lane.dir {
        dir.buf.fill(0);
        dir.head = 0;
        dir.len = 0;
        dir.writer = false;
    }
    lane.used = false;
}
