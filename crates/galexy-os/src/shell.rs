//! The galexy.os shell: line editing + command dispatch.
//!
//! Characters typed on the keyboard are echoed as they arrive; Enter
//! flushes the line. Left and right move inside the line. Backspace is
//! BS or DEL and deletes the character before the cursor. Lines starting
//! with a known command run it; anything else is echoed back (the
//! original echo-shell behavior). The status bar renders live system
//! stats at the bottom of the screen.

use crate::sync::Mutex;
use alloc::string::String;
use alloc::vec::Vec;

use crate::arch::mm;
use crate::drivers::{console, keyboard, screen};
use crate::sched;
use galexy_core::{line_backspace, line_delete, line_insert, line_left, line_right, Key, KeyDec};

/// Bytes of one in-kernel command line.
const LINE_MAX: usize = 128;

/// Prompt color.
pub(crate) const PROMPT_COLOR: screen::Color = screen::Color::new(0x7C, 0xA0, 0xFF);
/// Normal text color.
const TEXT_COLOR: screen::Color = screen::Color::new(0xE0, 0xE0, 0xE0);
/// Command output color.
const VALUE_COLOR: screen::Color = screen::Color::new(0xB0, 0xB0, 0xB0);
/// Status bar background / text colors.
const BAR_BG: screen::Color = screen::Color::new(0x20, 0x28, 0x38);
const BAR_FG: screen::Color = screen::Color::new(0xC8, 0xD0, 0xE0);

struct EditLine {
    buf: [u8; LINE_MAX],
    len: usize,
    pos: usize,
    keys: KeyDec,
}

impl EditLine {
    const fn new() -> Self {
        Self {
            buf: [0; LINE_MAX],
            len: 0,
            pos: 0,
            keys: KeyDec::new(),
        }
    }
}

/// Current line under construction.
static LINE: Mutex<EditLine> = Mutex::new(EditLine::new());
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
        if !c.is_ascii() {
            continue;
        }
        let mut next = Some(c as u8);
        while let Some(b) = next.take() {
            let key = line.keys.feed(b);
            if key == Key::More {
                continue;
            }
            if key == Key::Escape {
                next = line.keys.take_replay();
                continue;
            }
            screen::set_color(TEXT_COLOR);
            if key == Key::Submit {
                let n = line.len.min(LINE_MAX);
                let mut tmp = [0u8; LINE_MAX];
                tmp[..n].copy_from_slice(&line.buf[..n]);
                line.len = 0;
                line.pos = 0;
                let text = core::str::from_utf8(&tmp[..n]).unwrap_or("");
                // Drop the editor lock before dispatch: a spawn must
                // not re-enter this line.
                drop(line);
                flush_and_dispatch(text);
                return;
            }
            edit_key(&mut line, key);
        }
    }
}

fn edit_key(line: &mut EditLine, key: Key) {
    let EditLine { buf, len, pos, .. } = line;
    match key {
        Key::Backspace => {
            if line_backspace(buf, len, pos) {
                paint_backspace(buf, *len, *pos);
            }
        }
        Key::Delete => {
            if line_delete(buf, len, pos) {
                paint_delete(buf, *len, *pos);
            }
        }
        Key::Left => {
            if line_left(pos, *len) {
                out_move(1, b'D');
            }
        }
        Key::Right => {
            if line_right(pos, *len) {
                out_move(1, b'C');
            }
        }
        Key::Home => {
            out_move(*pos, b'D');
            *pos = 0;
        }
        Key::End => {
            out_move((*len).saturating_sub(*pos), b'C');
            *pos = *len;
        }
        Key::Other(0x15) => {
            out_move(*pos, b'D');
            for _ in 0..*len {
                console::out_str(" ");
            }
            out_move(*len, b'D');
            buf.fill(0);
            *len = 0;
            *pos = 0;
        }
        Key::Char(ch) if ch.is_ascii_graphic() || ch == b' ' => {
            if line_insert(buf, len, pos, ch) {
                paint_insert(buf, *len, *pos);
            }
        }
        _ => {}
    }
}

fn paint_backspace(line: &[u8], len: usize, pos: usize) {
    if pos > len || len > line.len() {
        return;
    }
    if pos == len {
        console::out_str("\u{8} \u{8}");
        return;
    }
    out_move(1, b'D');
    out_bytes(&line[pos..len]);
    console::out_str(" ");
    out_move(len - pos + 1, b'D');
}

fn paint_insert(line: &[u8], len: usize, pos: usize) {
    if pos == 0 || pos > len || len > line.len() {
        return;
    }
    out_bytes(&[line[pos - 1]]);
    let tail = len - pos;
    if tail == 0 {
        return;
    }
    out_bytes(&line[pos..len]);
    out_move(tail, b'D');
}

fn paint_delete(line: &[u8], len: usize, pos: usize) {
    if pos > len || len > line.len() {
        return;
    }
    let tail = len - pos;
    if tail > 0 {
        out_bytes(&line[pos..len]);
    }
    console::out_str(" ");
    out_move(tail + 1, b'D');
}

fn out_bytes(bytes: &[u8]) {
    if let Ok(text) = core::str::from_utf8(bytes) {
        console::out_str(text);
    }
}

fn out_move(n: usize, dir: u8) {
    if n == 0 {
        return;
    }
    let mut raw = [0u8; 16];
    raw[0] = 0x1b;
    raw[1] = b'[';
    let mut tmp = [0u8; 8];
    let mut t = tmp.len();
    let mut v = n;
    while v > 0 && t > 0 {
        t -= 1;
        tmp[t] = b'0' + (v % 10) as u8;
        v /= 10;
    }
    let digits = &tmp[t..];
    if 2 + digits.len() + 1 > raw.len() {
        return;
    }
    raw[2..2 + digits.len()].copy_from_slice(digits);
    let end = 2 + digits.len();
    raw[end] = dir;
    out_bytes(&raw[..=end]);
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
        _ => {
            if !trimmed.contains(' ') && launch(trimmed) {
                return;
            }
            not_found(trimmed);
        }
    }
}

/// Public seam for driving the command dispatcher without keystrokes
/// (boot tests call this directly; `poll` reaches it through the same path
/// the typing flow uses).
pub fn exec(line: &str) {
    flush_and_dispatch(line);
}

/// Starts a ramdisk ELF named by the whole line. A missing name, or a
/// file that is not an ELF, leaves the caller to report not-found.
/// Runs on the main loop, which is the kernel page table.
fn launch(name: &str) -> bool {
    let found = if sched::ramdisk::is_testing_path(name) {
        sched::ramdisk::testing_elf(name)
    } else {
        sched::ramdisk::find(name)
    };
    let Some(bytes) = found else {
        return false;
    };
    if !sched::loader::looks_like_elf(bytes) {
        return false;
    }
    if sched::loader::validate_elf(bytes).is_err() {
        return false;
    }
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
        // Image passed `validate_elf`. A later `Err` is a kernel bug.
        sched::loader::spawn_program(&owned, bytes).expect("validated ramdisk elf");
        *PENDING.lock() = Some(owned);
    });
    true
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
        "a program name on its own starts it".into(),
    ]);
}

fn stats() {
    let (heap_start, heap_size) = mm::heap::stats();
    let ticks = crate::arch::timer_ticks();
    out_lines(&[
        alloc::format!("uptime: {}.{}s", ticks / 1000, (ticks / 100) % 10),
        alloc::format!("frames free: {}", mm::free_frames()),
        alloc::format!(
            "heap: {} used, {} free of {} KiB",
            mm::heap::used_bytes(),
            mm::heap::free_bytes(),
            heap_size / 1024
        ),
        alloc::format!("heap at {:#x}", heap_start),
        alloc::format!(
            "galfs: {} / {} blocks",
            crate::sched::galfs::blocks_used(),
            crate::sched::galfs::BLOCK_SLOTS
        ),
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
        "kernel: framebuffer screen, virtio or PS/2 keyboard, LAPIC timer,".into(),
        "frame allocator, paging, heap, cooperative tasks +".into(),
        "preemptive threads".into(),
    ]);
}

/// Renders the live status bar on the screen's last line, and on COM1.
///
/// IRQ-gated as a whole: this touches the sched table, the heap and the
/// screen while rewriting pixels — none of these locks may be held across a
/// preemption (lock-audit rule, docs/DESIGN.md). The headless terminal
/// gets the same text on its last row; see `screen::draw_status_bar`.
pub fn render_status_bar() {
    use x86_64::instructions::interrupts;
    interrupts::without_interrupts(render_status_bar_inner);
}

fn render_status_bar_inner() {
    let Some((_, cols)) = screen::terminal_size() else {
        return;
    };

    // System strip first (command-center glance), then short thread ticks
    // if the row still has room. Truncate to screen width.
    let ticks = crate::arch::timer_ticks();
    let (_, heap_size) = mm::heap::stats();
    let mut text = alloc::format!(
        "F{} | up {}.{}s | heap {}/{}K | fs {}/{} | tasks {} | frames {}",
        screen::shown_tty() + 1,
        ticks / 1000,
        (ticks / 100) % 10,
        mm::heap::used_bytes() / 1024,
        heap_size / 1024,
        crate::sched::galfs::blocks_used(),
        crate::sched::galfs::BLOCK_SLOTS,
        crate::sched::active_tasks(),
        mm::free_frames(),
    );
    for (name, thread_ticks) in crate::sched::thread_stats() {
        let short = short_thread_name(&name);
        let piece = alloc::format!(" | {short} {thread_ticks}");
        if text.len() + piece.len() > cols {
            break;
        }
        text.push_str(&piece);
    }
    let main_piece = alloc::format!(" | main {}", crate::sched::main_ticks());
    if text.len() + main_piece.len() <= cols {
        text.push_str(&main_piece);
    }
    if text.len() > cols {
        text.truncate(cols);
    }

    // Paint the status row without moving the text cursor — see
    // `screen::draw_status_bar`, which also mirrors this string to COM1.
    // Using set_pos/out_plain here could leave the input cursor on the
    // status row so typed keys never appear above it.
    screen::draw_status_bar(BAR_BG, BAR_FG, &text);
}

/// First character of a demo thread name (`thread-a` → `a`) for a dense bar.
fn short_thread_name(name: &str) -> &str {
    if let Some(rest) = name.strip_prefix("thread-") {
        if !rest.is_empty() {
            return rest;
        }
    }
    name
}
