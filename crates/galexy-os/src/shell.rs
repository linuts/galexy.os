//! The galexy.os shell: line editing + command dispatch.
//!
//! Characters typed on the keyboard are echoed as they arrive; Enter
//! flushes the line. Lines starting with a known command run it; anything
//! else is echoed back (the original echo-shell behavior). The status bar
//! renders live system stats at the bottom of the screen.

use alloc::string::String;
use alloc::vec::Vec;
use spin::Mutex;

use crate::arch::mm;
use crate::drivers::{keyboard, screen};

/// Prompt color.
pub(crate) const PROMPT_COLOR: screen::Color = screen::Color::new(0x7C, 0xA0, 0xFF);
/// Normal text color.
const TEXT_COLOR: screen::Color = screen::Color::new(0xE0, 0xE0, 0xE0);
/// Command output color.
const VALUE_COLOR: screen::Color = screen::Color::new(0xB0, 0xB0, 0xB0);
/// Status bar background / text colors.
const BAR_BG: screen::Color = screen::Color::new(0x20, 0x28, 0x38);
const BAR_FG: screen::Color = screen::Color::new(0xC8, 0xD0, 0xE0);

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
                flush_and_dispatch(&line);
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

/// Echoes the flushed line (or runs it as a command), then prompts again.
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

/// Runs the line as a shell command; unknown lines are echoed.
fn flush_and_dispatch(text: &str) {
    let trimmed = text.trim();
    match trimmed {
        "" => prompt_only(),
        "help" => help(),
        "stats" => stats(),
        "tasks" => tasks(),
        "threads" => threads(),
        "clear" => {
            screen::clear_screen();
            prompt_only();
        }
        "about" => about(),
        _ => flush_and_echo(text),
    }
}

/// Prints a fresh prompt (for empty lines).
fn prompt_only() {
    screen::set_color(PROMPT_COLOR);
    screen::out_str("galexy> ");
    screen::set_color(TEXT_COLOR);
}

/// Prints colored output lines, then a prompt.
fn out_lines(lines: &[String]) {
    screen::set_color(VALUE_COLOR);
    for line in lines {
        screen::out_str("\n");
        screen::out_str(line);
    }
    screen::out_str("\n");
    screen::set_color(TEXT_COLOR);
    screen::set_color(PROMPT_COLOR);
    screen::out_str("galexy> ");
    screen::set_color(TEXT_COLOR);
}

fn help() {
    out_lines(&[
        "commands: help, stats, tasks, threads, clear, about".into(),
        "unknown lines are echoed back".into(),
    ]);
}

fn stats() {
    let (heap_start, heap_size) = mm::heap::stats();
    out_lines(&[
        alloc::format!("frames free: {}", mm::free_frames()),
        alloc::format!(
            "heap: {} used, {} free of {} KiB",
            mm::heap::used_bytes(),
            mm::heap::free_bytes(),
            heap_size / 1024
        ),
        alloc::format!("heap at {:#x}", heap_start),
    ]);
}

fn tasks() {
    out_lines(&[
        alloc::format!(
            "cooperative tasks: {} active, {} spawned since boot",
            crate::sched::active_tasks(),
            crate::sched::spawned_total()
        ),
        "preemption: timer @ ~1kHz, round-robin incl. main loop".into(),
    ]);
}

fn threads() {
    let mut lines: Vec<String> = Vec::new();
    for (name, ticks) in crate::sched::thread_stats() {
        lines.push(alloc::format!("{}: {} ticks", name, ticks));
    }
    lines.push(alloc::format!(
        "main loop: {} ticks",
        crate::sched::main_ticks()
    ));
    out_lines(&lines);
}

fn about() {
    out_lines(&[
        "galexy.os - a small Rust OS".into(),
        "boot: BIOS/UEFI via the bootloader crate".into(),
        "kernel: framebuffer screen, PS/2 keyboard, PIT timer,".into(),
        "frame allocator, paging, heap, cooperative tasks +".into(),
        "preemptive threads".into(),
    ]);
}

/// Renders the live status bar on the screen's last line.
///
/// IRQ-gated as a whole: this touches the sched table, the heap and the
/// screen while rewriting pixels — none of these locks may be held across a
/// preemption (lock-audit rule, docs/DESIGN.md).
pub fn render_status_bar() {
    use x86_64::instructions::interrupts;
    interrupts::without_interrupts(render_status_bar_inner);
}

fn render_status_bar_inner() {
    let Some((rows, cols)) = screen::terminal_size() else {
        return;
    };
    let last_row = rows - 1;

    // Build the bar text.
    let mut text = String::new();
    for (name, ticks) in crate::sched::thread_stats() {
        text.push_str(&alloc::format!("{} {} | ", name, ticks));
    }
    text.push_str(&alloc::format!("main {} | ", crate::sched::main_ticks()));
    text.push_str(&alloc::format!("frames {}", mm::free_frames()));
    // Truncate to screen width.
    if text.len() > cols {
        text.truncate(cols);
    }

    // Redraw the bar without disturbing the typing cursor.
    let (cx, cy) = screen::pos();
    screen::fill_row(last_row, BAR_BG);
    screen::set_color(BAR_FG);
    screen::set_pos(0, last_row);
    screen::out_str(&text);
    screen::set_color(TEXT_COLOR);
    screen::set_pos(cx, cy);
}
