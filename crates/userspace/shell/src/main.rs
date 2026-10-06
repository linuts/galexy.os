//! The interactive shell: a ring-3 program.
//!
//! Keys arrive through the keyboard capability. A program name starts that
//! program through the loader capability; this task is parked only until
//! the load finishes, then the prompt returns while the program runs.
//! `echo`, `cat`, `touch`, `mkdir`, `rm`, and `ls` are those programs.
//! The kernel keeps the status bar and the screen, and loads this shell
//! again if it faults. The current directory lives here and starts over
//! at `/` after a restart. Archive names stay at `/`. A path may begin
//! with `/`, and the first component may be `owner@name` (`/dan@Desktop`).

#![no_std]
#![no_main]

use galexy_abi::{Cap, SysError};
use galexy_rt::{
    entry, files_cap, grant, keyboard_cap, read, reboot, revoke, shutdown, spawn_with, stats_cap,
    tasks_cap, threads_cap, write_console, yield_now,
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
    if line == b"crash" {
        // Test seam: a null read kills this task. The kernel loads a new shell.
        unsafe {
            core::ptr::read_volatile(core::ptr::null::<u8>());
        }
        return;
    }
    if line == b"help" {
        write_console(b"commands: help, ls, echo, cat, touch, mkdir, cd, rm,\n");
        write_console(b"cp, mv, grant, revoke, stats, tasks, threads, about, clear\n");
        write_console(b"a program name on its own starts it\n");
        write_console(b"grant/revoke: <rights> <path> <task>  (r w l c x)\n");
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
    if let Some(rest) = arg_of(line, b"grant") {
        do_token(cwd, rest, true);
        return;
    }
    if let Some(rest) = arg_of(line, b"revoke") {
        do_token(cwd, rest, false);
        return;
    }
    if let Some(rest) = arg_of(line, b"cp") {
        two_path_util(cwd, b"cp", rest);
        return;
    }
    if let Some(rest) = arg_of(line, b"mv") {
        two_path_util(cwd, b"mv", rest);
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

/// `grant`/`revoke` `<rights> <path> <task>`. Rights: `r`/`w`/`l`/`c`/`x`.
fn do_token(cwd: &Cwd, rest: &[u8], is_grant: bool) {
    let rest = trim(rest);
    let usage = if is_grant {
        &b"usage: grant <rights> <path> <task>\n"[..]
    } else {
        &b"usage: revoke <rights> <path> <task>\n"[..]
    };
    let Some(sp1) = rest.iter().position(|b| *b == b' ') else {
        write_console(usage);
        prompt(cwd);
        return;
    };
    let rights_s = &rest[..sp1];
    let after = trim(&rest[sp1 + 1..]);
    let Some(sp2) = after.iter().position(|b| *b == b' ') else {
        write_console(usage);
        prompt(cwd);
        return;
    };
    let path = trim(&after[..sp2]);
    let task = trim(&after[sp2 + 1..]);
    if rights_s.is_empty() || path.is_empty() || task.is_empty() || !path_arg_ok(path) {
        write_console(usage);
        prompt(cwd);
        return;
    }
    if !task
        .iter()
        .all(|b| b.is_ascii_alphanumeric() || *b == b'.' || *b == b'_' || *b == b'-')
    {
        write_console(b"bad task name\n");
        prompt(cwd);
        return;
    }
    let mut rights = 0u64;
    for &b in rights_s {
        rights |= match b {
            b'r' | b'R' => galexy_abi::TOKEN_READ,
            b'w' | b'W' => galexy_abi::TOKEN_WRITE,
            b'l' | b'L' => galexy_abi::TOKEN_LIST,
            b'c' | b'C' => galexy_abi::TOKEN_CREATE,
            b'x' | b'X' => galexy_abi::TOKEN_REMOVE,
            _ => {
                write_console(b"rights are r,w,l,c,x\n");
                prompt(cwd);
                return;
            }
        };
    }
    if rights == 0 {
        write_console(usage);
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
        grant(&full[..n], rights, task)
    } else {
        revoke(&full[..n], rights, task)
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
/// not an ELF, is reported as an unknown command.
fn launch(cwd: &Cwd, name: &[u8]) {
    launch_util(cwd, name, b"", false);
}

/// Spawns `program` with `arg`. `query` adds the files snapshot grant.
/// The prompt returns once the program is running.
fn launch_util(cwd: &Cwd, program: &[u8], arg: &[u8], query: bool) {
    let grants = if query {
        galexy_abi::SPAWN_GRANT_QUERY
    } else {
        0
    };
    let result = spawn_with(program, arg, grants);
    if !result.ok {
        if result.value == SysError::NoResource as u64 {
            write_console(b"a program is already starting\n");
        } else {
            write_console(program);
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
    // `galexy` and `> `.
    let mut line = [0u8; 6 + 2 + PATH_MAX + 2];
    line[..6].copy_from_slice(b"galexy");
    let mut n = 6usize;
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
