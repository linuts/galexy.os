//! Fixed ring of recent kernel log lines (`dmesg`).
//!
//! [`crate::serial_println!`] appends one line here, then writes COM1.
//! The ring is the operator view: a query cap copies the newest lines
//! that fit in the caller's buffer. Consecutive `login fail` lines
//! collapse so a lockout storm cannot push faults out of the ring.
//!
//! Lock: `DMESG` is taken only to copy a line in or out. It is never
//! held while acquiring `THREADS`, the screen, or the galfs table.
//! `serial_println!` may run while those locks are already held, so
//! `DMESG` sits at the bottom of the order.

use core::fmt;

use crate::sync::Mutex;

/// Lines kept. Older lines fall off the front.
const SLOTS: usize = 32;
/// Bytes stored per line. Longer lines are cut on a UTF-8 boundary.
const WIDTH: usize = 96;

struct Slot {
    len: u8,
    bytes: [u8; WIDTH],
}

impl Slot {
    const EMPTY: Self = Self {
        len: 0,
        bytes: [0; WIDTH],
    };

    fn text(&self) -> &str {
        core::str::from_utf8(&self.bytes[..self.len as usize]).unwrap_or("")
    }
}

struct Log {
    slots: [Slot; SLOTS],
    /// Next write index.
    next: usize,
    filled: usize,
}

impl Log {
    const fn new() -> Self {
        Self {
            slots: [Slot::EMPTY; SLOTS],
            next: 0,
            filled: 0,
        }
    }

    fn last_text(&self) -> &str {
        if self.filled == 0 {
            return "";
        }
        let i = if self.next == 0 {
            SLOTS - 1
        } else {
            self.next - 1
        };
        self.slots[i].text()
    }

    fn push(&mut self, line: &str) {
        if login_fail(line) && login_fail(self.last_text()) {
            return;
        }
        let mut n = line.len().min(WIDTH);
        while n > 0 && !line.is_char_boundary(n) {
            n -= 1;
        }
        let slot = &mut self.slots[self.next];
        slot.bytes[..n].copy_from_slice(&line.as_bytes()[..n]);
        slot.len = n as u8;
        self.next = (self.next + 1) % SLOTS;
        if self.filled < SLOTS {
            self.filled += 1;
        }
    }

    /// Oldest-to-newest, newest window that fits in `dst`, newline separated.
    fn copy_out(&self, dst: &mut [u8]) -> usize {
        if self.filled == 0 || dst.is_empty() {
            return 0;
        }
        let start = if self.filled == SLOTS { self.next } else { 0 };
        let mut begin = 0usize;
        loop {
            let mut need = 0usize;
            for i in begin..self.filled {
                let slot = &self.slots[(start + i) % SLOTS];
                need = need.saturating_add(slot.len as usize).saturating_add(1);
            }
            if need <= dst.len() || begin + 1 >= self.filled {
                break;
            }
            begin += 1;
        }
        let mut n = 0usize;
        for i in begin..self.filled {
            let slot = &self.slots[(start + i) % SLOTS];
            let len = slot.len as usize;
            let room = dst.len().saturating_sub(n);
            let take = len.min(room);
            dst[n..n + take].copy_from_slice(&slot.bytes[..take]);
            n += take;
            if n < dst.len() {
                dst[n] = b'\n';
                n += 1;
            }
        }
        n
    }
}

fn login_fail(s: &str) -> bool {
    s.contains("login fail")
}

static DMESG: Mutex<Log> = Mutex::new(Log::new());

/// Panic path only: the `[PANIC]` line goes through [`record`], which
/// takes `DMESG`; a panic raised under that lock would otherwise hang the
/// handler silently. See `serial::force_unlock_for_panic`.
///
/// # Safety
/// Only from the panic handler, which never returns to the holder.
pub unsafe fn force_unlock_for_panic() {
    // SAFETY: caller contract above.
    unsafe { DMESG.force_unlock() };
}

struct LineWriter<'a> {
    buf: &'a mut [u8],
    n: usize,
}

impl fmt::Write for LineWriter<'_> {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        let rest = &mut self.buf[self.n..];
        let mut take = s.len().min(rest.len());
        while take > 0 && !s.is_char_boundary(take) {
            take -= 1;
        }
        rest[..take].copy_from_slice(&s.as_bytes()[..take]);
        self.n += take;
        Ok(())
    }
}

/// Appends one formatted log line. Called from [`crate::serial_println!`].
pub fn record(args: fmt::Arguments) {
    let mut raw = [0u8; WIDTH];
    let n = {
        let mut w = LineWriter {
            buf: &mut raw,
            n: 0,
        };
        let _ = fmt::Write::write_fmt(&mut w, args);
        w.n
    };
    let line = core::str::from_utf8(&raw[..n]).unwrap_or("");
    x86_64::instructions::interrupts::without_interrupts(|| {
        DMESG.lock().push(line);
    });
}

/// Copies the newest lines that fit into `dst`. Returns the byte count.
pub fn snapshot(dst: &mut [u8]) -> usize {
    x86_64::instructions::interrupts::without_interrupts(|| DMESG.lock().copy_out(dst))
}
