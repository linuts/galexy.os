//! The interactive shell: ring-3 command center.
//!
//! Keys arrive through the keyboard capability (arrow keys as CSI).
//! Utilities spawn with `SPAWN_INHERIT`, then [`galexy_rt::wait`] on the
//! child Cap so the prompt returns after they exit. A ramdisk program
//! with arguments gets the same inherit-and-wait path; the rest of the
//! line is a NUL-separated argv. A name with `/` is a galfs path
//! (`/hello`, and `./hello` from `/`). A bare name the ramdisk does not
//! have is the file in the current directory (`rustc test.rs` writes
//! `test`; typing `test` runs it). Sample programs are `testing/hello`.
//! A bare program name returns once
//! the load finishes (Cap dropped) and keeps running.
//! A bare launch shares this console. `cmd &` is a background job (`jobs`
//! / `fg`); Ctrl-Z is not implemented. Pipelines are up to four stages
//! joined by ` | `. A `*` word expands to
//! names in the current directory from the files snapshot.
//! `echo`, `cat`, `edit`, `touch`, `mkdir`, `rm`, and `ls` are those utilities.
//! `ls` and `ls -l` take a directory path (`ls Desktop`); with none, they
//! list the current directory.
//! `edit` (and `nano`, the same program) also receives the keyboard grant
//! and Cap-waits, so it can edit.
//! The kernel keeps the status bar and the screen, and loads this shell
//! again if it faults. The current directory lives here and starts over
//! at `/` after a restart. Archive names stay at `/`. A path may begin
//! with `/`, and the first component may be `owner@name` (`/dan@Desktop`).
//! Boot shows a login screen (`Galexy.OS v… (ttyN)`). After login a
//! fastfetch-style dashboard prints, then the prompt is `user@galexy>`
//! (with `:/path` when cwd is not `/`). Up/down arrows recall lines from
//! the session history (loaded from / saved to per-user `shell.history`
//! on login / logout). Left/right move inside the line on the prompt
//! and on every password or login field. Backspace is BS or DEL.
//! `lock` freezes the seat until the same password.
//! Five minutes with no keys does that too. `logout` returns to login.

#![no_std]
#![no_main]

extern crate alloc;

use galexy_abi::{Cap, SysError};
use galexy_rt::{
    arg, entry, keyboard_cap, read, stats_cap, tasks_cap, user, user_login, user_unlock,
    user_unlock_new, user_unlock_session, volume_query, write_console, yield_now,
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
        loop {
            match repl(kbd, &mut cwd, &mut must_change, &mut history) {
                ReplEnd::Logout => {
                    must_change = false;
                    break;
                }
                ReplEnd::Locked => lock_screen(kbd, tty),
                ReplEnd::Die(code) => return code,
            }
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
    let mut fresh = prompt_volume(kbd);
    // A denied keyboard returns here while the disk is still locked.
    // Do not open a login against the RAM table.
    if volume_query() != 0 {
        return None;
    }
    loop {
        write_console(&[0x0c]);
        write_console(b"Galexy.OS v");
        write_console(galexy_abi::OS_VERSION.as_bytes());
        write_console(b" (tty");
        write_tty_digits(tty);
        write_console(b")\n\n");
        if fresh {
            write_console(b"New volume. Log in as admin with the password you just chose.\n\n");
            fresh = false;
        }
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

/// Password prompt for a frozen session. Cwd and history are kept.
fn lock_screen(kbd: Cap, tty: u8) {
    loop {
        write_console(&[0x0c]);
        write_console(b"Galexy.OS v");
        write_console(galexy_abi::OS_VERSION.as_bytes());
        write_console(b" (tty");
        write_tty_digits(tty);
        write_console(b")\n\nSession locked\nUser     ");
        let mut name = [0u8; USER_MAX];
        let got = user(&mut name, galexy_abi::USER_WHOAMI);
        if got.ok && got.value > 0 {
            write_console(&name[..(got.value as usize).min(USER_MAX)]);
        } else {
            write_console(b"?");
        }
        write_console(b"\n\nPassword: ");
        let mut pass = [0u8; PASS_MAX];
        let plen = match read_line(kbd, &mut pass, true) {
            LineRead::Denied => {
                wipe(&mut pass);
                return;
            }
            LineRead::Cancel | LineRead::Overlong | LineRead::Line(0) => {
                wipe(&mut pass);
                continue;
            }
            LineRead::Line(n) => n,
        };
        let result = user_unlock_session(&pass[..plen]);
        wipe(&mut pass);
        if result.ok {
            write_console(b"\n");
            return;
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

/// Ask for the volume passphrase when the disk is locked.
///
/// A disk with no passphrase yet asks the operator to choose one and
/// confirm it. An existing seal asks for that passphrase. Login does
/// not start until the volume is open. An empty line asks again.
/// Returns whether this call sealed a new volume, so the login screen
/// can say what to do next.
fn prompt_volume(kbd: Cap) -> bool {
    match volume_query() {
        2 => prompt_new_volume(kbd),
        1 => {
            prompt_existing_volume(kbd);
            false
        }
        _ => false,
    }
}

fn prompt_new_volume(kbd: Cap) -> bool {
    loop {
        // Another seat may have sealed the disk while this one was
        // still sitting on the first prompt. Do not ask again, and do
        // not tell this seat it just created the volume.
        if volume_query() == 0 {
            return false;
        }
        write_console(b"This disk has no passphrase yet.\n");
        write_console(
            b"Choose a passphrase. It seals the filesystem. Login waits until it is set.\n",
        );
        write_console(b"New passphrase: ");
        let mut pass = [0u8; PASS_MAX];
        let n = match read_line(kbd, &mut pass, true) {
            LineRead::Denied => {
                wipe(&mut pass);
                return false;
            }
            LineRead::Cancel | LineRead::Overlong => {
                wipe(&mut pass);
                continue;
            }
            LineRead::Line(0) => {
                wipe(&mut pass);
                if volume_query() == 0 {
                    write_console(b"\n");
                    return false;
                }
                write_console(b"\nA passphrase is required.\n");
                continue;
            }
            LineRead::Line(n) => n,
        };
        write_console(b"Confirm: ");
        let mut confirm = [0u8; PASS_MAX];
        let c = match read_line(kbd, &mut confirm, true) {
            LineRead::Denied => {
                wipe(&mut pass);
                wipe(&mut confirm);
                return false;
            }
            LineRead::Cancel | LineRead::Overlong | LineRead::Line(0) => {
                wipe(&mut pass);
                wipe(&mut confirm);
                write_console(b"\npassphrases do not match\n");
                continue;
            }
            LineRead::Line(c) => c,
        };
        if n != c || pass[..n] != confirm[..c] {
            wipe(&mut pass);
            wipe(&mut confirm);
            write_console(b"\npassphrases do not match\n");
            continue;
        }
        wipe(&mut confirm);
        if volume_query() == 0 {
            wipe(&mut pass);
            write_console(b"\n");
            return false;
        }
        let mut admin = [0u8; PASS_MAX];
        let Some(admin_len) = prompt_admin_password(kbd, &mut admin) else {
            wipe(&mut pass);
            wipe(&mut admin);
            if volume_query() == 0 {
                write_console(b"\n");
                return false;
            }
            return false;
        };
        if volume_query() == 0 {
            wipe(&mut pass);
            wipe(&mut admin);
            write_console(b"\n");
            return false;
        }
        let result = user_unlock_new(&pass[..n], &admin[..admin_len]);
        wipe(&mut pass);
        wipe(&mut admin);
        if result.ok {
            write_console(b"\n");
            return true;
        }
        write_console(b"\nVolume unlock failed\n");
    }
}

/// Account password for the fresh volume. `None` when the keyboard is
/// denied or another seat already opened the disk.
fn prompt_admin_password(kbd: Cap, out: &mut [u8; PASS_MAX]) -> Option<usize> {
    loop {
        if volume_query() == 0 {
            return None;
        }
        write_console(b"Choose the admin password. The account name is admin.\n");
        write_console(b"Admin password: ");
        let n = match read_line(kbd, out, true) {
            LineRead::Denied => return None,
            LineRead::Cancel | LineRead::Overlong => {
                wipe(out);
                continue;
            }
            LineRead::Line(0) => {
                wipe(out);
                write_console(b"\nA password is required.\n");
                continue;
            }
            LineRead::Line(n) => n,
        };
        if &out[..n] == ADMIN_DEFAULT_PASS {
            wipe(out);
            write_console(b"\nChoose a different password.\n");
            continue;
        }
        write_console(b"Confirm: ");
        let mut confirm = [0u8; PASS_MAX];
        let c = match read_line(kbd, &mut confirm, true) {
            LineRead::Denied => {
                wipe(out);
                wipe(&mut confirm);
                return None;
            }
            LineRead::Cancel | LineRead::Overlong | LineRead::Line(0) => {
                wipe(out);
                wipe(&mut confirm);
                write_console(b"\npasswords do not match\n");
                continue;
            }
            LineRead::Line(c) => c,
        };
        if n != c || out[..n] != confirm[..c] {
            wipe(out);
            wipe(&mut confirm);
            write_console(b"\npasswords do not match\n");
            continue;
        }
        wipe(&mut confirm);
        return Some(n);
    }
}

fn prompt_existing_volume(kbd: Cap) {
    loop {
        if volume_query() == 0 {
            return;
        }
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
                if volume_query() == 0 {
                    write_console(b"\n");
                    return;
                }
                write_console(b"\nA passphrase is required.\n");
                continue;
            }
            LineRead::Line(n) => {
                if volume_query() == 0 {
                    wipe(&mut pass);
                    write_console(b"\n");
                    return;
                }
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
        for_stat_line(&buf[..n], b"tasks:", b"Tasks    ");
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
