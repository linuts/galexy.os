//! Line editor: CSI keys, history browse, tab completion.
//!
//! BS and DEL are both backspace. Left and right accept `ESC [ C`/`D`,
//! a parameterized form (`ESC [ 1 ; 2 D`), and SS3 (`ESC O D`). The
//! cursor stays inside the buffer, so a backspace cannot slice backwards.

use super::builtins::*;
use super::jobs::*;
use super::state::*;

use core::sync::atomic::{AtomicU16, Ordering};

use galexy_abi::{Cap, SysError};
use galexy_core::{line_backspace, line_delete, line_insert, line_left, line_right, Key, KeyDec};
use galexy_rt::{read, session_locked, write_console, yield_now};

/// One byte that followed a bare Esc, for the next `read_line`.
/// `0x100` means empty. A password prompt treats Esc as cancel, and the
/// key that proved it was not an arrow must still reach the next prompt.
static READ_LINE_REPLAY: AtomicU16 = AtomicU16::new(0x100);

fn stash_key(byte: u8) {
    READ_LINE_REPLAY.store(byte as u16, Ordering::Relaxed);
}

fn take_stashed_key() -> Option<u8> {
    let value = READ_LINE_REPLAY.swap(0x100, Ordering::Relaxed);
    if value == 0x100 {
        None
    } else {
        Some(value as u8)
    }
}

pub(crate) enum LineRead {
    Line(usize),
    /// Esc or Ctrl-C — caller retries or abandons the prompt.
    Cancel,
    /// Paste longer than the buffer — rejected with a message.
    Overlong,
    /// Keyboard capability denied.
    Denied,
}

/// Reads a line. When `secret`, echoes `*` (never cleartext) so the COM1
/// mirror of `write_console` cannot leak the password.
///
/// Left and right move inside the line. Backspace (BS or DEL) deletes
/// the character before the cursor. A bare Esc or Ctrl-C cancels. An
/// arrow sequence starts with Esc and is not a cancel.
pub(crate) fn read_line(kbd: Cap, buf: &mut [u8], secret: bool) -> LineRead {
    wipe(buf);
    let mut len = 0usize;
    let mut pos = 0usize;
    let mut dec = KeyDec::new();
    loop {
        let mut chunk = [0u8; 8];
        let n = if let Some(byte) = take_stashed_key() {
            chunk[0] = byte;
            1
        } else {
            let got = read(kbd, &mut chunk);
            if !got.ok {
                write_console(b"\nread: keyboard denied\n");
                wipe(buf);
                return LineRead::Denied;
            }
            got.value as usize
        };
        for &byte in &chunk[..n.min(chunk.len())] {
            let mut next = Some(byte);
            while let Some(b) = next.take() {
                match dec.feed(b) {
                    Key::More => {}
                    Key::Escape => {
                        if let Some(replay) = dec.take_replay() {
                            stash_key(replay);
                        }
                        write_console(b"\n");
                        wipe(buf);
                        return LineRead::Cancel;
                    }
                    Key::Cancel => {
                        write_console(b"\n");
                        wipe(buf);
                        return LineRead::Cancel;
                    }
                    Key::Submit => {
                        write_console(b"\n");
                        return LineRead::Line(len);
                    }
                    Key::Backspace => {
                        if line_backspace(buf, &mut len, &mut pos) {
                            paint_backspace(buf, len, pos, secret);
                        }
                    }
                    Key::Delete => {
                        if line_delete(buf, &mut len, &mut pos) {
                            paint_delete(buf, len, pos, secret);
                        }
                    }
                    Key::Left => {
                        if line_left(&mut pos, len) {
                            move_left(1);
                        }
                    }
                    Key::Right => {
                        if line_right(&mut pos, len) {
                            move_right(1);
                        }
                    }
                    Key::Home => {
                        move_left(pos);
                        pos = 0;
                    }
                    Key::End => {
                        move_right(len.saturating_sub(pos));
                        pos = len;
                    }
                    Key::Char(ch) if ch.is_ascii_graphic() || (!secret && ch == b' ') => {
                        if !line_insert(buf, &mut len, &mut pos, ch) {
                            write_console(b"\n");
                            write_console(if secret {
                                b"password too long\n"
                            } else {
                                b"input too long\n"
                            });
                            wipe(buf);
                            return LineRead::Overlong;
                        }
                        paint_insert(buf, len, pos, secret);
                    }
                    _ => {}
                }
            }
        }
    }
}

pub(crate) fn repl(
    kbd: Cap,
    cwd: &mut Cwd,
    must_change: &mut bool,
    history: &mut History,
) -> ReplEnd {
    let mut edit = ReplEdit::new();
    let mut dec = KeyDec::new();
    prompt(cwd);
    loop {
        if session_locked() {
            return ReplEnd::Locked;
        }
        let mut buf = [0u8; 8];
        let got = read(kbd, &mut buf);
        if !got.ok {
            if got.value == SysError::Locked as u64 {
                return ReplEnd::Locked;
            }
            write_console(b"\nread: keyboard denied\n");
            return ReplEnd::Die(1);
        }
        let n = got.value as usize;
        if n == 0 {
            // A short keyboard read means try again. Spinning keeps the
            // CPU, so anything else on this seat never runs.
            let _ = yield_now();
            continue;
        }
        for &byte in &buf[..n.min(buf.len())] {
            let mut next = Some(byte);
            while let Some(b) = next.take() {
                let key = dec.feed(b);
                if key == Key::More {
                    continue;
                }
                if key == Key::Escape {
                    next = dec.take_replay();
                    continue;
                }
                if let Some(end) = apply_repl_key(key, kbd, cwd, must_change, history, &mut edit) {
                    return end;
                }
            }
        }
    }
}

struct ReplEdit {
    line: [u8; LINE_MAX],
    len: usize,
    pos: usize,
    draft: [u8; LINE_MAX],
    draft_len: usize,
    hist_idx: Option<usize>,
}

impl ReplEdit {
    fn new() -> Self {
        Self {
            line: [0; LINE_MAX],
            len: 0,
            pos: 0,
            draft: [0; LINE_MAX],
            draft_len: 0,
            hist_idx: None,
        }
    }
}

fn apply_repl_key(
    key: Key,
    kbd: Cap,
    cwd: &mut Cwd,
    must_change: &mut bool,
    history: &mut History,
    edit: &mut ReplEdit,
) -> Option<ReplEnd> {
    let ReplEdit {
        line,
        len,
        pos,
        draft,
        draft_len,
        hist_idx,
    } = edit;
    match key {
        Key::Submit => {
            write_console(b"\n");
            let cmd = trim(&line[..*len]);
            history.push(cmd);
            *hist_idx = None;
            *draft_len = 0;
            if let Some(end) = dispatch(kbd, cmd, cwd, must_change, history) {
                return Some(end);
            }
            *len = 0;
            *pos = 0;
        }
        Key::Backspace => {
            editor_backspace(line, len, pos, draft, draft_len, hist_idx);
        }
        Key::Delete => {
            if line_delete(line, len, pos) {
                paint_delete(line, *len, *pos, false);
                *hist_idx = None;
                draft[..*len].copy_from_slice(&line[..*len]);
                *draft_len = *len;
            }
        }
        Key::Left => {
            if line_left(pos, *len) {
                move_left(1);
            }
        }
        Key::Right => {
            if line_right(pos, *len) {
                move_right(1);
            }
        }
        Key::Up => history_up(history, line, len, pos, draft, draft_len, hist_idx),
        Key::Down => history_down(history, line, len, pos, draft, draft_len, hist_idx),
        Key::Home => {
            move_left(*pos);
            *pos = 0;
        }
        Key::End => {
            move_right((*len).saturating_sub(*pos));
            *pos = *len;
        }
        Key::Other(0x15) => editor_clear(line, len, pos, draft, draft_len, hist_idx),
        Key::Other(0x09) => tab_complete(cwd, line, len, pos, draft, draft_len, hist_idx),
        Key::Char(ch) if ch.is_ascii_graphic() || ch == b' ' => {
            editor_insert(ch, line, len, pos, draft, draft_len, hist_idx);
        }
        _ => {}
    }
    None
}

pub(crate) fn editor_insert(
    byte: u8,
    line: &mut [u8; LINE_MAX],
    len: &mut usize,
    pos: &mut usize,
    draft: &mut [u8; LINE_MAX],
    draft_len: &mut usize,
    hist_idx: &mut Option<usize>,
) {
    if !line_insert(line, len, pos, byte) {
        return;
    }
    paint_insert(line, *len, *pos, false);
    *hist_idx = None;
    draft[..*len].copy_from_slice(&line[..*len]);
    *draft_len = *len;
}

pub(crate) fn editor_backspace(
    line: &mut [u8; LINE_MAX],
    len: &mut usize,
    pos: &mut usize,
    draft: &mut [u8; LINE_MAX],
    draft_len: &mut usize,
    hist_idx: &mut Option<usize>,
) {
    if !line_backspace(line, len, pos) {
        return;
    }
    paint_backspace(line, *len, *pos, false);
    *hist_idx = None;
    draft[..*len].copy_from_slice(&line[..*len]);
    *draft_len = *len;
}

pub(crate) fn editor_clear(
    line: &mut [u8; LINE_MAX],
    len: &mut usize,
    pos: &mut usize,
    draft: &mut [u8; LINE_MAX],
    draft_len: &mut usize,
    hist_idx: &mut Option<usize>,
) {
    if *pos > *len {
        *pos = *len;
    }
    move_left(*pos);
    for _ in 0..*len {
        write_console(b" ");
    }
    move_left(*len);
    *len = 0;
    *pos = 0;
    *draft_len = 0;
    *hist_idx = None;
    line[..LINE_MAX].fill(0);
    draft[..LINE_MAX].fill(0);
}

pub(crate) fn move_left(n: usize) {
    move_csi(n, b'D');
}

pub(crate) fn move_right(n: usize) {
    move_csi(n, b'C');
}

/// The buffer edit has already happened. `pos` is the new cursor.
fn paint_backspace(line: &[u8], len: usize, pos: usize, secret: bool) {
    if pos > len || len > line.len() {
        return;
    }
    if pos == len {
        write_console(&[0x08, b' ', 0x08]);
        return;
    }
    move_left(1);
    paint_span(&line[pos..len], secret);
    write_console(b" ");
    move_left(len - pos + 1);
}

/// `pos` is the index after the inserted byte. The screen cursor is
/// still where that byte was inserted.
fn paint_insert(line: &[u8], len: usize, pos: usize, secret: bool) {
    if pos == 0 || pos > len || len > line.len() {
        return;
    }
    if secret {
        write_console(b"*");
    } else {
        write_console(&[line[pos - 1]]);
    }
    let tail = len - pos;
    if tail == 0 {
        return;
    }
    paint_span(&line[pos..len], secret);
    move_left(tail);
}

/// The character under the cursor is already gone. The screen cursor
/// is still on that cell.
fn paint_delete(line: &[u8], len: usize, pos: usize, secret: bool) {
    if pos > len || len > line.len() {
        return;
    }
    let tail = len - pos;
    if tail > 0 {
        paint_span(&line[pos..len], secret);
    }
    write_console(b" ");
    move_left(tail + 1);
}

fn paint_span(bytes: &[u8], secret: bool) {
    if secret {
        let mut left = bytes.len();
        let stars = [b'*'; 8];
        while left > 0 {
            let n = left.min(stars.len());
            write_console(&stars[..n]);
            left -= n;
        }
        return;
    }
    write_console(bytes);
}

pub(crate) fn move_csi(n: usize, final_byte: u8) {
    if n == 0 {
        return;
    }
    // One write. A lone `ESC [` never leaves the console cutter's CSI
    // state, so the syscall returns 0 and this task retries forever.
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
    raw[2..2 + digits.len()].copy_from_slice(digits);
    let end = 2 + digits.len();
    raw[end] = final_byte;
    write_console(&raw[..=end]);
}

pub(crate) fn history_up(
    history: &History,
    line: &mut [u8; LINE_MAX],
    len: &mut usize,
    pos: &mut usize,
    draft: &mut [u8; LINE_MAX],
    draft_len: &mut usize,
    hist_idx: &mut Option<usize>,
) {
    if history.count == 0 {
        return;
    }
    let next = match *hist_idx {
        None => {
            draft[..*len].copy_from_slice(&line[..*len]);
            *draft_len = *len;
            history.count - 1
        }
        Some(0) => return,
        Some(i) => i - 1,
    };
    if let Some(text) = history.get(next) {
        replace_input_line(line, len, pos, text);
        *hist_idx = Some(next);
    }
}

pub(crate) fn history_down(
    history: &History,
    line: &mut [u8; LINE_MAX],
    len: &mut usize,
    pos: &mut usize,
    draft: &mut [u8; LINE_MAX],
    draft_len: &mut usize,
    hist_idx: &mut Option<usize>,
) {
    let Some(i) = *hist_idx else {
        return;
    };
    if i + 1 < history.count {
        if let Some(text) = history.get(i + 1) {
            replace_input_line(line, len, pos, text);
            *hist_idx = Some(i + 1);
        }
    } else {
        replace_input_line(line, len, pos, &draft[..*draft_len]);
        *hist_idx = None;
    }
}

pub(crate) fn replace_input_line(
    line: &mut [u8; LINE_MAX],
    len: &mut usize,
    pos: &mut usize,
    new: &[u8],
) {
    move_right((*len).saturating_sub(*pos));
    for _ in 0..*len {
        write_console(&[0x08, b' ', 0x08]);
    }
    let n = new.len().min(LINE_MAX);
    line[..n].copy_from_slice(&new[..n]);
    if n < LINE_MAX {
        line[n] = 0;
    }
    *len = n;
    *pos = n;
    if n > 0 {
        write_console(&line[..n]);
    }
}
pub(crate) fn show_history(history: &History) {
    use alloc::string::String;
    use alloc::vec::Vec;
    if history.count == 0 {
        write_console(b"history: none\n");
        return;
    }
    let mut lines: Vec<String> = Vec::new();
    for i in 0..history.count {
        let mut s = String::new();
        s.push_str("  ");
        push_dec(&mut s, (i + 1) as u64);
        s.push_str("  ");
        for &byte in &history.lines[i][..history.lens[i]] {
            s.push(byte as char);
        }
        lines.push(s);
    }
    for line in &lines {
        write_console(line.as_bytes());
        write_console(b"\n");
    }
}

pub(crate) fn tab_complete(
    cwd: &Cwd,
    line: &mut [u8; LINE_MAX],
    len: &mut usize,
    pos: &mut usize,
    draft: &mut [u8; LINE_MAX],
    draft_len: &mut usize,
    hist_idx: &mut Option<usize>,
) {
    if *pos > *len {
        return;
    }
    let mut start = *pos;
    while start > 0 && line[start - 1] != b' ' {
        start -= 1;
    }
    let prefix_len = *pos - start;
    let mut names = [[0u8; 64]; 16];
    let mut nlens = [0usize; 16];
    let mut nmatch = 0usize;
    let snap = crate::state::read_files_snapshot();
    let snap_n = snap.len();
    let mut i = 0usize;
    while i < snap_n && nmatch < names.len() {
        let rest = &snap[i..snap_n];
        let end = rest.iter().position(|b| *b == b'\n').unwrap_or(rest.len());
        if let Some(shown) = cwd_entry(cwd, &rest[..end]) {
            let bare = shown.strip_suffix(b"/").unwrap_or(shown);
            if bare.starts_with(&line[start..start + prefix_len]) && bare.len() <= 64 {
                names[nmatch][..bare.len()].copy_from_slice(bare);
                nlens[nmatch] = bare.len();
                nmatch += 1;
            }
        }
        i += end + 1;
    }
    if nmatch == 0 {
        return;
    }
    let mut common = nlens[0];
    for i in 1..nmatch {
        let mut k = 0usize;
        let limit = common.min(nlens[i]);
        while k < limit && names[0][k] == names[i][k] {
            k += 1;
        }
        common = k;
    }
    if common > prefix_len {
        for &byte in &names[0][prefix_len..common] {
            editor_insert(byte, line, len, pos, draft, draft_len, hist_idx);
        }
    }
    if nmatch == 1 && *len < LINE_MAX && line.get((*pos).saturating_sub(1)) != Some(&b' ') {
        editor_insert(b' ', line, len, pos, draft, draft_len, hist_idx);
        return;
    }
    if nmatch > 1 && common == prefix_len {
        write_console(b"\n");
        for i in 0..nmatch {
            write_console(&names[i][..nlens[i]]);
            write_console(b"\n");
        }
        prompt(cwd);
        if *len > 0 {
            write_console(&line[..*len]);
        }
        move_left((*len).saturating_sub(*pos));
    }
}

pub(crate) fn cwd_entry<'a>(cwd: &Cwd, line: &'a [u8]) -> Option<&'a [u8]> {
    if line.is_empty() {
        return None;
    }
    let dir = &cwd.buf[..cwd.len];
    if dir.is_empty() {
        let slashes = line.iter().filter(|b| **b == b'/').count();
        if slashes == 0 || (slashes == 1 && line.ends_with(b"/")) {
            return Some(line);
        }
        return None;
    }
    if line.len() <= dir.len() + 1 || &line[..dir.len()] != dir || line[dir.len()] != b'/' {
        return None;
    }
    let rest = &line[dir.len() + 1..];
    let slashes = rest.iter().filter(|b| **b == b'/').count();
    if slashes == 0 || (slashes == 1 && rest.ends_with(b"/")) {
        Some(rest)
    } else {
        None
    }
}
