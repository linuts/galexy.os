//! nano — a screen editor in the shape of GNU nano.
//!
//! Trust: the shell spawns this with `SPAWN_INHERIT` (the caller's galfs
//! cards) and `SPAWN_GRANT_KEYBOARD`, then Cap-waits. The ramdisk is
//! trusted code. The buffer lives in this image (no user heap) and holds
//! one text file. Ctrl-O writes it back, Ctrl-X leaves. Ctrl-C is the
//! seat's foreground kill and never reaches this loop.
//!
//! `.rs` and `.py` use the in-process language service: colors, one
//! diagnostic on the status line, and Ctrl-T completion. The image does
//! not carry rust-analyzer or a Python language server.

#![no_std]
#![no_main]

use core::cell::UnsafeCell;

use galexy_abi::{Cap, CapRights, SysError};
use galexy_core::{
    complete, diagnose, lang_of, paint_line, Compl, HiState, Key, KeyDec, Lang, Span, COMPL_LEN,
};
use galexy_rt::{
    arg, close, create_replace, entry, keyboard_cap, open, read, write, write_console, yield_now,
};

entry!(main);

/// Bytes of one file. Matches the default actor quota so a normal note fits.
const CAP: usize = 16 * 1024;
/// One cut line, including its newline.
const CUT: usize = 1024;
/// Columns of the line-number gutter, including the bar (`   1|`).
const GUTTER: usize = 5;
/// Columns of file text. With the gutter this stays inside a normal
/// terminal so a CSI row does not wrap under the cursor math.
const TEXT_COLS: usize = 64;
/// Title, status, and shortcut rows are this wide.
const WIDTH: usize = GUTTER + TEXT_COLS;
/// Text rows between the title and the status line.
const VIEW_ROWS: usize = 12;
/// Rows [`move_vert`] jumps for Ctrl-Y / Ctrl-V. Same count as [`VIEW_ROWS`].
const PAGE: isize = 12;

struct Scratch {
    doc: UnsafeCell<[u8; CAP]>,
    cut: UnsafeCell<[u8; CUT]>,
}

// SAFETY: one task owns this image. `main` is the only reader and writer,
// and the loader gives that task its own copy of these bytes.
unsafe impl Sync for Scratch {}

static SCRATCH: Scratch = Scratch {
    doc: UnsafeCell::new([0; CAP]),
    cut: UnsafeCell::new([0; CUT]),
};

struct Editor {
    len: usize,
    cursor: usize,
    /// Column to land on when moving up or down.
    goal: usize,
    top: usize,
    dirty: bool,
    readonly: bool,
    quit_armed: bool,
    cut_len: usize,
    /// Ctrl-G replaced the text area with the key list. The next key
    /// closes it and is not inserted.
    help: bool,
    note: Note,
    lang: Lang,
    msg: [u8; 48],
    msg_len: usize,
    compl_on: bool,
    prefix: [u8; COMPL_LEN],
    prefix_len: usize,
    compl_i: usize,
}

enum Note {
    None,
    New,
    Wrote(usize),
    ReadOnly,
    Unsaved,
    Full,
    Cut,
    Paste,
    Failed,
    /// Text in [`Editor::msg`].
    Msg,
}

enum Step {
    Stay,
    Redraw,
    Quit,
}

fn main() -> i32 {
    let path = arg();
    if path.is_empty() {
        write_console(b"nano: usage: nano <path>\n");
        return 1;
    }
    // SAFETY: this task never shares `SCRATCH`, and `main` does not
    // re-enter. The two slices are disjoint fields.
    let doc = unsafe { &mut *SCRATCH.doc.get() };
    let cut = unsafe { &mut *SCRATCH.cut.get() };
    let Some(loaded) = load(doc, path) else {
        return 1;
    };
    let mut ed = Editor {
        len: loaded.len,
        cursor: 0,
        goal: 0,
        top: 0,
        dirty: false,
        readonly: loaded.readonly,
        quit_armed: false,
        cut_len: 0,
        help: false,
        note: if loaded.new_file {
            Note::New
        } else {
            Note::None
        },
        lang: lang_of(path),
        msg: [0; 48],
        msg_len: 0,
        compl_on: false,
        prefix: [0; COMPL_LEN],
        prefix_len: 0,
        compl_i: 0,
    };
    write_console(b"\x1b[0m\x1b[2J");
    draw(doc, path, &mut ed);
    let mut dec = KeyDec::new();
    loop {
        let mut buf = [0u8; 8];
        let got = read(keyboard_cap(), &mut buf);
        if !got.ok {
            leave_screen();
            write_console(b"nano: keyboard denied\n");
            return 1;
        }
        if got.value == 0 {
            let _ = yield_now();
            continue;
        }
        let n = (got.value as usize).min(buf.len());
        // One key closes help. An arrow is several bytes, so the whole
        // sequence is that one key. A later key batched into the same
        // read (the queue fills while this task is off-CPU) still runs.
        let mut redraw = false;
        for &byte in &buf[..n] {
            let mut next = Some(byte);
            while let Some(b) = next.take() {
                let key = dec.feed(b);
                if key == Key::More {
                    continue;
                }
                if key == Key::Escape {
                    if ed.help {
                        let _ = dec.take_replay();
                    } else {
                        next = dec.take_replay();
                        continue;
                    }
                }
                if ed.help {
                    ed.help = false;
                    redraw = true;
                    continue;
                }
                match on_key(doc, cut, path, &mut ed, key) {
                    Step::Stay => {}
                    Step::Redraw => redraw = true,
                    Step::Quit => {
                        leave_screen();
                        return 0;
                    }
                }
            }
        }
        if redraw {
            draw(doc, path, &mut ed);
        }
    }
}

struct Loaded {
    len: usize,
    readonly: bool,
    new_file: bool,
}

fn load(doc: &mut [u8], path: &[u8]) -> Option<Loaded> {
    let opened = open(path);
    if !opened.ok {
        if opened.value == SysError::NotFound as u64 {
            return Some(Loaded {
                len: 0,
                readonly: false,
                new_file: true,
            });
        }
        if opened.value == SysError::Unsupported as u64 {
            write_console(b"nano: is a directory\n");
        } else if opened.value == SysError::AccessDenied as u64 {
            write_console(b"nano: denied\n");
        } else {
            write_console(b"nano: failed\n");
        }
        return None;
    }
    let cap = Cap::from_bits(opened.value);
    let readonly = !cap.rights().contains(CapRights::WRITE);
    let mut len = 0usize;
    let mut chunk = [0u8; 256];
    loop {
        let got = read(cap, &mut chunk);
        if !got.ok {
            let _ = close(cap);
            write_console(b"nano: read failed\n");
            return None;
        }
        if got.value == 0 {
            break;
        }
        let n = (got.value as usize).min(chunk.len());
        for &byte in &chunk[..n] {
            if !take_byte(doc, &mut len, byte) {
                let _ = close(cap);
                return None;
            }
        }
    }
    let _ = close(cap);
    Some(Loaded {
        len,
        readonly,
        new_file: false,
    })
}

/// Appends one file byte. A tab becomes four spaces. A byte that is not
/// text prints an error and returns false.
fn take_byte(doc: &mut [u8], len: &mut usize, byte: u8) -> bool {
    if byte == b'\t' {
        return push(doc, len, b"    ");
    }
    if byte.is_ascii_graphic() || byte == b' ' || byte == b'\n' {
        return push(doc, len, &[byte]);
    }
    write_console(b"nano: not text\n");
    false
}

fn push(doc: &mut [u8], len: &mut usize, bytes: &[u8]) -> bool {
    if len.saturating_add(bytes.len()) > CAP {
        write_console(b"nano: file too big\n");
        return false;
    }
    doc[*len..*len + bytes.len()].copy_from_slice(bytes);
    *len += bytes.len();
    true
}

fn on_key(doc: &mut [u8], cut: &mut [u8], path: &[u8], ed: &mut Editor, key: Key) -> Step {
    if !matches!(key, Key::Other(0x14)) {
        ed.compl_on = false;
    }
    match key {
        Key::Left => {
            move_left(doc, ed);
            Step::Redraw
        }
        Key::Right => {
            move_right(doc, ed);
            Step::Redraw
        }
        Key::Up => {
            move_vert(doc, ed, -1);
            Step::Redraw
        }
        Key::Down => {
            move_vert(doc, ed, 1);
            Step::Redraw
        }
        Key::Home => {
            ed.quit_armed = false;
            ed.cursor = line_start(doc, ed.len, cursor_line(doc, ed));
            sync_goal(doc, ed);
            Step::Redraw
        }
        Key::End => {
            ed.quit_armed = false;
            let start = line_start(doc, ed.len, cursor_line(doc, ed));
            ed.cursor = start + line_len(doc, ed.len, start);
            sync_goal(doc, ed);
            Step::Redraw
        }
        Key::Backspace => {
            backspace(doc, ed);
            Step::Redraw
        }
        Key::Delete | Key::Other(0x04) => {
            delete_at(doc, ed);
            Step::Redraw
        }
        Key::Submit => {
            insert(doc, ed, b'\n');
            Step::Redraw
        }
        Key::Char(b) if b.is_ascii_graphic() || b == b' ' => {
            insert(doc, ed, b);
            Step::Redraw
        }
        Key::Other(0x18) => {
            if ed.dirty && !ed.quit_armed {
                ed.quit_armed = true;
                ed.note = Note::Unsaved;
                Step::Redraw
            } else {
                Step::Quit
            }
        }
        Key::Other(0x0f) => {
            save(doc, path, ed);
            Step::Redraw
        }
        Key::Other(0x0b) => {
            cut_line(doc, cut, ed);
            Step::Redraw
        }
        Key::Other(0x15) => {
            paste(doc, cut, ed);
            Step::Redraw
        }
        Key::Other(0x07) => {
            ed.quit_armed = false;
            ed.help = true;
            Step::Redraw
        }
        Key::Other(0x14) => complete_word(doc, ed),
        Key::Other(0x19) => {
            move_vert(doc, ed, -PAGE);
            Step::Redraw
        }
        Key::Other(0x16) => {
            move_vert(doc, ed, PAGE);
            Step::Redraw
        }
        _ => Step::Stay,
    }
}

fn save(doc: &[u8], path: &[u8], ed: &mut Editor) {
    ed.quit_armed = false;
    if ed.readonly {
        ed.note = Note::ReadOnly;
        return;
    }
    let created = create_replace(path);
    if !created.ok || created.value == 0 {
        ed.note = Note::Failed;
        return;
    }
    let cap = Cap::from_bits(created.value);
    if !write_all(cap, &doc[..ed.len]) {
        let _ = close(cap);
        ed.note = Note::Failed;
        return;
    }
    let _ = close(cap);
    ed.dirty = false;
    ed.note = Note::Wrote(ed.len);
}

fn write_all(cap: Cap, bytes: &[u8]) -> bool {
    let mut off = 0usize;
    while off < bytes.len() {
        let n = (bytes.len() - off).min(256);
        let wrote = write(cap, &bytes[off..off + n]);
        if !wrote.ok || wrote.value != n as u64 {
            return false;
        }
        off += n;
    }
    true
}

fn insert(doc: &mut [u8], ed: &mut Editor, byte: u8) {
    if ed.len >= CAP {
        ed.quit_armed = false;
        ed.note = Note::Full;
        return;
    }
    doc.copy_within(ed.cursor..ed.len, ed.cursor + 1);
    doc[ed.cursor] = byte;
    ed.len += 1;
    ed.cursor += 1;
    mark_dirty(ed);
    sync_goal(doc, ed);
}

fn backspace(doc: &mut [u8], ed: &mut Editor) {
    if ed.cursor == 0 {
        ed.quit_armed = false;
        return;
    }
    ed.cursor -= 1;
    delete_at(doc, ed);
}

fn delete_at(doc: &mut [u8], ed: &mut Editor) {
    ed.quit_armed = false;
    if ed.cursor >= ed.len {
        return;
    }
    doc.copy_within(ed.cursor + 1..ed.len, ed.cursor);
    ed.len -= 1;
    mark_dirty(ed);
    sync_goal(doc, ed);
}

fn cut_line(doc: &mut [u8], cut: &mut [u8], ed: &mut Editor) {
    ed.quit_armed = false;
    let start = line_start(doc, ed.len, cursor_line(doc, ed));
    let mut end = start + line_len(doc, ed.len, start);
    if end < ed.len && doc[end] == b'\n' {
        end += 1;
    }
    let n = end - start;
    if n == 0 {
        return;
    }
    if n > CUT {
        ed.note = Note::Full;
        return;
    }
    cut[..n].copy_from_slice(&doc[start..end]);
    ed.cut_len = n;
    doc.copy_within(end..ed.len, start);
    ed.len -= n;
    ed.cursor = start.min(ed.len);
    mark_dirty(ed);
    ed.note = Note::Cut;
    sync_goal(doc, ed);
}

fn paste(doc: &mut [u8], cut: &[u8], ed: &mut Editor) {
    ed.quit_armed = false;
    let n = ed.cut_len;
    if n == 0 {
        return;
    }
    if ed.len.saturating_add(n) > CAP {
        ed.note = Note::Full;
        return;
    }
    doc.copy_within(ed.cursor..ed.len, ed.cursor + n);
    doc[ed.cursor..ed.cursor + n].copy_from_slice(&cut[..n]);
    ed.len += n;
    ed.cursor += n;
    mark_dirty(ed);
    ed.note = Note::Paste;
    sync_goal(doc, ed);
}

fn move_left(doc: &[u8], ed: &mut Editor) {
    ed.quit_armed = false;
    if ed.cursor > 0 {
        ed.cursor -= 1;
    }
    sync_goal(doc, ed);
}

fn move_right(doc: &[u8], ed: &mut Editor) {
    ed.quit_armed = false;
    if ed.cursor < ed.len {
        ed.cursor += 1;
    }
    sync_goal(doc, ed);
}

fn move_vert(doc: &[u8], ed: &mut Editor, delta: isize) {
    ed.quit_armed = false;
    let line = cursor_line(doc, ed);
    let last = last_line(&doc[..ed.len]);
    let next = if delta < 0 {
        line.saturating_sub(delta.unsigned_abs())
    } else {
        line.saturating_add(delta.unsigned_abs()).min(last)
    };
    ed.cursor = place_on_line(doc, ed.len, next, ed.goal);
}

fn mark_dirty(ed: &mut Editor) {
    ed.dirty = true;
    ed.quit_armed = false;
    ed.note = Note::None;
}

fn sync_goal(doc: &[u8], ed: &mut Editor) {
    ed.goal = cursor_line_col(&doc[..ed.len], ed.cursor).1;
}

fn cursor_line(doc: &[u8], ed: &Editor) -> usize {
    cursor_line_col(&doc[..ed.len], ed.cursor).0
}

fn cursor_line_col(doc: &[u8], cursor: usize) -> (usize, usize) {
    let mut line = 0usize;
    let mut col = 0usize;
    for &byte in &doc[..cursor.min(doc.len())] {
        if byte == b'\n' {
            line += 1;
            col = 0;
        } else {
            col += 1;
        }
    }
    (line, col)
}

fn last_line(doc: &[u8]) -> usize {
    doc.iter().filter(|byte| **byte == b'\n').count()
}

fn line_start(doc: &[u8], len: usize, line: usize) -> usize {
    let mut seen = 0usize;
    let mut i = 0usize;
    while i < len && seen < line {
        if doc[i] == b'\n' {
            seen += 1;
        }
        i += 1;
    }
    i
}

fn line_len(doc: &[u8], len: usize, start: usize) -> usize {
    let rest = &doc[start..len.min(doc.len())];
    rest.iter()
        .position(|byte| *byte == b'\n')
        .unwrap_or(rest.len())
}

fn place_on_line(doc: &[u8], len: usize, line: usize, goal: usize) -> usize {
    let start = line_start(doc, len, line);
    start + goal.min(line_len(doc, len, start))
}

fn complete_word(doc: &mut [u8], ed: &mut Editor) -> Step {
    ed.quit_armed = false;
    let start = word_start(doc, ed.cursor);
    if !ed.compl_on {
        let n = (ed.cursor - start).min(ed.prefix.len());
        ed.prefix[..n].copy_from_slice(&doc[start..start + n]);
        ed.prefix_len = n;
        ed.compl_i = 0;
        ed.compl_on = true;
    } else {
        ed.compl_i = ed.compl_i.saturating_add(1);
    }
    let mut slots = [Compl::new(); 8];
    let n = complete(
        ed.lang,
        &doc[..ed.len],
        &ed.prefix[..ed.prefix_len],
        &mut slots,
    );
    if n == 0 {
        set_msg(ed, b"no match");
        ed.compl_on = false;
        return Step::Redraw;
    }
    let slot = slots[ed.compl_i % n];
    if !replace_word(doc, ed, start, &slot.bytes[..slot.len]) {
        ed.compl_on = false;
        return Step::Redraw;
    }
    set_msg(ed, &slot.bytes[..slot.len]);
    Step::Redraw
}

fn word_start(doc: &[u8], cursor: usize) -> usize {
    let mut i = cursor.min(doc.len());
    while i > 0 && is_word(doc[i - 1]) {
        i -= 1;
    }
    i
}

fn is_word(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

fn replace_word(doc: &mut [u8], ed: &mut Editor, start: usize, word: &[u8]) -> bool {
    let old = ed.cursor.saturating_sub(start);
    if ed.len - old + word.len() > CAP {
        ed.note = Note::Full;
        return false;
    }
    let tail = ed.cursor;
    if word.len() > old {
        let extra = word.len() - old;
        doc.copy_within(tail..ed.len, tail + extra);
    } else if word.len() < old {
        let less = old - word.len();
        doc.copy_within(tail..ed.len, tail - less);
    }
    doc[start..start + word.len()].copy_from_slice(word);
    ed.len = ed.len - old + word.len();
    ed.cursor = start + word.len();
    mark_dirty(ed);
    sync_goal(doc, ed);
    true
}

fn set_msg(ed: &mut Editor, text: &[u8]) {
    let n = text.len().min(ed.msg.len());
    ed.msg[..n].copy_from_slice(&text[..n]);
    ed.msg_len = n;
    ed.note = Note::Msg;
}

fn draw(doc: &[u8], path: &[u8], ed: &mut Editor) {
    ed.cursor = ed.cursor.min(ed.len);
    let (line, col) = cursor_line_col(&doc[..ed.len], ed.cursor);
    if line < ed.top {
        ed.top = line;
    }
    if line >= ed.top + VIEW_ROWS {
        ed.top = line + 1 - VIEW_ROWS;
    }
    let hscroll = col.saturating_sub(TEXT_COLS - 1);
    draw_title(path, ed);
    if ed.help {
        draw_help_page();
        draw_status(doc, ed, line, col);
        draw_shortcuts();
        return;
    }
    let diag = diagnose(ed.lang, &doc[..ed.len]);
    let mut state = HiState::new();
    for line_no in 0..ed.top {
        if let Some(text) = view_line(doc, ed.len, line_no) {
            paint_line(ed.lang, text, &mut state, |_, _| {});
        }
    }
    let last = last_line(&doc[..ed.len]);
    for row in 0..VIEW_ROWS {
        let line_no = ed.top + row;
        let mut out = Row::new();
        out.goto(2 + row, 1);
        out.erase();
        if line_no <= last {
            push_gutter(
                &mut out,
                line_no + 1,
                diag.is_some_and(|d| d.line == line_no),
            );
            if let Some(text) = view_line(doc, ed.len, line_no) {
                paint_visible(&mut out, ed.lang, text, hscroll, &mut state);
            }
        }
        out.push_sgr(b"\x1b[0m");
        out.flush();
    }
    draw_status(doc, ed, line, col);
    draw_shortcuts();
    let mut out = Row::new();
    out.goto(2 + line - ed.top, 1 + GUTTER + col - hscroll);
    out.flush();
}

fn push_gutter(out: &mut Row, line: usize, bad: bool) {
    if bad {
        out.push_sgr(b"\x1b[31m");
    } else {
        out.push_sgr(b"\x1b[7m");
    }
    out.push_vis(&fmt_gutter(line, bad));
    out.push_sgr(b"\x1b[0m");
}

fn fmt_gutter(line: usize, bad: bool) -> [u8; GUTTER] {
    let mut raw = [b' '; GUTTER];
    raw[GUTTER - 1] = b'|';
    let mut n = line.max(1);
    let mut i = GUTTER - 2;
    loop {
        raw[i] = b'0' + (n % 10) as u8;
        n /= 10;
        if n == 0 || i == 0 {
            break;
        }
        i -= 1;
    }
    if bad {
        raw[0] = b'!';
    }
    raw
}

fn paint_visible(out: &mut Row, lang: Lang, line: &[u8], hscroll: usize, state: &mut HiState) {
    let mut col = 0usize;
    paint_line(lang, line, state, |span, bytes| {
        let from = col;
        let to = col + bytes.len();
        col = to;
        let vis_from = from.max(hscroll);
        let vis_to = to.min(hscroll + TEXT_COLS);
        if vis_from < vis_to {
            let a = vis_from - from;
            let b = vis_to - from;
            out.push_sgr(span_sgr(span));
            out.push_vis(&bytes[a..b]);
        }
    });
}

fn span_sgr(span: Span) -> &'static [u8] {
    match span {
        Span::Keyword => b"\x1b[36m",
        Span::String => b"\x1b[32m",
        Span::Comment => b"\x1b[90m",
        Span::Number => b"\x1b[35m",
        Span::Text => b"\x1b[37m",
    }
}

fn view_line(doc: &[u8], len: usize, line: usize) -> Option<&[u8]> {
    if line > last_line(&doc[..len]) {
        return None;
    }
    let start = line_start(doc, len, line);
    let n = line_len(doc, len, start);
    Some(&doc[start..start + n])
}

fn draw_title(path: &[u8], ed: &Editor) {
    let mut out = Row::new();
    out.goto(1, 1);
    out.erase();
    out.push_sgr(b"\x1b[7m");
    out.push_vis(b" galexy nano  ");
    out.push_vis(tail(path, 28));
    if ed.lang != Lang::Text {
        out.push_vis(b"  ");
        out.push_vis(ed.lang.name());
    }
    if ed.readonly {
        out.push_vis(b"  Read Only");
    } else if ed.dirty {
        out.push_vis(b"  Modified");
    }
    out.pad_to(WIDTH);
    out.push_sgr(b"\x1b[0m");
    out.flush();
}

fn draw_status(doc: &[u8], ed: &Editor, line: usize, col: usize) {
    let mut out = Row::new();
    out.goto(2 + VIEW_ROWS, 1);
    out.erase();
    out.push_sgr(b"\x1b[7m ");
    if ed.lang != Lang::Text {
        out.push_vis(ed.lang.name());
        out.push_vis(b"  ");
    }
    match ed.note {
        Note::New => out.push_vis(b"New File"),
        Note::Wrote(n) => {
            out.push_vis(b"Wrote ");
            out.push_dec(n);
        }
        Note::ReadOnly => out.push_vis(b"Read Only"),
        Note::Unsaved => out.push_vis(b"Unsaved; ^X discards"),
        Note::Full => out.push_vis(b"Buffer full"),
        Note::Cut => out.push_vis(b"Cut Text"),
        Note::Paste => out.push_vis(b"Uncut Text"),
        Note::Failed => out.push_vis(b"Save failed"),
        Note::Msg => out.push_vis(&ed.msg[..ed.msg_len]),
        Note::None => {
            if let Some(diag) = diagnose(ed.lang, &doc[..ed.len]) {
                out.push_vis(diag.msg.as_bytes());
            } else if ed.dirty {
                out.push_vis(b"Modified");
            }
        }
    }
    if ed.help {
        out.push_vis(b"  any key closes help");
    }
    out.push_vis(b"  @");
    out.push_dec(line + 1);
    out.push_vis(b",");
    out.push_dec(col + 1);
    out.push_vis(b"@");
    out.pad_to(WIDTH);
    out.push_sgr(b"\x1b[0m");
    out.flush();
}

fn draw_shortcuts() {
    let mut top = Row::new();
    top.goto(3 + VIEW_ROWS, 1);
    top.erase();
    shortcut(&mut top, b"^G", b"Help");
    shortcut(&mut top, b"^O", b"Write Out");
    shortcut(&mut top, b"^K", b"Cut Text");
    shortcut(&mut top, b"^T", b"Complete");
    shortcut(&mut top, b"^Y", b"Prev Pg");
    top.push_sgr(b"\x1b[0m");
    top.flush();
    let mut bot = Row::new();
    bot.goto(4 + VIEW_ROWS, 1);
    bot.erase();
    shortcut(&mut bot, b"^X", b"Exit");
    shortcut(&mut bot, b"^U", b"Uncut Text");
    shortcut(&mut bot, b"^V", b"Next Pg");
    shortcut(&mut bot, b"^D", b"Delete");
    bot.push_sgr(b"\x1b[0m");
    bot.flush();
}

fn shortcut(out: &mut Row, key: &[u8], label: &[u8]) {
    out.push_sgr(b"\x1b[7m");
    out.push_vis(key);
    out.push_sgr(b"\x1b[0m ");
    out.push_vis(label);
    out.push_vis(b"  ");
}

/// Keys that are not already on the shortcut bar.
fn draw_help_page() {
    const LINES: [&[u8]; 8] = [
        b"arrows            move the cursor",
        b"line numbers      every row; ! is a problem",
        b"Ctrl-A / Ctrl-E   start / end of the line",
        b"Ctrl-Y / Ctrl-V   page up / down",
        b"Ctrl-T            complete Rust or Python",
        b"Ctrl-K / Ctrl-U   cut the line / paste it",
        b"Ctrl-O / Ctrl-X   save / exit",
        b"Ctrl-G or any key close this help",
    ];
    for row in 0..VIEW_ROWS {
        let mut out = Row::new();
        out.goto(2 + row, 1);
        out.erase();
        if let Some(line) = LINES.get(row) {
            out.push(b" ");
            out.push(line);
        }
        out.flush();
    }
}

fn tail(bytes: &[u8], max: usize) -> &[u8] {
    if bytes.len() <= max {
        bytes
    } else {
        &bytes[bytes.len() - max..]
    }
}

fn leave_screen() {
    write_console(b"\x1b[0m\x1b[2J\x1b[H");
}

struct Row {
    buf: [u8; 512],
    len: usize,
    cols: usize,
}

impl Row {
    fn new() -> Self {
        Self {
            buf: [0; 512],
            len: 0,
            cols: 0,
        }
    }

    fn push(&mut self, bytes: &[u8]) {
        let room = self.buf.len() - self.len;
        let n = bytes.len().min(room);
        self.buf[self.len..self.len + n].copy_from_slice(&bytes[..n]);
        self.len += n;
    }

    fn push_vis(&mut self, bytes: &[u8]) {
        self.push(bytes);
        self.cols += bytes.len();
    }

    fn push_sgr(&mut self, bytes: &[u8]) {
        self.push(bytes);
    }

    fn push_dec(&mut self, mut n: usize) {
        if n == 0 {
            self.push_vis(b"0");
            return;
        }
        let mut tmp = [0u8; 8];
        let mut i = tmp.len();
        while n > 0 {
            i -= 1;
            tmp[i] = b'0' + (n % 10) as u8;
            n /= 10;
        }
        self.push_vis(&tmp[i..]);
    }

    fn pad_to(&mut self, cols: usize) {
        while self.cols < cols {
            self.push_vis(b" ");
        }
    }

    fn push_u(&mut self, mut n: usize) {
        if n == 0 {
            self.push(b"0");
            return;
        }
        let mut tmp = [0u8; 8];
        let mut i = tmp.len();
        while n > 0 {
            i -= 1;
            tmp[i] = b'0' + (n % 10) as u8;
            n /= 10;
        }
        self.push(&tmp[i..]);
    }

    fn goto(&mut self, row: usize, col: usize) {
        self.push(b"\x1b[");
        self.push_u(row);
        self.push(b";");
        self.push_u(col);
        self.push(b"H");
    }

    fn erase(&mut self) {
        self.push(b"\x1b[2K");
    }

    fn flush(&self) {
        if self.len > 0 {
            let _ = write_console(&self.buf[..self.len]);
        }
    }
}
