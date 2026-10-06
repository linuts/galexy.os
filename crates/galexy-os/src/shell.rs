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
use crate::sched;

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
/// The foreground program's name while it runs — the prompt stays away
/// until it exits, so its output never lands on an input line. Cleared by
/// [`poll`] once no task with that name is running.
static PENDING: Mutex<Option<String>> = Mutex::new(None);

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
    // Foreground bookkeeping: when the pending program is no longer
    // running (its exit syscall tombstoned it), reclaim the prompt.
    let pending = PENDING.lock().clone();
    if let Some(name) = pending {
        if !sched::is_name_running(&name) {
            *PENDING.lock() = None;
            prompt_only();
        }
    }
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
                // Echo through the console policy: the typed char shows on
                // screen AND lands on COM1 — headless harnesses sync the
                // typing flow on exactly this.
                crate::drivers::console::out_char(c);
            }
        }
    }
}

/// Unknown line: `<line>: command not found`, then the prompt.
fn not_found(text: &str) {
    out_lines(&[alloc::format!("{text}: command not found")]);
}

/// Runs the line as a shell command; unknown lines report not-found.
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
        _ if trimmed.starts_with("run ") || trimmed == "run" => run(trimmed),
        _ => not_found(trimmed),
    }
}

/// Public seam for driving the command dispatcher without keystrokes
/// (boot tests call this directly; `poll` reaches it through the same path
/// the typing flow uses).
pub fn exec(line: &str) {
    flush_and_dispatch(line);
}

/// `run <program>`: loads a user program's ELF from the ramdisk and spawns
/// it. Runs on the main loop = kernel tree (the loader's guard).
fn run(line: &str) {
    let name = line["run".len()..].trim();
    if name.is_empty() {
        out_lines(&["run: no program named (usage: run <program>)".into()]);
        return;
    }
    let Some(bytes) = sched::ramdisk::find(name) else {
        out_lines(&[alloc::format!("run: no such program '{name}'")]);
        return;
    };
    // Close the typed line FIRST — no kernel work between Enter and this
    // newline, so the program's output always starts on a fresh line.
    screen::set_color(TEXT_COLOR);
    screen::out_str("\n");
    let owned = String::from(name);
    // Foreground semantics: spawn inside one IRQ-off step — a pending
    // tick can only serve the rotation's earlier slots (main, demo
    // threads) after the gate, so nothing prints before this. The prompt
    // is NOT reclaimed here: it returns when the program exits (poll).
    x86_64::instructions::interrupts::without_interrupts(|| {
        sched::loader::spawn_program(&owned, bytes);
        *PENDING.lock() = Some(owned);
    });
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
        "commands: help, stats, tasks, threads, run <program>,".into(),
        "clear, about; anything else: command not found".into(),
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
    screen::out_plain(&text);
    screen::set_color(TEXT_COLOR);
    screen::set_pos(cx, cy);
}
