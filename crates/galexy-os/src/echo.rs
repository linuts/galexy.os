//! The galexy.os echo "shell".
//!
//! Characters typed on the keyboard are echoed to the screen as they arrive;
//! Enter flushes the line and echoes it back with a prefix. Backspace edits.

use crate::keyboard;
use crate::screen;
use spin::Mutex;

/// Maximum length of one line, in characters (no heap yet).
const LINE_MAX: usize = 128;

/// Prompt color.
const PROMPT_COLOR: screen::Color = screen::Color::new(0x7C, 0xA0, 0xFF);
/// Normal text color.
const TEXT_COLOR: screen::Color = screen::Color::new(0xE0, 0xE0, 0xE0);

/// Current line under construction.
static LINE: Mutex<LineBuffer> = Mutex::new(LineBuffer::EMPTY);

struct LineBuffer {
    chars: [char; LINE_MAX],
    len: usize,
}

impl LineBuffer {
    const EMPTY: LineBuffer = LineBuffer {
        chars: ['\0'; LINE_MAX],
        len: 0,
    };
}

/// Prints the startup prompt.
pub fn init() {
    screen::set_color(PROMPT_COLOR);
    screen::out_str("galexy> ");
    screen::set_color(TEXT_COLOR);
}

/// Drains queued keys, echoing printable input and handling line editing.
///
/// Called repeatedly from the main loop; hlt() between calls keeps the CPU
/// asleep until the next interrupt.
pub fn poll() {
    let mut line = LINE.lock();
    while let Some(c) = keyboard::pop_key() {
        match c {
            '\n' | '\r' => {
                line.flush_and_echo();
            }
            '\u{0008}' => {
                if line.len > 0 {
                    line.len -= 1;
                    screen::backspace();
                }
            }
            c => {
                if line.len < LINE_MAX {
                    let slot = line.len;
                    line.chars[slot] = c;
                    line.len = slot + 1;
                    screen::out_char(c);
                }
            }
        }
    }
}

impl LineBuffer {
    /// Echoes the buffered line back, then resets it.
    fn flush_and_echo(&mut self) {
        screen::out_str("\n");
        screen::set_color(PROMPT_COLOR);
        screen::out_str("echo: ");
        screen::set_color(TEXT_COLOR);
        for &c in &self.chars[..self.len] {
            screen::out_char(c);
        }
        screen::out_str("\n");
        screen::set_color(PROMPT_COLOR);
        screen::out_str("galexy> ");
        screen::set_color(TEXT_COLOR);
        self.len = 0;
    }
}
