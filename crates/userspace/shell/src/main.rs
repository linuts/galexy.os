//! The interactive shell: ring-3 command center.
//!
//! Keys arrive through the keyboard capability (arrow keys as CSI).
//! Utilities spawn with `SPAWN_INHERIT`, then [`galexy_rt::wait`] on the
//! child Cap so the prompt returns after they exit; a bare program name
//! returns once the load finishes (Cap dropped) and keeps running.
//! A bare launch shares this console. `cmd &` is a background job (`jobs`
//! / `fg`); Ctrl-Z is not implemented. Pipelines are up to four stages
//! joined by ` | `. A `*` word expands to
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

extern crate alloc;

use galexy_abi::{Cap, SysError};
use galexy_rt::{
    arg, entry, keyboard_cap, read, stats_cap, tasks_cap, user, user_login, user_unlock,
    volume_locked, write_console, yield_now,
};

mod builtins;
mod edit;
mod jobs;
mod state;

use edit::*;
use state::*;

entry!(main);

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
pub(crate) fn tty_number() -> u8 {
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

fn write_tty_digits(tty: u8) {
    if tty >= 10 {
        write_console(&[b'0' + tty / 10, b'0' + tty % 10]);
    } else {
        write_console(&[b'0' + tty]);
    }
}

/// Fastfetch-style system glance after login (also `fetch` / `dashboard`).
pub(crate) fn show_dashboard(tty: u8) {
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
