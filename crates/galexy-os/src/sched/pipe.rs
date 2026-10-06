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

/// Copies available bytes into `dst`. `Ok(0)` is empty (or EOF when no writers).
pub fn read(id: u8, dst: &mut [u8]) -> Result<usize, SysError> {
    let mut pipes = PIPES.lock();
    let pipe = pipes.get_mut(id as usize).ok_or(SysError::BadCap)?;
    if !pipe.used {
        return Err(SysError::BadCap);
    }
    if dst.is_empty() || pipe.len == 0 {
        return Ok(0);
    }
    let n = dst.len().min(pipe.len);
    for (i, slot) in dst.iter_mut().enumerate().take(n) {
        *slot = pipe.data[(pipe.head + i) % PIPE_BYTES];
    }
    pipe.head = (pipe.head + n) % PIPE_BYTES;
    pipe.len -= n;
    Ok(n)
}

/// Appends `src` into the pipe. Short-writes when the buffer fills.
pub fn write(id: u8, src: &[u8]) -> Result<usize, SysError> {
    let mut pipes = PIPES.lock();
    let pipe = pipes.get_mut(id as usize).ok_or(SysError::BadCap)?;
    if !pipe.used {
        return Err(SysError::BadCap);
    }
    if pipe.readers == 0 {
        return Err(SysError::Unsupported);
    }
    if src.is_empty() {
        return Ok(0);
    }
    let space = PIPE_BYTES - pipe.len;
    let n = src.len().min(space);
    for (i, &byte) in src.iter().enumerate().take(n) {
        let at = (pipe.head + pipe.len + i) % PIPE_BYTES;
        pipe.data[at] = byte;
    }
    pipe.len += n;
    Ok(n)
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
