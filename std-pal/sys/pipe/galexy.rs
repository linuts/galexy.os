//! Anonymous pipes over `Syscall::Pipe`. One end is a READ Cap, the other
//! a WRITE Cap, both in the per-task file table. `Read` parks until bytes
//! arrive or every writer is gone (then `0`); `Write` parks while the ring
//! is full. A pipe end moves to a child through `Spawn`'s
//! `SPAWN_WITH_CAPS`, which is how `Command` wires stdio.

use crate::io::{self, BorrowedCursor, IoSlice, IoSliceMut};
use crate::sys::pal::abi::Cap;
use crate::sys::pal::syscall;
use crate::{fmt, mem};

pub struct Pipe {
    cap: Cap,
}

#[inline]
pub fn pipe() -> io::Result<(Pipe, Pipe)> {
    let (read, write) = syscall::pipe().map_err(syscall::io_error)?;
    Ok((Pipe { cap: read }, Pipe { cap: write }))
}

impl Pipe {
    /// Gives the Cap up without closing it (it moved into a child).
    pub(crate) fn into_cap(self) -> Cap {
        let cap = self.cap;
        mem::forget(self);
        cap
    }

    pub fn try_clone(&self) -> io::Result<Self> {
        // No `dup`: a Cap is one table slot. Share the `Pipe` instead.
        crate::sys::pal::unsupported()
    }

    pub fn read(&self, buf: &mut [u8]) -> io::Result<usize> {
        syscall::read(self.cap, buf).map_err(syscall::io_error)
    }

    pub fn read_buf(&self, buf: BorrowedCursor<'_, u8>) -> io::Result<()> {
        io::default_read_buf(|b| self.read(b), buf)
    }

    pub fn read_vectored(&self, bufs: &mut [IoSliceMut<'_>]) -> io::Result<usize> {
        io::default_read_vectored(|b| self.read(b), bufs)
    }

    pub fn is_read_vectored(&self) -> bool {
        false
    }

    pub fn read_to_end(&self, buf: &mut Vec<u8>) -> io::Result<usize> {
        let mut total = 0usize;
        let mut chunk = [0u8; 512];
        loop {
            let n = self.read(&mut chunk)?;
            if n == 0 {
                return Ok(total);
            }
            buf.extend_from_slice(&chunk[..n]);
            total += n;
        }
    }

    pub fn write(&self, buf: &[u8]) -> io::Result<usize> {
        syscall::write(self.cap, buf).map_err(syscall::io_error)
    }

    pub fn write_vectored(&self, bufs: &[IoSlice<'_>]) -> io::Result<usize> {
        io::default_write_vectored(|b| self.write(b), bufs)
    }

    pub fn is_write_vectored(&self) -> bool {
        false
    }
}

impl Drop for Pipe {
    fn drop(&mut self) {
        let _ = syscall::close(self.cap);
    }
}

impl fmt::Debug for Pipe {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Pipe").field("cap", &self.cap.bits()).finish()
    }
}
