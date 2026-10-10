//! `stdin` / `stdout` / `stderr` over the console, the keyboard, or the
//! pipe Caps a pipeline spawn handed over (`startup::probe_stdio`).
//!
//! stdout and stderr share one sink: the kernel console has a single
//! output stream per TTY, and a pipeline captures both. The console has a
//! per-tick byte budget; a short write means "yield and continue", not
//! "lost", so the writer loops until every byte is accepted.

use crate::io;
use crate::sys::pal::abi::{CapRights, SysError};
use crate::sys::pal::{startup, syscall};

pub struct Stdin;
pub struct Stdout;
pub type Stderr = Stdout;

impl Stdin {
    pub const fn new() -> Stdin {
        Stdin
    }
}

impl io::Read for Stdin {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        match startup::stdin_slot() {
            startup::NO_SLOT => {
                // Keyboard: `0` means "no key waiting", not EOF. Park a
                // tick between polls so a blocked reader is not a hot loop.
                let cap = syscall::keyboard_cap();
                loop {
                    match syscall::read(cap, buf) {
                        Ok(0) => syscall::sleep_ms(1),
                        Ok(n) => return Ok(n),
                        Err(SysError::AccessDenied) => return Ok(0), // no keyboard grant: EOF
                        Err(e) => return Err(syscall::io_error(e)),
                    }
                }
            }
            slot => {
                let cap = syscall::file_cap(u64::from(slot), CapRights::READ);
                syscall::read(cap, buf).map_err(syscall::io_error)
            }
        }
    }
}

impl Stdout {
    pub const fn new() -> Stdout {
        Stdout
    }
}

/// Writes every byte of `buf` to the stdout sink.
fn write_all_std(buf: &[u8]) -> io::Result<()> {
    let slot = startup::stdout_slot();
    let cap = if slot == startup::NO_SLOT {
        syscall::console_cap()
    } else {
        syscall::file_cap(u64::from(slot), CapRights::WRITE)
    };
    let mut off = 0usize;
    while off < buf.len() {
        match syscall::write(cap, &buf[off..]) {
            Ok(0) => syscall::yield_now(),
            Ok(n) => off += n,
            Err(e) => return Err(syscall::io_error(e)),
        }
    }
    Ok(())
}

impl io::Write for Stdout {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        write_all_std(buf)?;
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

pub const STDIN_BUF_SIZE: usize = 512;

pub fn is_ebadf(err: &io::Error) -> bool {
    // A write with no console grant and no pipe is `AccessDenied`; `print!`
    // treats that as "stdout is closed" and stays silent instead of panicking.
    err.raw_os_error() == Some(SysError::AccessDenied as i32)
}

pub fn panic_output() -> Option<impl io::Write> {
    Some(Stderr::new())
}
