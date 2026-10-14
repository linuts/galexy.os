//! Line editing that cannot walk the cursor off the buffer.
//!
//! Backspace is both BS (`0x08`) and DEL (`0x7f`). A host terminal sends
//! DEL; the framebuffer Delete key is DEL; the UART rewrites both to BS
//! before they reach a program, but a program that reads the keyboard
//! capability still sees DEL from the PS/2 path. Arrow keys arrive as
//! `ESC [ A`–`D`, as `ESC [ <params> A`–`D` (the parameter is ignored),
//! or as SS3 `ESC O A`–`D`. `ESC [ 3 ~` deletes the character under the
//! cursor. A half-finished escape is never inserted as text.

/// One decoded key, or [`Key::More`] while an escape sequence is open.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Key {
    /// Printable ASCII, including space.
    Char(u8),
    /// Delete the character before the cursor.
    Backspace,
    /// Delete the character under the cursor.
    Delete,
    /// Move one cell left.
    Left,
    /// Move one cell right.
    Right,
    /// Previous line, or history toward older entries.
    Up,
    /// Next line, or history toward newer entries.
    Down,
    /// Cursor to the start of the line.
    Home,
    /// Cursor to the end of the line.
    End,
    /// Enter.
    Submit,
    /// Ctrl-C.
    Cancel,
    /// Escape that was not a cursor sequence. [`KeyDec::take_replay`]
    /// is the byte that followed, when the caller should handle it.
    Escape,
    /// A control byte this decoder does not treat as editing.
    Other(u8),
    /// The byte was swallowed. The sequence is still open.
    More,
    /// The byte does not change the line.
    Ignore,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    Ground,
    Esc,
    Csi,
    Ss3,
}

/// Byte-at-a-time decoder. State survives a short read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct KeyDec {
    phase: Phase,
    /// First CSI number. Digits after `;` do not change it.
    num: u16,
    any: bool,
    semi: bool,
    replay: Option<u8>,
}

impl KeyDec {
    /// Ground state, nothing to replay.
    pub const fn new() -> Self {
        Self {
            phase: Phase::Ground,
            num: 0,
            any: false,
            semi: false,
            replay: None,
        }
    }

    /// The byte that followed a bare Escape, once.
    pub fn take_replay(&mut self) -> Option<u8> {
        self.replay.take()
    }

    /// Feeds one byte.
    pub fn feed(&mut self, byte: u8) -> Key {
        match self.phase {
            Phase::Ground => self.ground(byte),
            Phase::Esc => self.esc(byte),
            Phase::Csi => self.csi(byte),
            Phase::Ss3 => self.ss3(byte),
        }
    }

    fn ground(&mut self, byte: u8) -> Key {
        match byte {
            0x08 | 0x7f => Key::Backspace,
            b'\n' | b'\r' => Key::Submit,
            0x03 => Key::Cancel,
            0x01 => Key::Home,
            0x05 => Key::End,
            0x1b => {
                self.phase = Phase::Esc;
                Key::More
            }
            b if b.is_ascii_graphic() || b == b' ' => Key::Char(b),
            b if b < 0x20 => Key::Other(b),
            _ => Key::Ignore,
        }
    }

    fn esc(&mut self, byte: u8) -> Key {
        match byte {
            b'[' => {
                self.phase = Phase::Csi;
                self.num = 0;
                self.any = false;
                self.semi = false;
                Key::More
            }
            b'O' => {
                self.phase = Phase::Ss3;
                Key::More
            }
            0x1b => Key::More,
            other => {
                self.phase = Phase::Ground;
                self.replay = Some(other);
                Key::Escape
            }
        }
    }

    fn csi(&mut self, byte: u8) -> Key {
        match byte {
            b'0'..=b'9' => {
                if !self.semi {
                    self.any = true;
                    let digit = (byte - b'0') as u16;
                    self.num = self.num.saturating_mul(10).saturating_add(digit);
                }
                Key::More
            }
            b';' => {
                self.semi = true;
                Key::More
            }
            0x1b => {
                self.phase = Phase::Esc;
                Key::More
            }
            // Intermediate and private bytes stay inside the sequence.
            0x20..=0x2f | 0x3a..=0x3f => Key::More,
            0x40..=0x7e => {
                self.phase = Phase::Ground;
                self.finish_csi(byte)
            }
            _ => {
                self.phase = Phase::Ground;
                Key::Ignore
            }
        }
    }

    fn finish_csi(&mut self, final_byte: u8) -> Key {
        match final_byte {
            b'A' => Key::Up,
            b'B' => Key::Down,
            b'C' => Key::Right,
            b'D' => Key::Left,
            b'H' => Key::Home,
            b'F' => Key::End,
            b'~' => match self.num {
                3 => Key::Delete,
                1 | 7 => Key::Home,
                4 | 8 => Key::End,
                _ => Key::Ignore,
            },
            // CSI u: the first parameter is a code point (kitty / fixterms).
            b'u' => match self.num {
                8 | 127 => Key::Backspace,
                n if (0x20..0x7f).contains(&n) => Key::Char(n as u8),
                _ => Key::Ignore,
            },
            _ => Key::Ignore,
        }
    }

    fn ss3(&mut self, byte: u8) -> Key {
        self.phase = Phase::Ground;
        match byte {
            b'A' => Key::Up,
            b'B' => Key::Down,
            b'C' => Key::Right,
            b'D' => Key::Left,
            b'H' => Key::Home,
            b'F' => Key::End,
            _ => Key::Ignore,
        }
    }
}

impl Default for KeyDec {
    fn default() -> Self {
        Self::new()
    }
}

/// Pulls `len` and `pos` back into `0..=cap` with `pos <= len`.
pub fn line_clamp(cap: usize, len: &mut usize, pos: &mut usize) {
    if *len > cap {
        *len = cap;
    }
    if *pos > *len {
        *pos = *len;
    }
}

/// Inserts `byte` at the cursor. False when the buffer is full.
pub fn line_insert(buf: &mut [u8], len: &mut usize, pos: &mut usize, byte: u8) -> bool {
    line_clamp(buf.len(), len, pos);
    if *len >= buf.len() {
        return false;
    }
    let at = *pos;
    let end = *len;
    let mut i = end;
    while i > at {
        buf[i] = buf[i - 1];
        i -= 1;
    }
    buf[at] = byte;
    *len = end + 1;
    *pos = at + 1;
    true
}

/// Deletes the character before the cursor. False at column 0.
pub fn line_backspace(buf: &mut [u8], len: &mut usize, pos: &mut usize) -> bool {
    line_clamp(buf.len(), len, pos);
    if *pos == 0 || *len == 0 {
        return false;
    }
    let at = *pos - 1;
    let end = *len - 1;
    let mut i = at;
    while i < end {
        buf[i] = buf[i + 1];
        i += 1;
    }
    buf[end] = 0;
    *pos = at;
    *len = end;
    true
}

/// Deletes the character under the cursor. False at the end of the line.
pub fn line_delete(buf: &mut [u8], len: &mut usize, pos: &mut usize) -> bool {
    line_clamp(buf.len(), len, pos);
    if *pos >= *len {
        return false;
    }
    let at = *pos;
    let end = *len - 1;
    let mut i = at;
    while i < end {
        buf[i] = buf[i + 1];
        i += 1;
    }
    buf[end] = 0;
    *len = end;
    true
}

/// One cell left. False at column 0. A cursor past `len` is pulled back first.
pub fn line_left(pos: &mut usize, len: usize) -> bool {
    if *pos > len {
        *pos = len;
    }
    if *pos == 0 {
        return false;
    }
    *pos -= 1;
    true
}

/// One cell right. False at the end of the line.
pub fn line_right(pos: &mut usize, len: usize) -> bool {
    if *pos >= len {
        return false;
    }
    *pos += 1;
    true
}

/// Horizontal cursor motion on a character grid.
///
/// `x == cols` means the cursor is sitting just past the last cell of
/// the row (the next glyph wraps). Moving left from column 0 continues
/// onto the last cell of the previous row. The result stays inside the
/// text area: column `0..=cols` and row `0..rows`. `cols == 0` or
/// `rows == 0` yields `(0, 0)`.
pub fn shift_cursor(x: usize, y: usize, cols: usize, rows: usize, dx: isize) -> (usize, usize) {
    if cols == 0 || rows == 0 {
        return (0, 0);
    }
    let cols_i = cols as isize;
    let rows_i = rows as isize;
    let x = x.min(cols) as isize;
    let y = y.min(rows - 1) as isize;
    let base = y.saturating_mul(cols_i).saturating_add(x);
    let mut pos = base.saturating_add(dx);
    let max = rows_i.saturating_mul(cols_i);
    if pos < 0 {
        pos = 0;
    }
    if pos > max {
        pos = max;
    }
    if pos == max {
        return (cols, rows - 1);
    }
    let pos = pos as usize;
    (pos % cols, pos / cols)
}

/// Cell a backspace erases, and where the cursor then sits.
///
/// `None` at the origin. Otherwise the same step as
/// [`shift_cursor`]`(…, -1)`: the cursor rests on the cleared cell.
pub fn backspace_cell(x: usize, y: usize, cols: usize, rows: usize) -> Option<(usize, usize)> {
    if cols == 0 || rows == 0 {
        return None;
    }
    let x_now = x.min(cols);
    let y_now = y.min(rows - 1);
    if x_now == 0 && y_now == 0 {
        return None;
    }
    Some(shift_cursor(x, y, cols, rows, -1))
}
