//! The console POLICY: user-facing text output = screen + serial.
//!
//! Not a driver — a thin façade over two of them (screen, serial) fixing
//! the output policy in ONE place: everything printed "to the console"
//! (userland's `write` syscall, the shell's typed-key echo) lands on the
//! pixel framebuffer and, when that TTY is the one on screen, on COM1.
//! Background consoles stay in their cell grids. The screen interprets
//! tab, CR, and CSI; the serial bytes stay raw.

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
pub fn out_str_tty(tty: u8, s: &str) {
    crate::drivers::screen::out_str_tty(tty, s);
    if crate::drivers::screen::shown_tty() == tty {
        crate::drivers::serial::write_bytes(s.as_bytes());
    }
}
