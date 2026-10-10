//! Line editor: CSI keys, history browse, tab completion.

use super::builtins::*;
use super::jobs::*;
use super::state::*;

use galexy_abi::Cap;
use galexy_rt::{files_cap, read, write_console, yield_now};

/// ESC / CSI parser while editing a line (arrow keys → history).
pub(crate) enum KeyParse {
    Normal,
    Esc,
    Csi,
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
pub(crate) fn read_line(kbd: Cap, buf: &mut [u8], secret: bool) -> LineRead {
    wipe(buf);
    let mut len = 0usize;
    loop {
        let mut chunk = [0u8; 8];
        let got = read(kbd, &mut chunk);
        if !got.ok {
            write_console(b"\nread: keyboard denied\n");
            wipe(buf);
            return LineRead::Denied;
        }
        let n = got.value as usize;
        for &byte in &chunk[..n.min(chunk.len())] {
            match byte {
                b'\n' | b'\r' => {
                    write_console(b"\n");
                    return LineRead::Line(len);
                }
                // Esc or Ctrl-C cancels without submitting.
                0x1b | 0x03 => {
                    write_console(b"\n");
                    wipe(buf);
                    return LineRead::Cancel;
                }
                0x08 => {
                    if len > 0 {
                        len -= 1;
                        buf[len] = 0;
                        write_console(&[0x08, b' ', 0x08]);
                    }
                }
                b if b.is_ascii_graphic() || (!secret && b == b' ') => {
                    if len >= buf.len() {
                        write_console(b"\n");
                        write_console(if secret {
                            b"password too long\n"
                        } else {
                            b"input too long\n"
                        });
                        wipe(buf);
                        return LineRead::Overlong;
                    }
                    buf[len] = b;
                    len += 1;
                    if secret {
                        write_console(b"*");
                    } else {
                        write_console(&[b]);
                    }
                }
                _ => {}
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
    let mut line = [0u8; LINE_MAX];
    let mut len = 0usize;
    let mut draft = [0u8; LINE_MAX];
    let mut draft_len = 0usize;
    // Index into history while browsing; `None` means the draft line.
    let mut hist_idx: Option<usize> = None;
    let mut parse = KeyParse::Normal;
    let mut pos = 0usize;
    prompt(cwd);
    loop {
        let mut buf = [0u8; 8];
        let got = read(kbd, &mut buf);
        if !got.ok {
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
            match parse {
                KeyParse::Normal => match byte {
                    b'\n' | b'\r' => {
                        write_console(b"\n");
                        let cmd = trim(&line[..len]);
                        history.push(cmd);
                        hist_idx = None;
                        draft_len = 0;
                        if let Some(end) = dispatch(kbd, cmd, cwd, must_change, history) {
                            return end;
                        }
                        len = 0;
                        pos = 0;
                    }
                    0x1b => parse = KeyParse::Esc,
                    0x08 => editor_backspace(
                        &mut line,
                        &mut len,
                        &mut pos,
                        &mut draft,
                        &mut draft_len,
                        &mut hist_idx,
                    ),
                    0x01 => {
                        move_left(pos);
                        pos = 0;
                    }
                    0x05 => {
                        move_right(len - pos);
                        pos = len;
                    }
                    0x15 => editor_clear(
                        &mut line,
                        &mut len,
                        &mut pos,
                        &mut draft,
                        &mut draft_len,
                        &mut hist_idx,
                    ),
                    0x09 => tab_complete(
                        cwd,
                        &mut line,
                        &mut len,
                        &mut pos,
                        &mut draft,
                        &mut draft_len,
                        &mut hist_idx,
                    ),
                    b if (b.is_ascii_graphic() || b == b' ') && len < LINE_MAX => {
                        editor_insert(
                            b,
                            &mut line,
                            &mut len,
                            &mut pos,
                            &mut draft,
                            &mut draft_len,
                            &mut hist_idx,
                        );
                    }
                    _ => {}
                },
                KeyParse::Esc => {
                    parse = if byte == b'[' {
                        KeyParse::Csi
                    } else {
                        KeyParse::Normal
                    };
                }
                KeyParse::Csi => {
                    parse = KeyParse::Normal;
                    match byte {
                        b'A' => history_up(
                            history,
                            &mut line,
                            &mut len,
                            &mut pos,
                            &mut draft,
                            &mut draft_len,
                            &mut hist_idx,
                        ),
                        b'B' => history_down(
                            history,
                            &mut line,
                            &mut len,
                            &mut pos,
                            &mut draft,
                            &mut draft_len,
                            &mut hist_idx,
                        ),
                        b'D' if pos > 0 => {
                            pos -= 1;
                            move_left(1);
                        }
                        b'C' if pos < len => {
                            pos += 1;
                            move_right(1);
                        }
                        _ => {}
                    }
                }
            }
        }
    }
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
    if *len >= LINE_MAX || *pos > *len {
        return;
    }
    *hist_idx = None;
    for i in (*pos..*len).rev() {
        line[i + 1] = line[i];
    }
    line[*pos] = byte;
    *len += 1;
    write_console(&[byte]);
    if *pos + 1 < *len {
        write_console(&line[*pos + 1..*len]);
        move_left(*len - *pos - 1);
    }
    *pos += 1;
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
    if *pos == 0 || *len == 0 {
        return;
    }
    *pos -= 1;
    for i in *pos..*len - 1 {
        line[i] = line[i + 1];
    }
    *len -= 1;
    line[*len] = 0;
    if *pos == *len {
        write_console(&[0x08, b' ', 0x08]);
    } else {
        move_left(1);
        write_console(&line[*pos..*len]);
        write_console(b" ");
        move_left(*len - *pos + 1);
    }
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

pub(crate) fn move_csi(n: usize, final_byte: u8) {
    if n == 0 {
        return;
    }
    write_console(b"\x1b[");
    write_u64_dec(n as u64);
    write_console(&[final_byte]);
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
    move_right(*len - *pos);
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
    let mut snap = [0u8; 1024];
    let got = read(files_cap(), &mut snap);
    if !got.ok {
        return;
    }
    let snap_n = (got.value as usize).min(snap.len());
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
        move_left(*len - *pos);
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
