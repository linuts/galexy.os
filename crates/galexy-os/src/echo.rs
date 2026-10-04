//! The galexy.os echo "shell".
//!
//! Characters typed on the keyboard are echoed to the screen as they arrive;
//! Enter flushes the line and echoes it back with a prefix. Backspace edits.
//! Lines are heap-allocated (kernel heap must be up before `poll` runs).

use alloc::string::String;
use spin::Mutex;

use crate::drivers::{keyboard, screen};

/// Prompt color.
pub(crate) const PROMPT_COLOR: screen::Color = screen::Color::new(0x7C, 0xA0, 0xFF);
/// Normal text color.
const TEXT_COLOR: screen::Color = screen::Color::new(0xE0, 0xE0, 0xE0);

/// Current line under construction.
static LINE: Mutex<String> = Mutex::new(String::new());

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
                flush_and_echo(&line);
                line.clear();
            }
            '\u{0008}' => {
                if line.pop().is_some() {
                    screen::backspace();
                }
            }
            c => {
                line.push(c);
                screen::out_char(c);
            }
        }
    }
}

/// Echoes the flushed line back, then prints a fresh prompt.
fn flush_and_echo(text: &str) {
    screen::set_color(TEXT_COLOR);
    screen::out_str("\n");
    screen::set_color(PROMPT_COLOR);
    screen::out_str("echo: ");
    screen::set_color(TEXT_COLOR);
    screen::out_str(text);
    screen::out_str("\n");
    screen::set_color(PROMPT_COLOR);
    screen::out_str("galexy> ");
    screen::set_color(TEXT_COLOR);
}
