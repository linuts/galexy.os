//! The console POLICY: user-facing text output = screen + serial.
//!
//! Not a driver — a thin façade over two of them (screen, serial) fixing
//! the output policy in ONE place: everything printed "to the console"
//! (userland's `write` syscall, the shell's typed-key echo) lands on the
//! pixel framebuffer and, when that TTY is the one on screen, on COM1.
//! Background consoles stay in their cell grids. The screen interprets
//! tab, CR, and CSI; the serial bytes stay raw. ASCII BEL (`\\x07`)
//! triggers a PC-speaker beep and is not drawn or mirrored to COM1.

/// ASCII bell — console write turns this into [`crate::arch::speaker::beep`].
pub const BEL: u8 = 0x07;

/// The bootloader may have left a partial line on COM1. The first mirror
/// starts its own line so that text is not glued to the passphrase prompt.
static HOST_LINE_DIRTY: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(true);

/// Writes one character to TTY 0 (screen, and COM1 when TTY 0 is visible).
pub fn out_char(c: char) {
    let mut buf = [0u8; 4];
    out_str_tty(0, c.encode_utf8(&mut buf));
}

/// Writes a string to TTY 0 (screen, and COM1 when TTY 0 is visible).
pub fn out_str(s: &str) {
    out_str_tty(0, s);
}

/// Writes `s` to `tty`. COM1 mirrors it only while that TTY is visible.
///
/// A write from another task while this TTY is mid-line is drawn on the
/// next lines, and the open line (prompt and `*` mask) is reprinted.
/// Nothing is dropped.
/// Each BEL runs a short speaker beep and is omitted from the text sinks.
pub fn out_str_tty(tty: u8, s: &str) {
    let bytes = s.as_bytes();
    let mut start = 0usize;
    for (i, b) in bytes.iter().enumerate() {
        if *b != BEL {
            continue;
        }
        if i > start {
            emit_tty(tty, &bytes[start..i]);
        }
        crate::arch::speaker::beep();
        start = i + 1;
    }
    if start < bytes.len() {
        emit_tty(tty, &bytes[start..]);
    }
}

fn emit_tty(tty: u8, bytes: &[u8]) {
    let Ok(text) = core::str::from_utf8(bytes) else {
        return;
    };
    let parked = crate::drivers::screen::out_str_tty_from(tty, text, crate::sched::current_slot());
    if crate::drivers::screen::shown_tty() == tty {
        if HOST_LINE_DIRTY.swap(false, core::sync::atomic::Ordering::Relaxed)
            && !bytes.starts_with(b"\n")
        {
            crate::drivers::serial::write_bytes(b"\n");
        }
        if let Some(line) = parked.as_ref() {
            crate::drivers::serial::write_bytes(b"\n");
            crate::drivers::serial::write_bytes(bytes);
            if !bytes.ends_with(b"\n") {
                crate::drivers::serial::write_bytes(b"\n");
            }
            crate::drivers::serial::write_bytes(line.as_bytes());
        } else {
            crate::drivers::serial::write_bytes(bytes);
        }
        crate::drivers::screen::note_console_output();
    }
}
