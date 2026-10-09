//! nano — a screen editor in the shape of GNU nano.
//!
//! Trust: the shell spawns this with `SPAWN_INHERIT` (the caller's galfs
//! cards) and `SPAWN_GRANT_KEYBOARD`, then Cap-waits. The ramdisk is
//! trusted code. The buffer lives in this image (no user heap) and holds
//! one text file. Ctrl-O writes it back, Ctrl-X leaves. Ctrl-C is the
//! seat's foreground kill and never reaches this loop.

#![no_std]
#![no_main]

use core::cell::UnsafeCell;

use galexy_abi::{Cap, CapRights, SysError};
use galexy_rt::{
    arg, close, create_replace, entry, keyboard_cap, open, read, write, write_console, yield_now,
};

entry!(main);

/// Bytes of one file. Matches the default actor quota so a normal note fits.
const CAP: usize = 16 * 1024;
/// One cut line, including its newline.
const CUT: usize = 1024;
/// Columns the editor paints. Narrower than the framebuffer and a typical
/// host terminal, so a CSI row does not wrap under the cursor math.
const COLS: usize = 72;
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
    note: Note,
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
    Help,
    Failed,
}

enum Parse {
    Normal,
    Esc,
    Csi,
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
        note: if loaded.new_file {
            Note::New
        } else {
            Note::None
        },
    };
    write_console(b"\x1b[0m\x1b[2J");
    draw(doc, path, &mut ed);
    let mut parse = Parse::Normal;
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
        let mut redraw = false;
        for &byte in &buf[..n] {
            match feed(&mut parse, doc, cut, path, &mut ed, byte) {
                Step::Stay => {}
                Step::Redraw => redraw = true,
                Step::Quit => {
                    leave_screen();
                    return 0;
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

fn feed(
    parse: &mut Parse,
    doc: &mut [u8],
    cut: &mut [u8],
    path: &[u8],
    ed: &mut Editor,
    byte: u8,
) -> Step {
    match parse {
        Parse::Esc => {
            *parse = if byte == b'[' {
                Parse::Csi
            } else if byte == 0x1b {
                Parse::Esc
            } else {
                Parse::Normal
            };
            Step::Stay
        }
        Parse::Csi => feed_csi(parse, doc, ed, byte),
        Parse::Normal => feed_normal(parse, doc, cut, path, ed, byte),
    }
}

fn feed_csi(parse: &mut Parse, doc: &[u8], ed: &mut Editor, byte: u8) -> Step {
    match byte {
        b'0'..=b'9' | b';' => Step::Stay,
        0x1b => {
            *parse = Parse::Esc;
            Step::Stay
        }
        b'A' => {
            *parse = Parse::Normal;
            move_vert(doc, ed, -1);
            Step::Redraw
        }
        b'B' => {
            *parse = Parse::Normal;
            move_vert(doc, ed, 1);
            Step::Redraw
        }
        b'C' => {
            *parse = Parse::Normal;
            move_right(doc, ed);
            Step::Redraw
        }
        b'D' => {
            *parse = Parse::Normal;
            move_left(doc, ed);
            Step::Redraw
        }
        _ => {
            *parse = Parse::Normal;
            Step::Stay
        }
    }
}

fn feed_normal(
    parse: &mut Parse,
    doc: &mut [u8],
    cut: &mut [u8],
    path: &[u8],
    ed: &mut Editor,
    byte: u8,
) -> Step {
    match byte {
        0x1b => {
            *parse = Parse::Esc;
            Step::Stay
        }
        0x18 => {
            if ed.dirty && !ed.quit_armed {
                ed.quit_armed = true;
                ed.note = Note::Unsaved;
                Step::Redraw
            } else {
                Step::Quit
            }
        }
        0x0f => {
            save(doc, path, ed);
            Step::Redraw
        }
        0x0b => {
            cut_line(doc, cut, ed);
            Step::Redraw
        }
        0x15 => {
            paste(doc, cut, ed);
            Step::Redraw
        }
        0x07 => {
            ed.quit_armed = false;
            ed.note = Note::Help;
            Step::Redraw
        }
        0x01 => {
            ed.quit_armed = false;
            ed.cursor = line_start(doc, ed.len, cursor_line(doc, ed));
            sync_goal(doc, ed);
            Step::Redraw
        }
        0x05 => {
            ed.quit_armed = false;
            let start = line_start(doc, ed.len, cursor_line(doc, ed));
            ed.cursor = start + line_len(doc, ed.len, start);
            sync_goal(doc, ed);
            Step::Redraw
        }
        0x04 => {
            delete_at(doc, ed);
            Step::Redraw
        }
        0x19 => {
            move_vert(doc, ed, -PAGE);
            Step::Redraw
        }
        0x16 => {
            move_vert(doc, ed, PAGE);
            Step::Redraw
        }
        0x08 | 0x7f => {
            backspace(doc, ed);
            Step::Redraw
        }
        b'\n' | b'\r' => {
            insert(doc, ed, b'\n');
            Step::Redraw
        }
        b if b.is_ascii_graphic() || b == b' ' => {
            insert(doc, ed, b);
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

fn draw(doc: &[u8], path: &[u8], ed: &mut Editor) {
    ed.cursor = ed.cursor.min(ed.len);
    let (line, col) = cursor_line_col(&doc[..ed.len], ed.cursor);
    if line < ed.top {
        ed.top = line;
    }
    if line >= ed.top + VIEW_ROWS {
        ed.top = line + 1 - VIEW_ROWS;
    }
    let hscroll = col.saturating_sub(COLS - 1);
    draw_title(path, ed);
    for row in 0..VIEW_ROWS {
        let mut out = Row::new();
        out.goto(2 + row, 1);
        out.erase();
        if let Some(text) = view_line(doc, ed.len, ed.top + row) {
            out.push(visible(text, hscroll));
        }
        out.flush();
    }
    draw_status(ed, line, col);
    draw_help();
    let mut out = Row::new();
    out.goto(2 + line - ed.top, 1 + col - hscroll);
    out.flush();
}

fn view_line(doc: &[u8], len: usize, line: usize) -> Option<&[u8]> {
    if line > last_line(&doc[..len]) {
        return None;
    }
    let start = line_start(doc, len, line);
    let n = line_len(doc, len, start);
    Some(&doc[start..start + n])
}

fn visible(line: &[u8], hscroll: usize) -> &[u8] {
    let rest = line.get(hscroll..).unwrap_or(&[]);
    let n = rest.len().min(COLS);
    &rest[..n]
}

fn draw_title(path: &[u8], ed: &Editor) {
    let mut out = Row::new();
    out.goto(1, 1);
    out.erase();
    out.push(b"\x1b[36m");
    out.push(b" galexy nano  ");
    out.push(tail(path, 40));
    if ed.readonly {
        out.push(b"  read only");
    }
    if ed.dirty {
        out.push(b"  *");
    }
    out.push(b"\x1b[0m");
    out.flush();
}

fn draw_status(ed: &Editor, line: usize, col: usize) {
    let mut out = Row::new();
    out.goto(2 + VIEW_ROWS, 1);
    out.erase();
    out.push(b"\x1b[33m ");
    match ed.note {
        Note::New => out.push(b"New file"),
        Note::Wrote(n) => {
            out.push(b"Wrote ");
            out.push_u(n);
        }
        Note::ReadOnly => out.push(b"Read only"),
        Note::Unsaved => out.push(b"Unsaved; ^X discards"),
        Note::Full => out.push(b"Buffer full"),
        Note::Cut => out.push(b"Cut"),
        Note::Paste => out.push(b"Pasted"),
        Note::Help => out.push(b"^X exit  ^O save  ^K cut  ^U paste"),
        Note::Failed => out.push(b"Save failed"),
        Note::None if ed.dirty => out.push(b"Modified"),
        Note::None => {}
    }
    out.push(b"  @");
    out.push_u(line + 1);
    out.push(b",");
    out.push_u(col + 1);
    out.push(b"@");
    out.push(b"\x1b[0m");
    out.flush();
}

fn draw_help() {
    let mut out = Row::new();
    out.goto(3 + VIEW_ROWS, 1);
    out.erase();
    out.push(b"\x1b[90m ^X Exit  ^O Save  ^K Cut  ^U Paste  ^G Help\x1b[0m");
    out.flush();
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
    buf: [u8; 160],
    len: usize,
}

impl Row {
    fn new() -> Self {
        Self {
            buf: [0; 160],
            len: 0,
        }
    }

    fn push(&mut self, bytes: &[u8]) {
        let room = self.buf.len() - self.len;
        let n = bytes.len().min(room);
        self.buf[self.len..self.len + n].copy_from_slice(&bytes[..n]);
        self.len += n;
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
