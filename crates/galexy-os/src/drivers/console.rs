//! The console POLICY: user-facing text output = screen + serial.
//!
//! Not a driver — a thin façade over two of them (screen, serial) fixing
//! the output policy in ONE place: everything printed "to the console"
//! (userland's `write` syscall, the shell's typed-key echo) lands on both
//! the pixel framebuffer and COM1, so output is visible interactively AND
//! observable headless (the boot-test harness reads COM1).

/// Writes one character to the console (screen + serial).
pub fn out_char(c: char) {
    let mut buf = [0u8; 4];
    crate::drivers::screen::out_char(c);
    crate::drivers::serial::write_bytes(c.encode_utf8(&mut buf).as_bytes());
}

/// Writes a string to the console (screen + serial).
pub fn out_str(s: &str) {
    crate::drivers::screen::out_str(s);
    crate::drivers::serial::write_bytes(s.as_bytes());
}
