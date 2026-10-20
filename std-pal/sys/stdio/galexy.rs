//! `stdin` / `stdout` / `stderr` over the console, the keyboard, or the
//! pipe Caps a pipeline spawn handed over (`startup::probe_stdio`).
//!
//! stdout and stderr share one sink: the kernel console has a single
//! output stream per TTY, and a pipeline captures both. The console has a
//! per-tick byte budget; a short write means "yield and continue", not
//! "lost", so the writer loops until every byte is accepted.
//!
//! Keyboard stdin is one cooked line: printable bytes are echoed, backspace
//! deletes, left and right move, Enter returns the line, and Ctrl-D on an
//! empty line is EOF. A pipe spawn is raw bytes from that pipe.

use crate::cell::UnsafeCell;
use crate::io;
use crate::sync::atomic::AtomicBool;
use crate::sync::atomic::Ordering::{Acquire, Release};
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
            startup::NO_SLOT => read_keyboard(buf),
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

/// Cooked keyboard line. Longer than a shell prompt, short of a heap alloc
/// on the stdin path.
const LINE_CAP: usize = 256;

struct Cooked {
    edit: [u8; LINE_CAP],
    len: usize,
    pos: usize,
    phase: u8,
    num: u16,
    semi: bool,
    pending: [u8; LINE_CAP + 1],
    pending_len: usize,
    pending_off: usize,
}

impl Cooked {
    const fn new() -> Self {
        Self {
            edit: [0; LINE_CAP],
            len: 0,
            pos: 0,
            phase: 0,
            num: 0,
            semi: false,
            pending: [0; LINE_CAP + 1],
            pending_len: 0,
            pending_off: 0,
        }
    }
}

struct CookedSlot(UnsafeCell<Cooked>);

// SAFETY: `LOCK` makes each `&mut Cooked` exclusive.
unsafe impl Sync for CookedSlot {}

static COOKED: CookedSlot = CookedSlot(UnsafeCell::new(Cooked::new()));
static LOCK: AtomicBool = AtomicBool::new(false);

fn with_cooked<R>(f: impl FnOnce(&mut Cooked) -> R) -> R {
    while LOCK
        .compare_exchange(false, true, Acquire, Acquire)
        .is_err()
    {
        core::hint::spin_loop();
    }
    // SAFETY: the flag is held, so this is the only `&mut`.
    let out = unsafe { f(&mut *COOKED.0.get()) };
    LOCK.store(false, Release);
    out
}

fn read_keyboard(buf: &mut [u8]) -> io::Result<usize> {
    with_cooked(|line| {
        if line.pending_off >= line.pending_len {
            match line.collect()? {
                Collect::Eof => return Ok(0),
                Collect::Ready => {}
            }
        }
        let have = line.pending_len - line.pending_off;
        let n = have.min(buf.len());
        buf[..n].copy_from_slice(&line.pending[line.pending_off..line.pending_off + n]);
        line.pending_off += n;
        if line.pending_off >= line.pending_len {
            line.pending_len = 0;
            line.pending_off = 0;
        }
        Ok(n)
    })
}

enum Collect {
    Eof,
    Ready,
}

enum Act {
    More,
    Submit,
    Eof,
    Backspace,
    Left,
    Right,
    Char(u8),
}

impl Cooked {
    fn collect(&mut self) -> io::Result<Collect> {
        self.len = 0;
        self.pos = 0;
        self.phase = 0;
        loop {
            let Some(byte) = next_key()? else {
                return Ok(Collect::Eof);
            };
            match self.feed(byte) {
                Act::More => {}
                Act::Submit => {
                    let _ = write_all_std(b"\n");
                    self.commit(true);
                    return Ok(Collect::Ready);
                }
                Act::Eof => {
                    if self.len == 0 {
                        return Ok(Collect::Eof);
                    }
                    self.commit(false);
                    return Ok(Collect::Ready);
                }
                Act::Backspace => self.backspace(),
                Act::Left => {
                    if self.pos > 0 {
                        self.pos -= 1;
                        move_cursor(1, b'D');
                    }
                }
                Act::Right => {
                    if self.pos < self.len {
                        self.pos += 1;
                        move_cursor(1, b'C');
                    }
                }
                Act::Char(ch) => self.insert(ch),
            }
        }
    }

    fn commit(&mut self, newline: bool) {
        self.pending[..self.len].copy_from_slice(&self.edit[..self.len]);
        self.pending_len = self.len;
        if newline && self.pending_len < self.pending.len() {
            self.pending[self.pending_len] = b'\n';
            self.pending_len += 1;
        }
        self.pending_off = 0;
        self.len = 0;
        self.pos = 0;
    }

    fn insert(&mut self, ch: u8) {
        if self.len >= LINE_CAP {
            return;
        }
        self.edit.copy_within(self.pos..self.len, self.pos + 1);
        self.edit[self.pos] = ch;
        self.len += 1;
        self.pos += 1;
        let _ = write_all_std(&[ch]);
        let tail = self.len - self.pos;
        if tail == 0 {
            return;
        }
        let _ = write_all_std(&self.edit[self.pos..self.len]);
        move_cursor(tail, b'D');
    }

    fn backspace(&mut self) {
        if self.pos == 0 {
            return;
        }
        self.pos -= 1;
        self.edit.copy_within(self.pos + 1..self.len, self.pos);
        self.len -= 1;
        if self.pos == self.len {
            let _ = write_all_std(b"\x08 \x08");
            return;
        }
        move_cursor(1, b'D');
        let _ = write_all_std(&self.edit[self.pos..self.len]);
        let _ = write_all_std(b" ");
        move_cursor(self.len - self.pos + 1, b'D');
    }

    fn feed(&mut self, byte: u8) -> Act {
        match self.phase {
            1 => {
                if byte == b'[' {
                    self.phase = 2;
                    self.num = 0;
                    self.semi = false;
                    return Act::More;
                }
                if byte == b'O' {
                    self.phase = 3;
                    return Act::More;
                }
                if byte == 0x1b {
                    return Act::More;
                }
                self.phase = 0;
                ground(byte)
            }
            2 => match byte {
                b'0'..=b'9' => {
                    if !self.semi {
                        self.num = self
                            .num
                            .saturating_mul(10)
                            .saturating_add(u16::from(byte - b'0'));
                    }
                    Act::More
                }
                b';' => {
                    self.semi = true;
                    Act::More
                }
                0x1b => {
                    self.phase = 1;
                    Act::More
                }
                0x20..=0x3f => Act::More,
                b'D' => {
                    self.phase = 0;
                    Act::Left
                }
                b'C' => {
                    self.phase = 0;
                    Act::Right
                }
                b'~' if self.num == 3 => {
                    self.phase = 0;
                    Act::Backspace
                }
                b'u' if self.num == 8 || self.num == 127 => {
                    self.phase = 0;
                    Act::Backspace
                }
                _ => {
                    self.phase = 0;
                    Act::More
                }
            },
            3 => {
                self.phase = 0;
                match byte {
                    b'D' => Act::Left,
                    b'C' => Act::Right,
                    _ => Act::More,
                }
            }
            _ => {
                if byte == 0x1b {
                    self.phase = 1;
                    return Act::More;
                }
                ground(byte)
            }
        }
    }
}

fn ground(byte: u8) -> Act {
    match byte {
        b'\n' | b'\r' => Act::Submit,
        0x04 => Act::Eof,
        0x08 | 0x7f => Act::Backspace,
        b if b.is_ascii_graphic() || b == b' ' => Act::Char(b),
        _ => Act::More,
    }
}

fn next_key() -> io::Result<Option<u8>> {
    let cap = syscall::keyboard_cap();
    let mut byte = [0u8; 1];
    loop {
        match syscall::read(cap, &mut byte) {
            Ok(0) => syscall::sleep_ms(1),
            Ok(_) => return Ok(Some(byte[0])),
            Err(SysError::AccessDenied) => return Ok(None),
            Err(err) => return Err(syscall::io_error(err)),
        }
    }
}

fn move_cursor(n: usize, dir: u8) {
    if n == 0 {
        return;
    }
    let mut raw = [0u8; 16];
    raw[0] = 0x1b;
    raw[1] = b'[';
    let mut tmp = [0u8; 8];
    let mut at = tmp.len();
    let mut value = n;
    while value > 0 {
        at -= 1;
        tmp[at] = b'0' + (value % 10) as u8;
        value /= 10;
    }
    let digits = &tmp[at..];
    raw[2..2 + digits.len()].copy_from_slice(digits);
    let end = 2 + digits.len();
    raw[end] = dir;
    let _ = write_all_std(&raw[..=end]);
}

pub fn is_ebadf(err: &io::Error) -> bool {
    // A write with no console grant and no pipe is `AccessDenied`; `print!`
    // treats that as "stdout is closed" and stays silent instead of panicking.
    err.raw_os_error() == Some(SysError::AccessDenied as i32)
}

pub fn panic_output() -> Option<impl io::Write> {
    Some(Stderr::new())
}
