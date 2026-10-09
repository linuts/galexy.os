//! The interactive shell: ring-3 command center.
//!
//! Keys arrive through the keyboard capability (arrow keys as CSI).
//! Utilities spawn with `SPAWN_INHERIT`, then [`galexy_rt::wait`] on the
//! child Cap so the prompt returns after they exit; a bare program name
//! returns once the load finishes (Cap dropped) and keeps running.
//! Bare launches share this console — there is no background job.
//! `echo text | cat` is a pipe moved into the children at spawn. A `*` word expands to
//! names in the current directory from the files snapshot.
//! `echo`, `cat`, `nano`, `touch`, `mkdir`, `rm`, and `ls` are those utilities.
//! `nano` also receives the keyboard grant and Cap-waits, so it can edit.
//! The kernel keeps the status bar and the screen, and loads this shell
//! again if it faults. The current directory lives here and starts over
//! at `/` after a restart. Archive names stay at `/`. A path may begin
//! with `/`, and the first component may be `owner@name` (`/dan@Desktop`).
//! Boot shows a login screen (`Galexy.OS v… (ttyN)`). After login a
//! fastfetch-style dashboard prints, then the prompt is `user@galexy>`
//! (with `:/path` when cwd is not `/`). Up/down arrows recall lines from
//! the session history (loaded from / saved to per-user `shell.history`
//! on login / logout). `logout` returns to login.

#![no_std]
#![no_main]

use core::sync::atomic::{AtomicU64, Ordering};

use galexy_abi::{Cap, SysError, SyscallResult};
use galexy_rt::{
    arg, close, create, create_replace, dmesg_cap, entry, files_cap, grant, keyboard_cap, kill,
    open, pipe, read, reboot, revoke, share, shutdown, spawn_with, stats_cap, sync, tasks_cap,
    threads_cap, unshare, user, user_login, user_logout, user_name, user_name_pass, user_passwd,
    user_quota, user_setquota, user_unlock, volume_locked, wait, write, write_console, yield_now,
};

/// Exit status of the last Cap-waited utility (or spawn failure).
static LAST_STATUS: AtomicU64 = AtomicU64::new(0);

entry!(main);

const LINE_MAX: usize = 80;
const PATH_MAX: usize = 64;
const USER_MAX: usize = 16;
const PASS_MAX: usize = 64;
/// Kept command lines in RAM and in `shell.history`.
const HIST_MAX: usize = 32;
const HIST_FILE: &[u8] = b"shell.history";
/// Format-time admin password; seats that still use it must `passwd` first.
const ADMIN_DEFAULT_PASS: &[u8] = b"admin";

struct Cwd {
    buf: [u8; PATH_MAX],
    len: usize,
}

struct History {
    lines: [[u8; LINE_MAX]; HIST_MAX],
    lens: [usize; HIST_MAX],
    count: usize,
}

enum ReplEnd {
    Logout,
    Die(i32),
}

/// ESC / CSI parser while editing a line (arrow keys → history).
enum KeyParse {
    Normal,
    Esc,
    Csi,
}

fn main() -> i32 {
    let kbd = keyboard_cap();
    let tty = tty_number();
    let mut cwd = Cwd {
        buf: [0; PATH_MAX],
        len: 0,
    };
    let mut must_change = false;
    loop {
        if !session_logged_in() {
            match login_screen(kbd, tty) {
                None => return 1,
                Some(change) => {
                    must_change = change;
                    cwd.len = 0;
                    show_dashboard(tty);
                    if must_change {
                        write_console(b"passwd: change the default password\n");
                    }
                }
            }
        }
        let mut history = History::new();
        history.load();
        match repl(kbd, &mut cwd, &mut must_change, &mut history) {
            ReplEnd::Logout => {
                must_change = false;
                continue;
            }
            ReplEnd::Die(code) => return code,
        }
    }
}

/// 1-based TTY from the loader startup arg, or 1 if missing.
fn tty_number() -> u8 {
    match arg().first() {
        Some(&n) if (1..=12).contains(&n) => n,
        _ => 1,
    }
}

fn session_logged_in() -> bool {
    let mut name = [0u8; USER_MAX];
    let got = user(&mut name, galexy_abi::USER_WHOAMI);
    got.ok && got.value > 0
}

/// Login banner + prompts until a password succeeds.
///
/// Returns `Some(must_change)` — `must_change` is set when `admin` still
/// uses the format default password.
fn login_screen(kbd: Cap, tty: u8) -> Option<bool> {
    prompt_volume(kbd);
    loop {
        write_console(&[0x0c]);
        write_console(b"Galexy.OS v");
        write_console(galexy_abi::OS_VERSION.as_bytes());
        write_console(b" (tty");
        write_tty_digits(tty);
        write_console(b")\n\n");
        write_console(b"Login as: ");
        let mut name = [0u8; USER_MAX];
        match read_line(kbd, &mut name, false) {
            LineRead::Denied => return None,
            LineRead::Cancel | LineRead::Overlong => continue,
            LineRead::Line(0) => continue,
            LineRead::Line(nlen) => {
                write_console(b"Password: ");
                let mut pass = [0u8; PASS_MAX];
                let plen = match read_line(kbd, &mut pass, true) {
                    LineRead::Denied => return None,
                    LineRead::Cancel | LineRead::Overlong | LineRead::Line(0) => {
                        wipe(&mut pass);
                        continue;
                    }
                    LineRead::Line(n) => n,
                };
                let default_admin =
                    &name[..nlen] == b"admin" && &pass[..plen] == ADMIN_DEFAULT_PASS;
                let result = user_login(&name[..nlen], &pass[..plen]);
                wipe(&mut pass);
                if result.ok {
                    write_console(b"\n");
                    return Some(default_admin);
                }
                if result.value == SysError::Locked as u64 {
                    write_console(b"\nLogin locked\n");
                } else {
                    write_console(b"\nLogin incorrect\n");
                }
                for _ in 0..30 {
                    yield_now();
                }
            }
        }
    }
}

/// Ask for the volume passphrase when the disk is locked.
///
/// An empty line skips to the RAM-only login. A wrong passphrase retries.
fn prompt_volume(kbd: Cap) {
    if !volume_locked() {
        return;
    }
    loop {
        write_console(b"Volume passphrase: ");
        let mut pass = [0u8; PASS_MAX];
        match read_line(kbd, &mut pass, true) {
            LineRead::Denied => {
                wipe(&mut pass);
                return;
            }
            LineRead::Cancel | LineRead::Overlong => {
                wipe(&mut pass);
                continue;
            }
            LineRead::Line(0) => {
                wipe(&mut pass);
                write_console(b"\nVolume stays locked (RAM-only)\n");
                return;
            }
            LineRead::Line(n) => {
                let result = user_unlock(&pass[..n]);
                wipe(&mut pass);
                if result.ok {
                    write_console(b"\n");
                    return;
                }
                write_console(b"\nVolume unlock failed\n");
            }
        }
    }
}

fn wipe(buf: &mut [u8]) {
    for b in buf.iter_mut() {
        *b = 0;
    }
}

enum LineRead {
    Line(usize),
    /// Esc or Ctrl-C — caller retries or abandons the prompt.
    Cancel,
    /// Paste longer than the buffer — rejected with a message.
    Overlong,
    /// Keyboard capability denied.
    Denied,
}

fn write_tty_digits(tty: u8) {
    if tty >= 10 {
        write_console(&[b'0' + tty / 10, b'0' + tty % 10]);
    } else {
        write_console(&[b'0' + tty]);
    }
}

impl History {
    fn new() -> Self {
        Self {
            lines: [[0; LINE_MAX]; HIST_MAX],
            lens: [0; HIST_MAX],
            count: 0,
        }
    }

    fn get(&self, i: usize) -> Option<&[u8]> {
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
    fn push(&mut self, line: &[u8]) {
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

    fn load(&mut self) {
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

    fn save(&self) {
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

/// Fastfetch-style system glance after login (also `fetch` / `dashboard`).
fn show_dashboard(tty: u8) {
    write_console(&[0x0c]);
    write_console(b"Galexy.OS v");
    write_console(galexy_abi::OS_VERSION.as_bytes());
    write_console(b"\n");
    write_console(b"================================\n");
    write_console(b"command center\n\n");

    write_console(b"OS       Galexy.OS ");
    write_console(galexy_abi::OS_VERSION.as_bytes());
    write_console(b"\n");
    write_console(b"Host     galexy\n");
    write_console(b"TTY      tty");
    write_tty_digits(tty);
    write_console(b"\n");

    write_console(b"User     ");
    let mut name = [0u8; USER_MAX];
    let got = user(&mut name, galexy_abi::USER_WHOAMI);
    if got.ok && got.value > 0 {
        write_console(&name[..(got.value as usize).min(USER_MAX)]);
    } else {
        write_console(b"?");
    }
    write_console(b"\n");

    // Relabel the query Cap snapshots into a dense glance.
    let mut buf = [0u8; 1024];
    let stats = read(stats_cap(), &mut buf);
    if stats.ok && stats.value > 0 {
        let n = (stats.value as usize).min(buf.len());
        for_stat_line(&buf[..n], b"uptime:", b"Uptime   ");
        for_stat_line(&buf[..n], b"frames free:", b"Frames   ");
        for_stat_line(&buf[..n], b"heap:", b"Heap     ");
        for_stat_line(&buf[..n], b"galfs:", b"Galfs    ");
    }

    let tasks = read(tasks_cap(), &mut buf);
    if tasks.ok && tasks.value > 0 {
        let n = (tasks.value as usize).min(buf.len());
        for_stat_line(&buf[..n], b"cooperative tasks:", b"Tasks    ");
    }

    write_console(b"\nlive strip: status bar (bottom)  |  history: up/down\n\n");
}

fn for_stat_line(blob: &[u8], prefix: &[u8], label: &[u8]) {
    let mut i = 0usize;
    while i < blob.len() {
        let rest = &blob[i..];
        let end = rest.iter().position(|b| *b == b'\n').unwrap_or(rest.len());
        let line = &rest[..end];
        if line.starts_with(prefix) {
            write_console(label);
            let val = trim(&line[prefix.len()..]);
            write_console(val);
            write_console(b"\n");
            return;
        }
        i += end + 1;
        if end == rest.len() {
            break;
        }
    }
}

/// Sectioned help — short command column + one-line blurb.
fn show_help() {
    write_console(b"Galexy.OS shell\n");
    write_console(b"===============\n\n");

    help_section(b"files");
    help_row(b"ls", b"list directory");
    help_row(b"cd <path>", b"change directory (.. / /name)");
    help_row(b"echo [text]", b"print, or echo text >|>> file");
    help_row(b"echo $?", b"last Cap-wait exit status");
    help_row(b"cat <path>", b"print file");
    help_row(b"nano <path>", b"edit a file (Ctrl-O save, Ctrl-X exit)");
    help_row(b"touch <path>", b"create empty file");
    help_row(b"mkdir <path>", b"create directory");
    help_row(b"rm <path>", b"remove file or empty dir");
    help_row(b"cp <src> <dst>", b"copy");
    help_row(b"mv <src> <dst>", b"rename / move");
    help_row(b"stat <path>", b"metadata");
    help_row(b"truncate <path> <n>", b"set file length");
    help_row(b"sync", b"flush galfs to disk");
    write_console(b"\n");

    help_section(b"session");
    help_row(b"whoami", b"current user");
    help_row(b"users", b"list accounts");
    help_row(b"tokens", b"list access cards");
    help_row(b"quota [user]", b"show object/byte limits");
    help_row(b"quota set ...", b"quota set <user> <objs> <bytes>");
    help_row(b"login [user]", b"password prompt (or inline pass)");
    help_row(b"logout", b"return to login screen");
    help_row(b"passwd [name]", b"masked password + confirm prompts");
    help_row(b"useradd <name>", b"create account (optional pass)");
    help_row(b"userdel <name>", b"delete empty account");
    help_row(b"su <user>", b"switch via admin / ALL card");
    write_console(b"\n");

    help_section(b"caps");
    help_row(b"grant <r> <path> <task>", b"live card  (r w l c x a=all)");
    help_row(b"revoke <r> <path> <task>", b"drop live card rights");
    help_row(b"share <r> <path> <user>", b"durable home share");
    help_row(b"unshare <r> <path> <user>", b"clear durable share");
    write_console(b"\n");

    help_section(b"system");
    help_row(b"fetch", b"dashboard (alias: dashboard)");
    help_row(b"stats", b"uptime / frames / heap / galfs");
    help_row(b"dmesg", b"recent kernel log lines");
    help_row(b"tasks", b"cooperative tasks");
    help_row(b"threads", b"preemptive thread ticks");
    help_row(b"about", b"version blurb");
    help_row(b"clear", b"clear screen");
    help_row(b"shutdown", b"power off (admin)");
    help_row(b"reboot", b"reset (admin)");
    write_console(b"\n");

    help_section(b"keys");
    help_row(b"up / down", b"history (saved on logout)");
    help_row(b"left / right", b"move inside the line");
    help_row(b"Ctrl-A / Ctrl-E", b"start / end of the line");
    help_row(b"Ctrl-U", b"clear the line");
    help_row(b"Esc / Ctrl-C", b"cancel a prompt");
    help_row(b"Ctrl-D", b"ignored (not end of input)");
    help_row(b"F1-F12", b"switch consoles");
    write_console(b"\n");

    write_console(b"notes\n");
    write_console(b"  echo text | cat    pipe the text through cat\n");
    write_console(b"  *                  names in the current directory\n");
    write_console(b"  a bare program shares this console (no background)\n");
    write_console(b"  default admin/admin must passwd before other commands\n");
    write_console(b"  su admin restores a born-admin seat; login always needs a password\n");
    write_console(b"  a bare program name loads it from the ramdisk\n");
}

fn help_section(title: &[u8]) {
    write_console(title);
    write_console(b"\n");
}

fn help_row(cmd: &[u8], blurb: &[u8]) {
    write_console(b"  ");
    write_console(cmd);
    // Pad command column to 22 chars for a clean second column.
    let mut pad = 22usize.saturating_sub(cmd.len());
    while pad > 0 {
        write_console(b" ");
        pad -= 1;
    }
    write_console(blurb);
    write_console(b"\n");
}

/// Reads a line. When `secret`, echoes `*` (never cleartext) so the COM1
/// mirror of `write_console` cannot leak the password.
fn read_line(kbd: Cap, buf: &mut [u8], secret: bool) -> LineRead {
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

fn repl(kbd: Cap, cwd: &mut Cwd, must_change: &mut bool, history: &mut History) -> ReplEnd {
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

fn editor_insert(
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

fn editor_backspace(
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

fn editor_clear(
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

fn move_left(n: usize) {
    move_csi(n, b'D');
}

fn move_right(n: usize) {
    move_csi(n, b'C');
}

fn move_csi(n: usize, final_byte: u8) {
    if n == 0 {
        return;
    }
    write_console(b"\x1b[");
    write_u64_dec(n as u64);
    write_console(&[final_byte]);
}

fn history_up(
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

fn history_down(
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

fn replace_input_line(line: &mut [u8; LINE_MAX], len: &mut usize, pos: &mut usize, new: &[u8]) {
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

/// Commands allowed while the default admin password is still in use.
fn must_change_allowed(line: &[u8]) -> bool {
    line == b"help"
        || line == b"whoami"
        || line == b"logout"
        || line == b"fetch"
        || line == b"dashboard"
        || line == b"about"
        || arg_of(line, b"passwd").is_some()
}

/// `Some` ends the REPL. `None` keeps reading.
fn dispatch(
    kbd: Cap,
    line: &[u8],
    cwd: &mut Cwd,
    must_change: &mut bool,
    history: &mut History,
) -> Option<ReplEnd> {
    if line.is_empty() {
        prompt(cwd);
        return None;
    }
    if *must_change && !must_change_allowed(line) {
        write_console(b"passwd: change the default password first\n");
        prompt(cwd);
        return None;
    }
    let mut owned = [0u8; LINE_MAX];
    let line = match expanded_line(cwd, line, &mut owned) {
        None => {
            write_console(b"glob: too long\n");
            prompt(cwd);
            return None;
        }
        Some(n) => trim(&owned[..n]),
    };
    if line.is_empty() {
        prompt(cwd);
        return None;
    }
    // Compiled out of release images. The supervisor e2e boots a ramdisk
    // whose shell was built with `--features crash-seam`.
    #[cfg(feature = "crash-seam")]
    if line == b"crash" {
        unsafe {
            core::ptr::read_volatile(core::ptr::null::<u8>());
        }
        return None;
    }
    if try_pipeline(cwd, line) {
        return None;
    }
    if line == b"help" {
        show_help();
        prompt(cwd);
        return None;
    }
    if line == b"fetch" || line == b"dashboard" {
        show_dashboard(tty_number());
        prompt(cwd);
        return None;
    }
    if line == b"about" {
        write_console(b"Galexy.OS v");
        write_console(galexy_abi::OS_VERSION.as_bytes());
        write_console(b" - a small Rust OS\n");
        write_console(b"this shell is the ring-3 command center\n");
        write_console(b"type fetch for the login dashboard; up/down for history\n");
        prompt(cwd);
        return None;
    }
    if line == b"ls" {
        ls(cwd);
        return None;
    }
    if line == b"stats" {
        show(stats_cap(), cwd);
        return None;
    }
    if line == b"dmesg" {
        show(dmesg_cap(), cwd);
        return None;
    }
    if line == b"tasks" {
        show(tasks_cap(), cwd);
        return None;
    }
    if line == b"threads" {
        show(threads_cap(), cwd);
        return None;
    }
    if line == b"shutdown" {
        report_power(cwd, b"shutdown", shutdown());
        return None;
    }
    if line == b"reboot" {
        report_power(cwd, b"reboot", reboot());
        return None;
    }
    if line == b"clear" {
        write_console(&[0x0c]);
        prompt(cwd);
        return None;
    }
    if let Some(name) = arg_of(line, b"echo") {
        echo(cwd, name);
        return None;
    }
    if let Some(name) = arg_of(line, b"cat") {
        cat(cwd, name);
        return None;
    }
    if let Some(name) = arg_of(line, b"nano") {
        nano(cwd, name);
        return None;
    }
    if let Some(name) = arg_of(line, b"touch") {
        touch(cwd, name);
        return None;
    }
    if let Some(name) = arg_of(line, b"mkdir") {
        mkdir(cwd, name);
        return None;
    }
    if let Some(name) = arg_of(line, b"cd") {
        cd(cwd, name);
        return None;
    }
    if let Some(name) = arg_of(line, b"rm") {
        rm(cwd, name);
        return None;
    }
    if let Some(name) = arg_of(line, b"stat") {
        stat_cmd(cwd, name);
        return None;
    }
    if let Some(rest) = arg_of(line, b"truncate") {
        truncate_cmd(cwd, rest);
        return None;
    }
    if line == b"whoami" {
        whoami(cwd);
        return None;
    }
    if line == b"users" {
        users_cmd(cwd);
        return None;
    }
    if line == b"tokens" {
        tokens_cmd(cwd);
        return None;
    }
    if line == b"sync" {
        sync_cmd(cwd);
        return None;
    }
    if line == b"quota" {
        quota_cmd(cwd, b"");
        return None;
    }
    if let Some(rest) = arg_of(line, b"quota") {
        quota_cmd(cwd, rest);
        return None;
    }
    if let Some(rest) = arg_of(line, b"useradd") {
        user_pass_op(
            kbd,
            cwd,
            rest,
            galexy_abi::USER_ADD,
            b"useradd",
            must_change,
        );
        return None;
    }
    if let Some(rest) = arg_of(line, b"userdel") {
        user_op(cwd, rest, galexy_abi::USER_DEL, b"userdel");
        return None;
    }
    if let Some(rest) = arg_of(line, b"login") {
        user_pass_op(
            kbd,
            cwd,
            rest,
            galexy_abi::USER_LOGIN,
            b"login",
            must_change,
        );
        return None;
    }
    if line == b"logout" {
        // Persist while the session still holds galfs tokens — after
        // logout the seat is pre-login and create/write would fail.
        history.save();
        let result = user_logout();
        if !result.ok {
            report_user(cwd, b"logout", result, false);
            return None;
        }
        cwd.len = 0;
        *must_change = false;
        return Some(ReplEnd::Logout);
    }
    if let Some(rest) = arg_of(line, b"passwd") {
        passwd_cmd(kbd, cwd, rest, must_change);
        return None;
    }
    if let Some(rest) = arg_of(line, b"su") {
        user_op(cwd, rest, galexy_abi::USER_SU, b"su");
        return None;
    }
    if let Some(rest) = arg_of(line, b"grant") {
        do_token(cwd, rest, true);
        return None;
    }
    if let Some(rest) = arg_of(line, b"revoke") {
        do_token(cwd, rest, false);
        return None;
    }
    if let Some(rest) = arg_of(line, b"share") {
        do_share(cwd, rest, true);
        return None;
    }
    if let Some(rest) = arg_of(line, b"unshare") {
        do_share(cwd, rest, false);
        return None;
    }
    if let Some(rest) = arg_of(line, b"cp") {
        two_path_util(cwd, b"cp", rest);
        return None;
    }
    if let Some(rest) = arg_of(line, b"mv") {
        two_path_util(cwd, b"mv", rest);
        return None;
    }
    if !line.contains(&b' ') {
        launch(cwd, line);
        return None;
    }
    write_console(line);
    write_console(b": command not found\n");
    write_console(b"\x07"); // BEL → PC speaker
    prompt(cwd);
    None
}

/// `echo text | cat` — one pipe, two utilities. Each end moves into the
/// child at spawn (`SPAWN_WITH_CAPS`), so neither utility polls for `give`.
fn try_pipeline(cwd: &Cwd, line: &[u8]) -> bool {
    let Some(at) = find_slice(line, b" | ") else {
        return false;
    };
    if find_slice(&line[at + 3..], b" | ").is_some() {
        write_console(b"pipeline: one pipe only\n");
        prompt(cwd);
        return true;
    }
    let left = trim(&line[..at]);
    let right = trim(&line[at + 3..]);
    let Some(text) = arg_of(left, b"echo") else {
        write_console(b"pipeline: usage: echo text | cat\n");
        prompt(cwd);
        return true;
    };
    if right != b"cat" {
        write_console(b"pipeline: usage: echo text | cat\n");
        prompt(cwd);
        return true;
    }
    run_echo_cat(cwd, text);
    true
}

/// File-table index of `cap` (offset from [`galexy_abi::FILE_CAP_BASE`]).
fn file_slot(cap: Cap) -> u64 {
    cap.index().saturating_sub(galexy_abi::FILE_CAP_BASE)
}

fn run_echo_cat(cwd: &Cwd, text: &[u8]) {
    let mut arg = [0u8; 2 + LINE_MAX];
    arg[0] = 3;
    arg[1] = 0;
    let ncopy = text.len().min(LINE_MAX);
    arg[2..2 + ncopy].copy_from_slice(&text[..ncopy]);
    let mut ends = [0u64; 2];
    let piped = pipe(&mut ends);
    if !piped.ok {
        write_console(b"pipeline: failed\n");
        LAST_STATUS.store(1, Ordering::Relaxed);
        prompt(cwd);
        return;
    }
    let read_cap = Cap::from_bits(ends[0]);
    let write_cap = Cap::from_bits(ends[1]);
    let echo_grants = galexy_abi::SPAWN_INHERIT
        | galexy_abi::SPAWN_WITH_CAPS
        | (file_slot(write_cap) << galexy_abi::SPAWN_CAP_SHIFT)
        | (galexy_abi::SPAWN_CAP_NONE << (galexy_abi::SPAWN_CAP_SHIFT + 4));
    let echo_res = spawn_with(b"echo", &arg[..2 + ncopy], echo_grants);
    if !echo_res.ok {
        let _ = close(read_cap);
        let _ = close(write_cap);
        write_console(b"pipeline: failed\n");
        LAST_STATUS.store(1, Ordering::Relaxed);
        prompt(cwd);
        return;
    }
    let echo_cap = Cap::from_bits(echo_res.value);
    let cat_grants = galexy_abi::SPAWN_INHERIT
        | galexy_abi::SPAWN_WITH_CAPS
        | (file_slot(read_cap) << galexy_abi::SPAWN_CAP_SHIFT)
        | (galexy_abi::SPAWN_CAP_NONE << (galexy_abi::SPAWN_CAP_SHIFT + 4));
    let cat_res = spawn_with(b"cat", b"-", cat_grants);
    if !cat_res.ok {
        let _ = close(read_cap);
        let _ = kill(echo_cap);
        let _ = wait(echo_cap);
        write_console(b"pipeline: failed\n");
        LAST_STATUS.store(1, Ordering::Relaxed);
        prompt(cwd);
        return;
    }
    let cat_cap = Cap::from_bits(cat_res.value);
    let _ = wait(echo_cap);
    let cat_done = wait(cat_cap);
    LAST_STATUS.store(
        if cat_done.ok { cat_done.value } else { 1 },
        Ordering::Relaxed,
    );
    prompt(cwd);
}

/// Copies `line` into `out`, expanding `*` words against the files snapshot.
/// `None` when the expansion does not fit.
fn expanded_line(cwd: &Cwd, line: &[u8], out: &mut [u8; LINE_MAX]) -> Option<usize> {
    if !line.contains(&b'*') {
        if line.len() > out.len() {
            return None;
        }
        out[..line.len()].copy_from_slice(line);
        return Some(line.len());
    }
    let mut snap = [0u8; 1024];
    let got = read(files_cap(), &mut snap);
    let snap_n = if got.ok {
        (got.value as usize).min(snap.len())
    } else {
        0
    };
    let mut n = 0usize;
    let mut i = 0usize;
    let mut first = true;
    while i < line.len() {
        while i < line.len() && line[i] == b' ' {
            i += 1;
        }
        if i >= line.len() {
            break;
        }
        let start = i;
        while i < line.len() && line[i] != b' ' {
            i += 1;
        }
        let word = &line[start..i];
        if !first && !push_byte(out, &mut n, b' ') {
            return None;
        }
        first = false;
        if word.contains(&b'*') {
            let mut matched = false;
            let mut s = 0usize;
            while s < snap_n {
                let rest = &snap[s..snap_n];
                let end = rest.iter().position(|b| *b == b'\n').unwrap_or(rest.len());
                if let Some(name) = dir_entry(&cwd.buf[..cwd.len], &rest[..end]) {
                    if glob_one(word, name) {
                        if matched && !push_byte(out, &mut n, b' ') {
                            return None;
                        }
                        if !push_bytes(out, &mut n, name) {
                            return None;
                        }
                        matched = true;
                    }
                }
                s += end + 1;
            }
            if !matched && !push_bytes(out, &mut n, word) {
                return None;
            }
        } else if !push_bytes(out, &mut n, word) {
            return None;
        }
    }
    Some(n)
}

fn push_byte(out: &mut [u8], n: &mut usize, b: u8) -> bool {
    if *n >= out.len() {
        return false;
    }
    out[*n] = b;
    *n += 1;
    true
}

fn push_bytes(out: &mut [u8], n: &mut usize, bytes: &[u8]) -> bool {
    if n.saturating_add(bytes.len()) > out.len() {
        return false;
    }
    out[*n..*n + bytes.len()].copy_from_slice(bytes);
    *n += bytes.len();
    true
}

/// One `*` in `pat`. The name is one directory entry (no extra slash).
fn glob_one(pat: &[u8], name: &[u8]) -> bool {
    let Some(star) = pat.iter().position(|b| *b == b'*') else {
        return false;
    };
    if pat[star + 1..].contains(&b'*') {
        return false;
    }
    let prefix = &pat[..star];
    let suffix = &pat[star + 1..];
    let leaf = name.strip_suffix(b"/").unwrap_or(name);
    leaf.len() >= prefix.len() + suffix.len() && leaf.starts_with(prefix) && leaf.ends_with(suffix)
}

/// One child of `cwd` from a files-snapshot line. Same shape as `ls`.
fn dir_entry<'a>(cwd: &[u8], line: &'a [u8]) -> Option<&'a [u8]> {
    if line.is_empty() {
        return None;
    }
    if cwd.is_empty() {
        let slashes = line.iter().filter(|b| **b == b'/').count();
        if slashes == 0 || (slashes == 1 && line.ends_with(b"/")) {
            return Some(line);
        }
        return None;
    }
    if line.len() <= cwd.len() + 1 {
        return None;
    }
    if &line[..cwd.len()] != cwd || line[cwd.len()] != b'/' {
        return None;
    }
    let rest = &line[cwd.len() + 1..];
    let slashes = rest.iter().filter(|b| **b == b'/').count();
    if slashes == 0 || (slashes == 1 && rest.ends_with(b"/")) {
        Some(rest)
    } else {
        None
    }
}

fn echo(cwd: &Cwd, rest: &[u8]) {
    if rest == b"$?" {
        write_u64_dec(LAST_STATUS.load(Ordering::Relaxed));
        write_console(b"\n");
        prompt(cwd);
        return;
    }
    if let Some((text, name, append)) = redirection(rest) {
        if name.is_empty() || name.contains(&b' ') || name.contains(&b'/') {
            write_console(b"echo: usage: echo [text] > name\n");
            prompt(cwd);
            return;
        }
        let mut path = [0u8; PATH_MAX];
        let Some(n) = compose(cwd, name, false, &mut path) else {
            write_console(b"echo: path too long\n");
            prompt(cwd);
            return;
        };
        let mut arg = [0u8; 2 + PATH_MAX + LINE_MAX];
        arg[0] = if append { 2 } else { 1 };
        arg[1] = n as u8;
        arg[2..2 + n].copy_from_slice(&path[..n]);
        let ncopy = text.len().min(LINE_MAX);
        arg[2 + n..2 + n + ncopy].copy_from_slice(&text[..ncopy]);
        launch_util(cwd, b"echo", &arg[..2 + n + ncopy], false);
        return;
    }
    let mut arg = [0u8; 2 + LINE_MAX];
    arg[0] = 0;
    arg[1] = 0;
    let ncopy = rest.len().min(LINE_MAX);
    arg[2..2 + ncopy].copy_from_slice(&rest[..ncopy]);
    launch_util(cwd, b"echo", &arg[..2 + ncopy], false);
}

fn redirection(rest: &[u8]) -> Option<(&[u8], &[u8], bool)> {
    if let Some(rest) = rest.strip_prefix(b">> ") {
        return Some((b"", trim(rest), true));
    }
    if let Some(rest) = rest.strip_prefix(b"> ") {
        return Some((b"", trim(rest), false));
    }
    if let Some(at) = find_slice(rest, b" >> ") {
        return Some((trim(&rest[..at]), trim(&rest[at + 4..]), true));
    }
    if let Some(at) = find_slice(rest, b" > ") {
        return Some((trim(&rest[..at]), trim(&rest[at + 3..]), false));
    }
    None
}

fn nano(cwd: &Cwd, name: &[u8]) {
    let name = trim(name);
    if name.is_empty() || !path_arg_ok(name) {
        write_console(b"nano: usage: nano <path>\n");
        prompt(cwd);
        return;
    }
    let mut path = [0u8; PATH_MAX];
    let Some(n) = compose(cwd, name, false, &mut path) else {
        write_console(b"nano: path too long\n");
        prompt(cwd);
        return;
    };
    let grants = galexy_abi::SPAWN_INHERIT | galexy_abi::SPAWN_GRANT_KEYBOARD;
    spawn_and_prompt(cwd, b"nano", &path[..n], grants, true);
}

fn cat(cwd: &Cwd, name: &[u8]) {
    let name = trim(name);
    if !path_arg_ok(name) {
        write_console(b"cat: usage: cat <name>\n");
        prompt(cwd);
        return;
    }
    let mut path = [0u8; PATH_MAX];
    let Some(n) = compose(cwd, name, false, &mut path) else {
        write_console(b"cat: path too long\n");
        prompt(cwd);
        return;
    };
    launch_util(cwd, b"cat", &path[..n], false);
}

fn touch(cwd: &Cwd, name: &[u8]) {
    let name = trim(name);
    if !path_arg_ok(name) {
        write_console(b"touch: usage: touch <name>\n");
        prompt(cwd);
        return;
    }
    let mut path = [0u8; PATH_MAX];
    let Some(n) = compose(cwd, name, false, &mut path) else {
        write_console(b"touch: path too long\n");
        prompt(cwd);
        return;
    };
    launch_util(cwd, b"touch", &path[..n], false);
}

fn mkdir(cwd: &Cwd, name: &[u8]) {
    let name = trim(name);
    if !path_arg_ok(name) {
        write_console(b"mkdir: usage: mkdir <name>\n");
        prompt(cwd);
        return;
    }
    let mut path = [0u8; PATH_MAX];
    let Some(n) = compose(cwd, name, true, &mut path) else {
        write_console(b"mkdir: path too long\n");
        prompt(cwd);
        return;
    };
    launch_util(cwd, b"mkdir", &path[..n], false);
}

fn rm(cwd: &Cwd, name: &[u8]) {
    let name = trim(name);
    if !path_arg_ok(name) || name == b"." || name == b".." {
        write_console(b"rm: usage: rm <name>\n");
        prompt(cwd);
        return;
    }
    let mut path = [0u8; PATH_MAX];
    let Some(n) = compose(cwd, name, false, &mut path) else {
        write_console(b"rm: path too long\n");
        prompt(cwd);
        return;
    };
    launch_util(cwd, b"rm", &path[..n], true);
}

fn stat_cmd(cwd: &Cwd, name: &[u8]) {
    let name = trim(name);
    if !path_arg_ok(name) {
        write_console(b"stat: usage: stat <name>\n");
        prompt(cwd);
        return;
    }
    let mut path = [0u8; PATH_MAX];
    let Some(n) = compose(cwd, name, false, &mut path) else {
        write_console(b"stat: path too long\n");
        prompt(cwd);
        return;
    };
    launch_util(cwd, b"stat", &path[..n], false);
}

fn truncate_cmd(cwd: &Cwd, rest: &[u8]) {
    let rest = trim(rest);
    let Some(sp) = rest.iter().position(|b| *b == b' ') else {
        write_console(b"truncate: usage: truncate <name> <size>\n");
        prompt(cwd);
        return;
    };
    let name = trim(&rest[..sp]);
    let size = trim(&rest[sp + 1..]);
    if !path_arg_ok(name) || size.is_empty() || size.contains(&b' ') {
        write_console(b"truncate: usage: truncate <name> <size>\n");
        prompt(cwd);
        return;
    }
    let mut path = [0u8; PATH_MAX];
    let Some(n) = compose(cwd, name, false, &mut path) else {
        write_console(b"truncate: path too long\n");
        prompt(cwd);
        return;
    };
    let mut arg = [0u8; PATH_MAX + 32];
    if n + 1 + size.len() > arg.len() {
        write_console(b"truncate: args too long\n");
        prompt(cwd);
        return;
    }
    arg[..n].copy_from_slice(&path[..n]);
    arg[n] = 0;
    arg[n + 1..n + 1 + size.len()].copy_from_slice(size);
    launch_util(cwd, b"truncate", &arg[..n + 1 + size.len()], false);
}

/// `grant`/`revoke` `<rights> <path> <task>`. Rights: `r`/`w`/`l`/`c`/`x`.
fn do_token(cwd: &Cwd, rest: &[u8], is_grant: bool) {
    let rest = trim(rest);
    let usage = if is_grant {
        &b"usage: grant <rights> <path> <task>\n"[..]
    } else {
        &b"usage: revoke <rights> <path> <task>\n"[..]
    };
    let Some((rights, path, target)) = parse_rights_path_target(rest, usage) else {
        prompt(cwd);
        return;
    };
    if !target
        .iter()
        .all(|b| b.is_ascii_alphanumeric() || *b == b'.' || *b == b'_' || *b == b'-')
    {
        write_console(b"bad task name\n");
        prompt(cwd);
        return;
    }
    let mut full = [0u8; PATH_MAX];
    let Some(n) = compose(cwd, path, path.ends_with(b"/"), &mut full) else {
        write_console(b"path too long\n");
        prompt(cwd);
        return;
    };
    let result = if is_grant {
        grant(&full[..n], rights, target)
    } else {
        revoke(&full[..n], rights, target)
    };
    if !result.ok {
        write_console(if is_grant { b"grant: " } else { b"revoke: " });
        match SysError::from_code(result.value) {
            SysError::NotFound => write_console(b"not found\n"),
            SysError::AccessDenied => write_console(b"access denied\n"),
            SysError::NoResource => write_console(b"no token slot\n"),
            SysError::BadValue => write_console(b"bad value\n"),
            _ => write_console(b"failed\n"),
        };
    }
    prompt(cwd);
}

/// `share`/`unshare` `<rights> <path> <user>` — durable; reapplied at login.
fn do_share(cwd: &Cwd, rest: &[u8], is_share: bool) {
    let rest = trim(rest);
    let usage = if is_share {
        &b"usage: share <rights> <path> <user>\n"[..]
    } else {
        &b"usage: unshare <rights> <path> <user>\n"[..]
    };
    let Some((rights, path, user)) = parse_rights_path_target(rest, usage) else {
        prompt(cwd);
        return;
    };
    if !user
        .iter()
        .all(|b| b.is_ascii_alphanumeric() || *b == b'.' || *b == b'_' || *b == b'-')
    {
        write_console(b"bad user name\n");
        prompt(cwd);
        return;
    }
    let mut full = [0u8; PATH_MAX];
    let Some(n) = compose(cwd, path, path.ends_with(b"/"), &mut full) else {
        write_console(b"path too long\n");
        prompt(cwd);
        return;
    };
    let result = if is_share {
        share(&full[..n], rights, user)
    } else {
        unshare(&full[..n], rights, user)
    };
    if !result.ok {
        write_console(if is_share { b"share: " } else { b"unshare: " });
        match SysError::from_code(result.value) {
            SysError::NotFound => write_console(b"not found\n"),
            SysError::AccessDenied => write_console(b"access denied\n"),
            SysError::NoResource => write_console(b"no share slot\n"),
            SysError::BadValue => write_console(b"bad value\n"),
            _ => write_console(b"failed\n"),
        };
    }
    prompt(cwd);
}

/// Shared parse for grant/revoke/share/unshare: `<rights> <path> <target>`.
fn parse_rights_path_target<'a>(rest: &'a [u8], usage: &[u8]) -> Option<(u64, &'a [u8], &'a [u8])> {
    let Some(sp1) = rest.iter().position(|b| *b == b' ') else {
        write_console(usage);
        return None;
    };
    let rights_s = &rest[..sp1];
    let after = trim(&rest[sp1 + 1..]);
    let Some(sp2) = after.iter().position(|b| *b == b' ') else {
        write_console(usage);
        return None;
    };
    let path = trim(&after[..sp2]);
    let target = trim(&after[sp2 + 1..]);
    if rights_s.is_empty() || path.is_empty() || target.is_empty() || !path_arg_ok(path) {
        write_console(usage);
        return None;
    }
    let mut rights = 0u64;
    for &b in rights_s {
        rights |= match b {
            b'r' | b'R' => galexy_abi::TOKEN_READ,
            b'w' | b'W' => galexy_abi::TOKEN_WRITE,
            b'l' | b'L' => galexy_abi::TOKEN_LIST,
            b'c' | b'C' => galexy_abi::TOKEN_CREATE,
            b'x' | b'X' => galexy_abi::TOKEN_REMOVE,
            b'a' | b'A' => galexy_abi::TOKEN_ALL,
            _ => {
                write_console(b"rights are r,w,l,c,x,a\n");
                return None;
            }
        };
    }
    if rights == 0 {
        write_console(usage);
        return None;
    }
    Some((rights, path, target))
}

/// `cp`/`mv` `<src> <dst>` — both paths composed against cwd.
fn two_path_util(cwd: &Cwd, program: &[u8], rest: &[u8]) {
    let rest = trim(rest);
    let Some(sp) = rest.iter().position(|b| *b == b' ') else {
        write_console(b"usage: ");
        write_console(program);
        write_console(b" <src> <dst>\n");
        prompt(cwd);
        return;
    };
    let src = trim(&rest[..sp]);
    let dst = trim(&rest[sp + 1..]);
    if !path_arg_ok(src) || !path_arg_ok(dst) {
        write_console(b"usage: ");
        write_console(program);
        write_console(b" <src> <dst>\n");
        prompt(cwd);
        return;
    }
    let mut sbuf = [0u8; PATH_MAX];
    let mut dbuf = [0u8; PATH_MAX];
    let Some(sn) = compose(cwd, src, false, &mut sbuf) else {
        write_console(b"path too long\n");
        prompt(cwd);
        return;
    };
    let Some(dn) = compose(cwd, dst, false, &mut dbuf) else {
        write_console(b"path too long\n");
        prompt(cwd);
        return;
    };
    // Argument is `src\0dst` so the util can split on NUL.
    let mut arg = [0u8; PATH_MAX * 2 + 1];
    if sn + 1 + dn > arg.len() {
        write_console(b"path too long\n");
        prompt(cwd);
        return;
    }
    arg[..sn].copy_from_slice(&sbuf[..sn]);
    arg[sn] = 0;
    arg[sn + 1..sn + 1 + dn].copy_from_slice(&dbuf[..dn]);
    launch_util(cwd, program, &arg[..sn + 1 + dn], false);
}

fn whoami(cwd: &Cwd) {
    let mut buf = [0u8; 64];
    let got = user(&mut buf, galexy_abi::USER_WHOAMI);
    if !got.ok {
        write_console(b"whoami: failed\n");
        prompt(cwd);
        return;
    }
    let n = (got.value as usize).min(buf.len());
    write_console(&buf[..n]);
    write_console(b"\n");
    prompt(cwd);
}

fn parse_u32(s: &[u8]) -> Option<u32> {
    if s.is_empty() {
        return None;
    }
    let mut n = 0u32;
    for &b in s {
        if !b.is_ascii_digit() {
            return None;
        }
        n = n.checked_mul(10)?.checked_add((b - b'0') as u32)?;
    }
    Some(n)
}

fn write_u32(n: u32) {
    let mut buf = [0u8; 10];
    let mut x = n;
    let mut i = buf.len();
    loop {
        i -= 1;
        buf[i] = b'0' + (x % 10) as u8;
        x /= 10;
        if x == 0 {
            break;
        }
    }
    write_console(&buf[i..]);
}

fn quota_cmd(cwd: &Cwd, rest: &[u8]) {
    let rest = trim(rest);
    if rest.is_empty() {
        show_quota(cwd, b"");
        return;
    }
    if let Some(after) = arg_of(rest, b"set") {
        let after = trim(after);
        let Some(sp1) = after.iter().position(|b| *b == b' ') else {
            write_console(b"usage: quota set <user> <objects> <bytes>\n");
            prompt(cwd);
            return;
        };
        let name = trim(&after[..sp1]);
        let after = trim(&after[sp1 + 1..]);
        let Some(sp2) = after.iter().position(|b| *b == b' ') else {
            write_console(b"usage: quota set <user> <objects> <bytes>\n");
            prompt(cwd);
            return;
        };
        let objects = trim(&after[..sp2]);
        let bytes = trim(&after[sp2 + 1..]);
        let Some(max_o) = parse_u32(objects) else {
            write_console(b"quota: bad object limit\n");
            prompt(cwd);
            return;
        };
        let Some(max_b) = parse_u32(bytes) else {
            write_console(b"quota: bad byte limit\n");
            prompt(cwd);
            return;
        };
        if max_o == 0 || max_o > u16::MAX as u32 || name.is_empty() {
            write_console(b"usage: quota set <user> <objects> <bytes>\n");
            prompt(cwd);
            return;
        }
        if !name
            .iter()
            .all(|b| b.is_ascii_alphanumeric() || *b == b'.' || *b == b'_' || *b == b'-')
        {
            write_console(b"quota: bad user name\n");
            prompt(cwd);
            return;
        }
        let got = user_setquota(name, max_o as u16, max_b);
        if !got.ok {
            write_console(b"quota set: ");
            match SysError::from_code(got.value) {
                SysError::AccessDenied => write_console(b"access denied\n"),
                SysError::NotFound => write_console(b"not found\n"),
                SysError::BadValue => write_console(b"bad value\n"),
                _ => write_console(b"failed\n"),
            };
            prompt(cwd);
            return;
        }
        show_quota(cwd, name);
        return;
    }
    if rest.contains(&b' ') {
        write_console(b"usage: quota [user] | quota set <user> <objects> <bytes>\n");
        prompt(cwd);
        return;
    }
    show_quota(cwd, rest);
}

fn show_quota(cwd: &Cwd, name: &[u8]) {
    let mut buf = [0u8; galexy_abi::QUOTA_LEN];
    let got = user_quota(&mut buf, name);
    if !got.ok {
        write_console(b"quota: ");
        match SysError::from_code(got.value) {
            SysError::AccessDenied => write_console(b"access denied\n"),
            SysError::NotFound => write_console(b"not found\n"),
            _ => write_console(b"failed\n"),
        };
        prompt(cwd);
        return;
    }
    let used_o = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]);
    let max_o = u32::from_le_bytes([buf[4], buf[5], buf[6], buf[7]]);
    let used_b = u32::from_le_bytes([buf[8], buf[9], buf[10], buf[11]]);
    let max_b = u32::from_le_bytes([buf[12], buf[13], buf[14], buf[15]]);
    write_console(b"objects ");
    write_u32(used_o);
    write_console(b"/");
    write_u32(max_o);
    write_console(b"  bytes ");
    write_u32(used_b);
    write_console(b"/");
    write_u32(max_b);
    write_console(b"\n");
    prompt(cwd);
}

fn users_cmd(cwd: &Cwd) {
    let mut buf = [0u8; 256];
    let got = user(&mut buf, galexy_abi::USER_USERS);
    if !got.ok {
        write_console(b"users: failed\n");
        prompt(cwd);
        return;
    }
    let n = (got.value as usize).min(buf.len());
    if n > 0 {
        write_console(&buf[..n]);
    }
    prompt(cwd);
}

fn tokens_cmd(cwd: &Cwd) {
    let mut buf = [0u8; 512];
    let got = user(&mut buf, galexy_abi::USER_TOKENS);
    if !got.ok {
        write_console(b"tokens: failed\n");
        prompt(cwd);
        return;
    }
    let n = (got.value as usize).min(buf.len());
    if n == 0 {
        write_console(b"(no tokens)\n");
    } else {
        write_console(&buf[..n]);
    }
    prompt(cwd);
}

fn sync_cmd(cwd: &Cwd) {
    let got = sync();
    if !got.ok {
        write_console(b"sync: failed\n");
    }
    prompt(cwd);
}

fn user_op(cwd: &mut Cwd, rest: &[u8], op: u64, label: &[u8]) {
    let name = trim(rest);
    if name.is_empty()
        || !name
            .iter()
            .all(|b| b.is_ascii_alphanumeric() || *b == b'.' || *b == b'_' || *b == b'-')
    {
        write_console(b"usage: ");
        write_console(label);
        write_console(b" <name>\n");
        prompt(cwd);
        return;
    }
    let result = user_name(name, op);
    report_user(cwd, label, result, op == galexy_abi::USER_SU);
}

fn user_pass_op(
    kbd: Cap,
    cwd: &mut Cwd,
    rest: &[u8],
    op: u64,
    label: &[u8],
    must_change: &mut bool,
) {
    let rest = trim(rest);
    let mut name_buf = [0u8; USER_MAX];
    let mut pass_buf = [0u8; PASS_MAX];

    let (name_inline, pass_inline) = if let Some(sp) = rest.iter().position(|b| *b == b' ') {
        (trim(&rest[..sp]), trim(&rest[sp + 1..]))
    } else {
        (rest, b"" as &[u8])
    };

    let name = if name_inline.is_empty() {
        if op != galexy_abi::USER_LOGIN {
            write_console(b"usage: ");
            write_console(label);
            write_console(b" <name> [password]\n");
            prompt(cwd);
            return;
        }
        write_console(b"Login as: ");
        match read_line(kbd, &mut name_buf, false) {
            LineRead::Line(n) if n > 0 => &name_buf[..n],
            LineRead::Denied => {
                prompt(cwd);
                return;
            }
            _ => {
                prompt(cwd);
                return;
            }
        }
    } else {
        name_inline
    };

    if name.is_empty()
        || !name
            .iter()
            .all(|b| b.is_ascii_alphanumeric() || *b == b'.' || *b == b'_' || *b == b'-')
    {
        write_console(b"usage: ");
        write_console(label);
        write_console(b" <name> [password]\n");
        prompt(cwd);
        return;
    }

    let pass = if pass_inline.is_empty() {
        write_console(b"Password: ");
        match read_line(kbd, &mut pass_buf, true) {
            LineRead::Line(n) if n > 0 => &pass_buf[..n],
            LineRead::Denied => {
                wipe(&mut pass_buf);
                prompt(cwd);
                return;
            }
            _ => {
                wipe(&mut pass_buf);
                prompt(cwd);
                return;
            }
        }
    } else {
        pass_inline
    };

    let default_admin =
        op == galexy_abi::USER_LOGIN && name == b"admin" && pass == ADMIN_DEFAULT_PASS;
    let result = if op == galexy_abi::USER_LOGIN {
        user_login(name, pass)
    } else {
        user_name_pass(name, pass, op)
    };
    wipe(&mut pass_buf);
    if result.ok && op == galexy_abi::USER_LOGIN {
        *must_change = default_admin;
        if *must_change {
            write_console(b"passwd: change the default password\n");
        }
    }
    report_user(
        cwd,
        label,
        result,
        op == galexy_abi::USER_LOGIN || op == galexy_abi::USER_SU,
    );
}

fn passwd_cmd(kbd: Cap, cwd: &mut Cwd, rest: &[u8], must_change: &mut bool) {
    let rest = trim(rest);
    // Never accept an inline password — always prompt + confirm masked.
    if rest.contains(&b' ') {
        write_console(b"usage: passwd [name]\n");
        prompt(cwd);
        return;
    }
    let name = rest;
    if !name.is_empty()
        && !name
            .iter()
            .all(|b| b.is_ascii_alphanumeric() || *b == b'.' || *b == b'_' || *b == b'-')
    {
        write_console(b"usage: passwd [name]\n");
        prompt(cwd);
        return;
    }
    let mut pass_buf = [0u8; PASS_MAX];
    let Some(plen) = read_secret_confirmed(kbd, &mut pass_buf) else {
        prompt(cwd);
        return;
    };
    let result = user_passwd(name, &pass_buf[..plen]);
    wipe(&mut pass_buf);
    if result.ok {
        // Changing own password (empty name) clears the default-password gate.
        if name.is_empty() || name == b"admin" {
            *must_change = false;
        }
    }
    report_user(cwd, b"passwd", result, false);
}

/// Masked `Password:` then `Confirm:`; returns length when both match.
fn read_secret_confirmed(kbd: Cap, out: &mut [u8]) -> Option<usize> {
    let mut first = [0u8; PASS_MAX];
    let mut second = [0u8; PASS_MAX];
    write_console(b"Password: ");
    let n1 = match read_line(kbd, &mut first, true) {
        LineRead::Line(n) if n > 0 => n,
        LineRead::Denied => {
            wipe(&mut first);
            return None;
        }
        _ => {
            wipe(&mut first);
            return None;
        }
    };
    write_console(b"Confirm: ");
    let n2 = match read_line(kbd, &mut second, true) {
        LineRead::Line(n) if n > 0 => n,
        LineRead::Denied => {
            wipe(&mut first);
            wipe(&mut second);
            return None;
        }
        _ => {
            wipe(&mut first);
            wipe(&mut second);
            return None;
        }
    };
    if n1 != n2 || first[..n1] != second[..n2] {
        write_console(b"passwd: passwords do not match\n");
        wipe(&mut first);
        wipe(&mut second);
        return None;
    }
    if n1 > out.len() {
        write_console(b"password too long\n");
        wipe(&mut first);
        wipe(&mut second);
        return None;
    }
    out[..n1].copy_from_slice(&first[..n1]);
    wipe(&mut first);
    wipe(&mut second);
    Some(n1)
}

fn report_user(cwd: &mut Cwd, label: &[u8], result: SyscallResult, reset_cwd: bool) {
    if !result.ok {
        write_console(label);
        write_console(b": ");
        match SysError::from_code(result.value) {
            SysError::NotFound => write_console(b"not found\n"),
            SysError::AccessDenied => write_console(b"access denied\n"),
            SysError::Unsupported => write_console(b"unsupported\n"),
            SysError::NoResource => write_console(b"no resource\n"),
            SysError::BadValue => write_console(b"bad value\n"),
            SysError::Locked => write_console(b"locked\n"),
            _ => write_console(b"failed\n"),
        };
    } else if reset_cwd {
        cwd.len = 0;
    }
    prompt(cwd);
}

/// Power Cap missing → access denied; success that returns is a QEMU quirk.
fn report_power(cwd: &Cwd, label: &[u8], result: SyscallResult) {
    if !result.ok {
        write_console(label);
        write_console(b": ");
        match SysError::from_code(result.value) {
            SysError::AccessDenied => write_console(b"access denied\n"),
            SysError::BadCap => write_console(b"bad capability\n"),
            _ => write_console(b"failed\n"),
        };
    } else {
        write_console(label);
        write_console(b": the machine stayed up\n");
    }
    prompt(cwd);
}

fn cd(cwd: &mut Cwd, name: &[u8]) {
    let name = trim(name);
    if name.is_empty() || name == b"/" {
        cwd.len = 0;
        prompt(cwd);
        return;
    }
    if name == b".." {
        if let Some(slash) = cwd.buf[..cwd.len].iter().rposition(|b| *b == b'/') {
            cwd.len = slash;
        } else {
            cwd.len = 0;
        }
        prompt(cwd);
        return;
    }
    if name.contains(&b' ') {
        write_console(b"cd: usage: cd <name>\n");
        prompt(cwd);
        return;
    }
    // Absolute `/Desktop` or `/dan@Desktop`, or a single relative component.
    if !name.starts_with(b"/") && name.contains(&b'/') {
        write_console(b"cd: usage: cd <name>\n");
        prompt(cwd);
        return;
    }
    let mut path = [0u8; PATH_MAX];
    let Some(n) = compose(cwd, name, true, &mut path) else {
        write_console(b"cd: path too long\n");
        prompt(cwd);
        return;
    };
    if !snapshot_has(&path[..n]) {
        // Leave cwd alone so the prompt stays on the directory that exists.
        write_console(b"cd: no such directory\n");
        prompt(cwd);
        return;
    }
    let bare = n - 1;
    cwd.buf[..bare].copy_from_slice(&path[..bare]);
    cwd.len = bare;
    prompt(cwd);
}

fn ls(cwd: &Cwd) {
    launch_util(cwd, b"ls", &cwd.buf[..cwd.len], true);
}

/// Starts the ramdisk program `name`. A missing name, or a file that is
/// not an ELF, is reported as an unknown command. The prompt returns
/// once the program is loaded (linger / hello keep running).
fn launch(cwd: &Cwd, name: &[u8]) {
    if is_console_shell_name(name) {
        write_console(name);
        write_console(b": reserved (use F1-F12)\n");
        prompt(cwd);
        return;
    }
    spawn_and_prompt(cwd, name, b"", 0, false);
}

/// Spawns a utility with `arg`. `query` adds the files snapshot grant.
/// Inherits session tokens, Cap-waits for exit, then returns the prompt.
fn launch_util(cwd: &Cwd, program: &[u8], arg: &[u8], query: bool) {
    let mut grants = galexy_abi::SPAWN_INHERIT;
    if query {
        grants |= galexy_abi::SPAWN_GRANT_QUERY;
    }
    spawn_and_prompt(cwd, program, arg, grants, true);
}

fn spawn_and_prompt(cwd: &Cwd, program: &[u8], arg: &[u8], grants: u64, wait_exit: bool) {
    let result = spawn_with(program, arg, grants);
    if !result.ok {
        write_console(program);
        write_console(b": ");
        match SysError::from_code(result.value) {
            SysError::NoResource => write_console(b"busy\n"),
            SysError::Unsupported => write_console(b"reserved\n"),
            SysError::NotFound => {
                write_console(b"command not found\n");
                write_console(b"\x07")
            }
            _ => write_console(b"failed\n"),
        };
        // 127 ≈ command not found; other spawn failures are 1.
        let code = if matches!(SysError::from_code(result.value), SysError::NotFound) {
            127
        } else {
            1
        };
        LAST_STATUS.store(code, Ordering::Relaxed);
    } else if wait_exit {
        let waited = wait(Cap::from_bits(result.value));
        LAST_STATUS.store(if waited.ok { waited.value } else { 1 }, Ordering::Relaxed);
    } else {
        // Fire-and-forget: drop the Cap so an exited child can be reaped.
        let _ = close(Cap::from_bits(result.value));
        LAST_STATUS.store(0, Ordering::Relaxed);
    }
    prompt(cwd);
}

fn write_u64_dec(value: u64) {
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
fn is_console_shell_name(name: &[u8]) -> bool {
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
fn arg_of<'a>(line: &'a [u8], cmd: &[u8]) -> Option<&'a [u8]> {
    if line == cmd {
        return Some(b"");
    }
    if line.starts_with(cmd) && line.get(cmd.len()) == Some(&b' ') {
        return Some(trim(&line[cmd.len() + 1..]));
    }
    None
}

fn trim(bytes: &[u8]) -> &[u8] {
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

fn find_slice(hay: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || needle.len() > hay.len() {
        return None;
    }
    hay.windows(needle.len()).position(|w| w == needle)
}

/// `cwd/name`, plus a trailing `/` when `dir`. A name that starts with `/`
/// is absolute (`/Desktop`, `/dan@Desktop`) and ignores `cwd`.
fn compose(cwd: &Cwd, name: &[u8], dir: bool, out: &mut [u8; PATH_MAX]) -> Option<usize> {
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
fn path_arg_ok(name: &[u8]) -> bool {
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

fn comp_bytes_ok(comp: &[u8]) -> bool {
    !comp.is_empty()
        && comp
            .iter()
            .all(|b| b.is_ascii_alphanumeric() || *b == b'.' || *b == b'_' || *b == b'-')
}

fn snapshot_has(line: &[u8]) -> bool {
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

fn show(cap: Cap, cwd: &Cwd) {
    let mut buf = [0u8; 1024];
    let got = read(cap, &mut buf);
    if !got.ok {
        write_console(b"query: denied\n");
    } else if got.value > 0 {
        let n = (got.value as usize).min(buf.len());
        write_console(&buf[..n]);
        if buf[n - 1] != b'\n' {
            write_console(b"\n");
        }
    }
    prompt(cwd);
}

fn prompt(cwd: &Cwd) {
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
