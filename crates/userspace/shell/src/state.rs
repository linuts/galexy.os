//! Shared shell state: cwd, history, prompt, path checks.

use core::sync::atomic::{AtomicBool, AtomicU64};

use galexy_abi::Cap;
use galexy_rt::{
    close, create, create_replace, files_cap, open, read, sync, user, write, write_console,
};

pub(crate) static LAST_STATUS: AtomicU64 = AtomicU64::new(0);
/// Set for the command currently being dispatched when it ended with ` &`.
pub(crate) static BACKGROUND: AtomicBool = AtomicBool::new(false);

pub(crate) const LINE_MAX: usize = 80;
pub(crate) const PATH_MAX: usize = 64;
pub(crate) const USER_MAX: usize = 16;
pub(crate) const PASS_MAX: usize = 64;
/// Kept command lines in RAM and in `shell.history`.
pub(crate) const HIST_MAX: usize = 32;
pub(crate) const HIST_FILE: &[u8] = b"shell.history";
/// Format-time admin password; seats that still use it must `passwd` first.
pub(crate) const ADMIN_DEFAULT_PASS: &[u8] = b"admin";

pub(crate) struct Cwd {
    pub(crate) buf: [u8; PATH_MAX],
    pub(crate) len: usize,
}

pub(crate) struct History {
    pub(crate) lines: [[u8; LINE_MAX]; HIST_MAX],
    pub(crate) lens: [usize; HIST_MAX],
    pub(crate) count: usize,
}

pub(crate) enum ReplEnd {
    Logout,
    Die(i32),
}
pub(crate) fn wipe(buf: &mut [u8]) {
    for b in buf.iter_mut() {
        *b = 0;
    }
}

impl History {
    pub(crate) fn new() -> Self {
        Self {
            lines: [[0; LINE_MAX]; HIST_MAX],
            lens: [0; HIST_MAX],
            count: 0,
        }
    }

    pub(crate) fn get(&self, i: usize) -> Option<&[u8]> {
        if i < self.count {
            Some(&self.lines[i][..self.lens[i]])
        } else {
            None
        }
    }

    /// Append a non-empty line; skip if it matches the newest entry.
    ///
    /// RAM only — [`save`] is for logout. Every galfs write syncs the whole
    /// dual-slot image (~144 KiB + flush); doing that per command made
    /// `ls` feel wedged.
    pub(crate) fn push(&mut self, line: &[u8]) {
        let line = trim(line);
        if line.is_empty() || line.len() > LINE_MAX {
            return;
        }
        if self.count > 0 {
            let last = self.count - 1;
            if self.lens[last] == line.len() && &self.lines[last][..line.len()] == line {
                return;
            }
        }
        if self.count == HIST_MAX {
            for i in 1..HIST_MAX {
                self.lines[i - 1] = self.lines[i];
                self.lens[i - 1] = self.lens[i];
            }
            self.count = HIST_MAX - 1;
        }
        let i = self.count;
        self.lines[i][..line.len()].copy_from_slice(line);
        self.lens[i] = line.len();
        self.count += 1;
    }

    pub(crate) fn load(&mut self) {
        let opened = open(HIST_FILE);
        if !opened.ok {
            return;
        }
        let cap = Cap::from_bits(opened.value);
        let mut buf = [0u8; HIST_MAX * (LINE_MAX + 1)];
        let mut fill = 0usize;
        loop {
            if fill >= buf.len() {
                break;
            }
            let got = read(cap, &mut buf[fill..]);
            if !got.ok || got.value == 0 {
                break;
            }
            fill += got.value as usize;
        }
        let _ = close(cap);
        let mut start = 0usize;
        while start < fill && self.count < HIST_MAX {
            let rest = &buf[start..fill];
            let end = rest.iter().position(|b| *b == b'\n').unwrap_or(rest.len());
            let line = trim(&rest[..end]);
            if !line.is_empty() && line.len() <= LINE_MAX {
                let i = self.count;
                self.lines[i][..line.len()].copy_from_slice(line);
                self.lens[i] = line.len();
                self.count += 1;
            }
            start += end + 1;
            if end == rest.len() {
                break;
            }
        }
    }

    pub(crate) fn save(&self) {
        let created = create_replace(HIST_FILE);
        let bits = if created.ok {
            created.value
        } else {
            let made = create(HIST_FILE);
            if !made.ok {
                return;
            }
            made.value
        };
        let cap = Cap::from_bits(bits);
        for i in 0..self.count {
            let _ = write(cap, &self.lines[i][..self.lens[i]]);
            let _ = write(cap, b"\n");
        }
        let _ = close(cap);
        // One barrier so logout does not race the 1 Hz write-back tick.
        let _ = sync();
    }
}
pub(crate) fn split_word(line: &[u8]) -> Option<(&[u8], &[u8])> {
    let line = trim(line);
    if line.is_empty() {
        return None;
    }
    let at = line.iter().position(|b| *b == b' ').unwrap_or(line.len());
    Some((&line[..at], trim(&line[at..])))
}

pub(crate) fn push_byte(out: &mut [u8], n: &mut usize, b: u8) -> bool {
    if *n >= out.len() {
        return false;
    }
    out[*n] = b;
    *n += 1;
    true
}

pub(crate) fn push_bytes(out: &mut [u8], n: &mut usize, bytes: &[u8]) -> bool {
    if n.saturating_add(bytes.len()) > out.len() {
        return false;
    }
    out[*n..*n + bytes.len()].copy_from_slice(bytes);
    *n += bytes.len();
    true
}
pub(crate) fn write_u64_dec(value: u64) {
    if value == 0 {
        write_console(b"0");
        return;
    }
    let mut tmp = [0u8; 20];
    let mut i = tmp.len();
    let mut n = value;
    while n > 0 {
        i -= 1;
        tmp[i] = b'0' + (n % 10) as u8;
        n /= 10;
    }
    write_console(&tmp[i..]);
}

/// F-key console task names. The kernel also rejects spawning these.
pub(crate) fn is_console_shell_name(name: &[u8]) -> bool {
    matches!(
        name,
        b"shell"
            | b"shell2"
            | b"shell3"
            | b"shell4"
            | b"shell5"
            | b"shell6"
            | b"shell7"
            | b"shell8"
            | b"shell9"
            | b"shell10"
            | b"shell11"
            | b"shell12"
    )
}

/// The bytes after `cmd`, or empty when the line is exactly `cmd`.
pub(crate) fn arg_of<'a>(line: &'a [u8], cmd: &[u8]) -> Option<&'a [u8]> {
    if line == cmd {
        return Some(b"");
    }
    if line.starts_with(cmd) && line.get(cmd.len()) == Some(&b' ') {
        return Some(trim(&line[cmd.len() + 1..]));
    }
    None
}

pub(crate) fn trim(bytes: &[u8]) -> &[u8] {
    let mut start = 0;
    let mut end = bytes.len();
    while start < end && bytes[start] == b' ' {
        start += 1;
    }
    while end > start && bytes[end - 1] == b' ' {
        end -= 1;
    }
    &bytes[start..end]
}

pub(crate) fn find_slice(hay: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || needle.len() > hay.len() {
        return None;
    }
    hay.windows(needle.len()).position(|w| w == needle)
}

/// `cwd/name`, plus a trailing `/` when `dir`. A name that starts with `/`
/// is absolute (`/Desktop`, `/dan@Desktop`) and ignores `cwd`.
pub(crate) fn compose(
    cwd: &Cwd,
    name: &[u8],
    dir: bool,
    out: &mut [u8; PATH_MAX],
) -> Option<usize> {
    let abs = name.starts_with(b"/");
    let body = if abs { &name[1..] } else { name };
    if body.is_empty() && !dir {
        return None;
    }
    let extra = usize::from(dir);
    let need = if abs || cwd.len == 0 {
        body.len() + extra
    } else {
        cwd.len + 1 + body.len() + extra
    };
    if need == 0 || need > PATH_MAX {
        return None;
    }
    let mut n = 0usize;
    if !abs && cwd.len > 0 {
        out[..cwd.len].copy_from_slice(&cwd.buf[..cwd.len]);
        out[cwd.len] = b'/';
        n = cwd.len + 1;
    }
    out[n..n + body.len()].copy_from_slice(body);
    n += body.len();
    if dir {
        out[n] = b'/';
        n += 1;
    }
    Some(n)
}

/// A path the shell may pass to a utility or store as the cwd.
pub(crate) fn path_arg_ok(name: &[u8]) -> bool {
    if name.is_empty() || name.contains(&b' ') {
        return false;
    }
    let body = if name.starts_with(b"/") {
        &name[1..]
    } else {
        name
    };
    if body.is_empty() {
        return false;
    }
    // Actor-root card path: `/eve@/`
    if let Some(head) = body.strip_suffix(b"/") {
        if let Some(at) = head.iter().position(|b| *b == b'@') {
            let own = &head[..at];
            let leaf = &head[at + 1..];
            if leaf.is_empty() && !head.contains(&b'/') && comp_bytes_ok(own) {
                return true;
            }
        }
    }
    let mut first = true;
    for comp in body.split(|b| *b == b'/') {
        if comp.is_empty() || comp == b"." || comp == b".." {
            return false;
        }
        if first {
            if let Some(at) = comp.iter().position(|b| *b == b'@') {
                let (own, leaf) = comp.split_at(at);
                let leaf = &leaf[1..];
                if own.is_empty()
                    || leaf.is_empty()
                    || leaf.contains(&b'@')
                    || !comp_bytes_ok(own)
                    || !comp_bytes_ok(leaf)
                {
                    return false;
                }
            } else if !comp_bytes_ok(comp) {
                return false;
            }
            first = false;
        } else if comp.contains(&b'@') || !comp_bytes_ok(comp) {
            return false;
        }
    }
    true
}

pub(crate) fn comp_bytes_ok(comp: &[u8]) -> bool {
    !comp.is_empty()
        && comp
            .iter()
            .all(|b| b.is_ascii_alphanumeric() || *b == b'.' || *b == b'_' || *b == b'-')
}

pub(crate) fn snapshot_has(line: &[u8]) -> bool {
    let mut buf = [0u8; 1024];
    let got = read(files_cap(), &mut buf);
    if !got.ok {
        return false;
    }
    let n = (got.value as usize).min(buf.len());
    let mut i = 0usize;
    while i < n {
        let rest = &buf[i..n];
        let end = rest.iter().position(|b| *b == b'\n').unwrap_or(rest.len());
        if &rest[..end] == line {
            return true;
        }
        i += end + 1;
    }
    false
}

pub(crate) fn prompt(cwd: &Cwd) {
    // One write, so a kernel log on the serial mirror cannot land between
    // the name and `> `. Shape: `user@galexy>` or `user@galexy:/path> `.
    let mut name = [0u8; USER_MAX];
    let got = user(&mut name, galexy_abi::USER_WHOAMI);
    let mut line = [0u8; USER_MAX + 1 + 6 + 2 + PATH_MAX + 2];
    let mut n = 0usize;
    let uname = if got.ok && got.value > 0 {
        &name[..(got.value as usize).min(USER_MAX)]
    } else {
        b"?"
    };
    line[n..n + uname.len()].copy_from_slice(uname);
    n += uname.len();
    line[n] = b'@';
    n += 1;
    line[n..n + 6].copy_from_slice(b"galexy");
    n += 6;
    if cwd.len > 0 {
        line[n] = b':';
        line[n + 1] = b'/';
        n += 2;
        line[n..n + cwd.len].copy_from_slice(&cwd.buf[..cwd.len]);
        n += cwd.len;
    }
    line[n] = b'>';
    line[n + 1] = b' ';
    n += 2;
    write_console(&line[..n]);
}
