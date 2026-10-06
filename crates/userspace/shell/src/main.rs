//! The interactive shell: a ring-3 program.
//!
//! Keys arrive through the keyboard capability. Programs start through the
//! loader capability, and this task stays parked until they exit. The
//! kernel keeps the status bar and the screen.
//!
//! The current directory lives here. Archive names stay at `/`. A program
//! name on its own starts that program.

#![no_std]
#![no_main]

use galexy_abi::{Cap, SysError};
use galexy_rt::{
    close, create, create_replace, entry, files_cap, keyboard_cap, open, read, reboot, remove,
    shutdown, spawn, stats_cap, tasks_cap, threads_cap, write, write_console, yield_now,
};

entry!(main);

const LINE_MAX: usize = 80;
const PATH_MAX: usize = 64;

struct Cwd {
    buf: [u8; PATH_MAX],
    len: usize,
}

fn main() -> i32 {
    let kbd = keyboard_cap();
    let mut line = [0u8; LINE_MAX];
    let mut len = 0usize;
    let mut cwd = Cwd {
        buf: [0; PATH_MAX],
        len: 0,
    };
    prompt(&cwd);
    loop {
        let mut buf = [0u8; 8];
        let got = read(kbd, &mut buf);
        if !got.ok {
            write_console(b"\nread: keyboard denied\n");
            prompt(&cwd);
            continue;
        }
        if got.value == 0 {
            yield_now();
            continue;
        }
        let n = got.value as usize;
        for &byte in &buf[..n.min(buf.len())] {
            match byte {
                b'\n' | b'\r' => {
                    write_console(b"\n");
                    dispatch(trim(&line[..len]), &mut cwd);
                    len = 0;
                }
                0x08 => {
                    if len > 0 {
                        len -= 1;
                        write_console(&[0x08]);
                    }
                }
                b if (b.is_ascii_graphic() || b == b' ') && len < LINE_MAX => {
                    line[len] = b;
                    len += 1;
                    write_console(&[b]);
                }
                _ => {}
            }
        }
    }
}

fn dispatch(line: &[u8], cwd: &mut Cwd) {
    if line.is_empty() {
        prompt(cwd);
        return;
    }
    if line == b"help" {
        write_console(b"commands: help, ls, echo, cat, touch, mkdir, cd, rm,\n");
        write_console(b"stats, tasks, threads, about, clear\n");
        write_console(b"a program name on its own starts it\n");
        write_console(b"power: shutdown, reboot\n");
        prompt(cwd);
        return;
    }
    if line == b"about" {
        write_console(b"galexy.os - a small Rust OS\n");
        write_console(b"this shell is a ring-3 program\n");
        prompt(cwd);
        return;
    }
    if line == b"ls" {
        ls(cwd);
        return;
    }
    if line == b"stats" {
        show(stats_cap(), cwd);
        return;
    }
    if line == b"tasks" {
        show(tasks_cap(), cwd);
        return;
    }
    if line == b"threads" {
        show(threads_cap(), cwd);
        return;
    }
    if line == b"shutdown" {
        let _ = shutdown();
        write_console(b"shutdown: the machine stayed up\n");
        prompt(cwd);
        return;
    }
    if line == b"reboot" {
        let _ = reboot();
        write_console(b"reboot: the machine stayed up\n");
        prompt(cwd);
        return;
    }
    if line == b"clear" {
        write_console(&[0x0c]);
        prompt(cwd);
        return;
    }
    if let Some(name) = arg_of(line, b"echo") {
        echo(cwd, name);
        return;
    }
    if let Some(name) = arg_of(line, b"cat") {
        cat(cwd, name);
        return;
    }
    if let Some(name) = arg_of(line, b"touch") {
        touch(cwd, name);
        return;
    }
    if let Some(name) = arg_of(line, b"mkdir") {
        mkdir(cwd, name);
        return;
    }
    if let Some(name) = arg_of(line, b"cd") {
        cd(cwd, name);
        return;
    }
    if let Some(name) = arg_of(line, b"rm") {
        rm(cwd, name);
        return;
    }
    if !line.contains(&b' ') {
        launch(cwd, line);
        return;
    }
    write_console(line);
    write_console(b": command not found\n");
    prompt(cwd);
}

fn echo(cwd: &Cwd, rest: &[u8]) {
    if let Some((text, name, append)) = redirection(rest) {
        write_redir(cwd, text, name, append);
        return;
    }
    write_console(rest);
    write_console(b"\n");
    prompt(cwd);
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

fn write_redir(cwd: &Cwd, text: &[u8], name: &[u8], append: bool) {
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
    let opened = if append {
        let existing = open(&path[..n]);
        if existing.ok {
            existing
        } else if existing.value == SysError::NotFound as u64 {
            create(&path[..n])
        } else {
            existing
        }
    } else {
        create_replace(&path[..n])
    };
    if !opened.ok {
        if opened.value == SysError::Unsupported as u64 {
            write_console(b"echo: cannot replace\n");
        } else if opened.value == SysError::NotFound as u64 {
            write_console(b"echo: no such directory\n");
        } else {
            write_console(b"echo: failed\n");
        }
        prompt(cwd);
        return;
    }
    let cap = Cap::from_bits(opened.value);
    let mut payload = [0u8; LINE_MAX + 1];
    let ncopy = text.len().min(LINE_MAX);
    payload[..ncopy].copy_from_slice(&text[..ncopy]);
    payload[ncopy] = b'\n';
    let wrote = write(cap, &payload[..=ncopy]);
    let _ = close(cap);
    if !wrote.ok || wrote.value != (ncopy + 1) as u64 {
        write_console(b"echo: failed\n");
    }
    prompt(cwd);
}

fn cat(cwd: &Cwd, name: &[u8]) {
    let name = trim(name);
    if name.is_empty() || name.contains(&b' ') || name.contains(&b'/') {
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
    let opened = open(&path[..n]);
    if !opened.ok {
        if opened.value == SysError::NotFound as u64 {
            write_console(b"cat: no such file\n");
        } else {
            write_console(b"cat: failed\n");
        }
        prompt(cwd);
        return;
    }
    let cap = Cap::from_bits(opened.value);
    let mut buf = [0u8; 256];
    let mut ended_nl = true;
    loop {
        let got = read(cap, &mut buf);
        if !got.ok || got.value == 0 {
            break;
        }
        let chunk = &buf[..(got.value as usize).min(buf.len())];
        let wrote = write_console(chunk);
        if !wrote.ok {
            write_console(b"\ncat: not text\n");
            let _ = close(cap);
            prompt(cwd);
            return;
        }
        ended_nl = chunk.last() == Some(&b'\n');
    }
    let _ = close(cap);
    if !ended_nl {
        write_console(b"\n");
    }
    prompt(cwd);
}

fn touch(cwd: &Cwd, name: &[u8]) {
    let name = trim(name);
    if name.is_empty() || name.contains(&b' ') || name.contains(&b'/') {
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
    let made = create(&path[..n]);
    if made.ok {
        if made.value != 0 {
            let _ = close(Cap::from_bits(made.value));
        }
        prompt(cwd);
        return;
    }
    if made.value == SysError::Unsupported as u64 && snapshot_has(&path[..n]) {
        prompt(cwd);
        return;
    }
    if made.value == SysError::Unsupported as u64 {
        write_console(b"touch: cannot replace\n");
    } else if made.value == SysError::NotFound as u64 {
        write_console(b"touch: no such directory\n");
    } else {
        write_console(b"touch: failed\n");
    }
    prompt(cwd);
}

fn mkdir(cwd: &Cwd, name: &[u8]) {
    let name = trim(name);
    if name.is_empty() || name.contains(&b' ') || name.contains(&b'/') {
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
    let made = create(&path[..n]);
    if !made.ok {
        if made.value == SysError::Unsupported as u64 {
            write_console(b"mkdir: cannot replace\n");
        } else if made.value == SysError::NotFound as u64 {
            write_console(b"mkdir: no such directory\n");
        } else {
            write_console(b"mkdir: failed\n");
        }
    }
    prompt(cwd);
}

fn rm(cwd: &Cwd, name: &[u8]) {
    let name = trim(name);
    if name.is_empty()
        || name.contains(&b' ')
        || name.contains(&b'/')
        || name == b"."
        || name == b".."
    {
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
    let removed = remove(&path[..n]);
    if !removed.ok {
        if removed.value == SysError::NotFound as u64 {
            write_console(b"rm: no such file\n");
        } else if removed.value == SysError::Unsupported as u64 && snapshot_has_child(&path[..n]) {
            write_console(b"rm: directory not empty\n");
        } else if removed.value == SysError::Unsupported as u64 {
            write_console(b"rm: cannot remove\n");
        } else {
            write_console(b"rm: failed\n");
        }
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
    if name.contains(&b'/') || name.contains(&b' ') {
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
    let mut buf = [0u8; 1024];
    let got = read(files_cap(), &mut buf);
    if !got.ok {
        write_console(b"ls: denied\n");
        prompt(cwd);
        return;
    }
    let n = (got.value as usize).min(buf.len());
    let mut i = 0usize;
    while i < n {
        let rest = &buf[i..n];
        let end = rest.iter().position(|b| *b == b'\n').unwrap_or(rest.len());
        if let Some(shown) = ls_name(cwd, &rest[..end]) {
            write_console(shown);
            write_console(b"\n");
        }
        i += end + 1;
    }
    prompt(cwd);
}

fn ls_name<'a>(cwd: &Cwd, line: &'a [u8]) -> Option<&'a [u8]> {
    if line.is_empty() {
        return None;
    }
    if cwd.len == 0 {
        let slashes = line.iter().filter(|b| **b == b'/').count();
        if slashes == 0 || (slashes == 1 && line.ends_with(b"/")) {
            return Some(line);
        }
        return None;
    }
    if line.len() <= cwd.len + 1 {
        return None;
    }
    if line[..cwd.len] != cwd.buf[..cwd.len] || line[cwd.len] != b'/' {
        return None;
    }
    let rest = &line[cwd.len + 1..];
    let slashes = rest.iter().filter(|b| **b == b'/').count();
    if slashes == 0 || (slashes == 1 && rest.ends_with(b"/")) {
        Some(rest)
    } else {
        None
    }
}

/// Starts the ramdisk program `name`. A missing name, or a file that is
/// not an ELF, is reported as an unknown command.
fn launch(cwd: &Cwd, name: &[u8]) {
    let result = spawn(name);
    if !result.ok {
        if result.value == SysError::NoResource as u64 {
            write_console(b"a program is already starting\n");
        } else {
            write_console(name);
            write_console(b": command not found\n");
        }
    }
    prompt(cwd);
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

/// `cwd/name`, plus a trailing `/` when `dir`.
fn compose(cwd: &Cwd, name: &[u8], dir: bool, out: &mut [u8; PATH_MAX]) -> Option<usize> {
    let extra = usize::from(dir);
    let need = if cwd.len == 0 {
        name.len() + extra
    } else {
        cwd.len + 1 + name.len() + extra
    };
    if need == 0 || need > PATH_MAX {
        return None;
    }
    let mut n = 0usize;
    if cwd.len > 0 {
        out[..cwd.len].copy_from_slice(&cwd.buf[..cwd.len]);
        out[cwd.len] = b'/';
        n = cwd.len + 1;
    }
    out[n..n + name.len()].copy_from_slice(name);
    n += name.len();
    if dir {
        out[n] = b'/';
        n += 1;
    }
    Some(n)
}

/// True when some snapshot line is strictly inside `dir` (`box/leaf`).
fn snapshot_has_child(dir: &[u8]) -> bool {
    if dir.len() + 1 > PATH_MAX {
        return false;
    }
    let mut prefix = [0u8; PATH_MAX + 1];
    prefix[..dir.len()].copy_from_slice(dir);
    prefix[dir.len()] = b'/';
    let prefix = &prefix[..=dir.len()];
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
        let line = &rest[..end];
        if line.starts_with(prefix) && line.len() > prefix.len() {
            return true;
        }
        i += end + 1;
    }
    false
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
    write_console(b"galexy");
    if cwd.len > 0 {
        write_console(b":/");
        write_console(&cwd.buf[..cwd.len]);
    }
    write_console(b"> ");
}
