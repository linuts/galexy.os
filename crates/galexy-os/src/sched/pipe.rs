//! Anonymous pipes: fixed ring buffers shared by open ends.

use spin::Mutex;

use galexy_abi::SysError;

/// Pipes the kernel will hold.
pub const PIPE_SLOTS: usize = 8;
/// Bytes one pipe buffer holds.
pub const PIPE_BYTES: usize = 256;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum PipeEnd {
    Read,
    Write,
}

/// Result of a non-blocking pipe read attempt.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReadResult {
    /// Copied `n` bytes (`n > 0`).
    Ready(usize),
    /// Empty but writers remain — caller should park.
    WouldBlock,
    /// Empty and no writers — EOF.
    Eof,
}

/// Result of a non-blocking pipe write attempt.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WriteResult {
    /// Copied `n` bytes (`n > 0`), or `0` for an empty source.
    Ready(usize),
    /// Full but readers remain — caller should park.
    WouldBlock,
    /// No readers left.
    Closed,
}

struct Pipe {
    used: bool,
    data: [u8; PIPE_BYTES],
    head: usize,
    len: usize,
    readers: u8,
    writers: u8,
}

impl Pipe {
    const fn empty() -> Self {
        Self {
            used: false,
            data: [0; PIPE_BYTES],
            head: 0,
            len: 0,
            readers: 0,
            writers: 0,
        }
    }
}

static PIPES: Mutex<[Pipe; PIPE_SLOTS]> = Mutex::new([const { Pipe::empty() }; PIPE_SLOTS]);

/// Allocates a pipe and marks one reader and one writer open.
pub fn alloc() -> Result<u8, SysError> {
    let mut pipes = PIPES.lock();
    let Some(id) = pipes.iter().position(|p| !p.used) else {
        return Err(SysError::NoResource);
    };
    pipes[id] = Pipe {
        used: true,
        data: [0; PIPE_BYTES],
        head: 0,
        len: 0,
        readers: 1,
        writers: 1,
    };
    Ok(id as u8)
}

/// Copies available bytes into `dst`. Prefer [`try_read`] for blocking.
pub fn read(id: u8, dst: &mut [u8]) -> Result<usize, SysError> {
    match try_read(id, dst)? {
        ReadResult::Ready(n) => Ok(n),
        ReadResult::WouldBlock | ReadResult::Eof => Ok(0),
    }
}

/// Non-blocking read with WouldBlock / EOF distinction (Milestone 57).
pub fn try_read(id: u8, dst: &mut [u8]) -> Result<ReadResult, SysError> {
    let mut pipes = PIPES.lock();
    let pipe = pipes.get_mut(id as usize).ok_or(SysError::BadCap)?;
    if !pipe.used {
        return Err(SysError::BadCap);
    }
    if dst.is_empty() {
        return Ok(ReadResult::Ready(0));
    }
    if pipe.len == 0 {
        return Ok(if pipe.writers == 0 {
            ReadResult::Eof
        } else {
            ReadResult::WouldBlock
        });
    }
    let n = dst.len().min(pipe.len);
    for (i, slot) in dst.iter_mut().enumerate().take(n) {
        *slot = pipe.data[(pipe.head + i) % PIPE_BYTES];
    }
    pipe.head = (pipe.head + n) % PIPE_BYTES;
    pipe.len -= n;
    Ok(ReadResult::Ready(n))
}

/// Appends `src` into the pipe. Prefer [`try_write`] for blocking.
pub fn write(id: u8, src: &[u8]) -> Result<usize, SysError> {
    match try_write(id, src)? {
        WriteResult::Ready(n) => Ok(n),
        WriteResult::WouldBlock => Ok(0),
        WriteResult::Closed => Err(SysError::Unsupported),
    }
}

/// Non-blocking write with WouldBlock / Closed distinction (Milestone 57).
pub fn try_write(id: u8, src: &[u8]) -> Result<WriteResult, SysError> {
    let mut pipes = PIPES.lock();
    let pipe = pipes.get_mut(id as usize).ok_or(SysError::BadCap)?;
    if !pipe.used {
        return Err(SysError::BadCap);
    }
    if pipe.readers == 0 {
        return Ok(WriteResult::Closed);
    }
    if src.is_empty() {
        return Ok(WriteResult::Ready(0));
    }
    let space = PIPE_BYTES - pipe.len;
    if space == 0 {
        return Ok(WriteResult::WouldBlock);
    }
    let n = src.len().min(space);
    for (i, &byte) in src.iter().enumerate().take(n) {
        let at = (pipe.head + pipe.len + i) % PIPE_BYTES;
        pipe.data[at] = byte;
    }
    pipe.len += n;
    Ok(WriteResult::Ready(n))
}

/// Drops one open end. Frees the pipe when both sides are gone.
pub fn close_end(id: u8, end: PipeEnd) {
    let mut pipes = PIPES.lock();
    let Some(pipe) = pipes.get_mut(id as usize) else {
        return;
    };
    if !pipe.used {
        return;
    }
    match end {
        PipeEnd::Read => pipe.readers = pipe.readers.saturating_sub(1),
        PipeEnd::Write => pipe.writers = pipe.writers.saturating_sub(1),
    }
    if pipe.readers == 0 && pipe.writers == 0 {
        *pipe = Pipe::empty();
    }
}
