//! ls — list the current directory. The argument is that path, or empty at `/`.
//!
//! Trust: the shell spawns this with `SPAWN_INHERIT` (the caller's galfs
//! cards) and the files snapshot. The ramdisk is trusted code.
//!
//! `ls -l` (optional NUL and the same directory path) prints `f <size> name`
//! or `d <size> name` from `stat`. A name `stat` cannot read is `? 0 name`.
//! Plain `ls` is unchanged.

#![no_std]
#![no_main]

use galexy_abi::{STAT_DIR, STAT_FILE, STAT_LEN};
use galexy_rt::{arg, entry, files_cap, read, stat, write_console, write_std};

entry!(main);

fn main() -> i32 {
    let raw = arg();
    if raw.starts_with(b"-l") && (raw.len() == 2 || raw.get(2) == Some(&0)) {
        let cwd = if raw.len() > 3 { &raw[3..] } else { &[] };
        return ls_long(trim_nul(cwd));
    }
    ls_plain(raw)
}

fn trim_nul(bytes: &[u8]) -> &[u8] {
    match bytes.iter().position(|b| *b == 0) {
        Some(i) => &bytes[..i],
        None => bytes,
    }
}

fn ls_plain(cwd: &[u8]) -> i32 {
    let mut buf = [0u8; 1024];
    let got = read(files_cap(), &mut buf);
    if !got.ok {
        let _ = write_console(b"ls: denied\n");
        return 1;
    }
    let n = (got.value as usize).min(buf.len());
    let mut i = 0usize;
    while i < n {
        let rest = &buf[i..n];
        let end = rest.iter().position(|b| *b == b'\n').unwrap_or(rest.len());
        if let Some(shown) = ls_name(cwd, &rest[..end]) {
            let _ = write_std(shown);
            let _ = write_std(b"\n");
        }
        i += end + 1;
    }
    0
}

fn ls_long(cwd: &[u8]) -> i32 {
    let mut buf = [0u8; 1024];
    let got = read(files_cap(), &mut buf);
    if !got.ok {
        let _ = write_console(b"ls: denied\n");
        return 1;
    }
    let n = (got.value as usize).min(buf.len());
    let mut i = 0usize;
    while i < n {
        let rest = &buf[i..n];
        let end = rest.iter().position(|b| *b == b'\n').unwrap_or(rest.len());
        if let Some(shown) = ls_name(cwd, &rest[..end]) {
            print_long(cwd, shown);
        }
        i += end + 1;
    }
    0
}

fn print_long(cwd: &[u8], shown: &[u8]) {
    let mut path = [0u8; 96];
    let plen = compose(cwd, shown, &mut path);
    let mut rec = [0u8; STAT_LEN];
    let got = if plen == 0 {
        None
    } else {
        Some(stat(&path[..plen], &mut rec))
    };
    let (kind, size) = if got.is_some_and(|got| got.ok && got.value == STAT_LEN as u64) {
        let size = u32::from_le_bytes([rec[4], rec[5], rec[6], rec[7]]);
        let kind = match rec[0] {
            STAT_FILE => b'f',
            STAT_DIR => b'd',
            _ => b'?',
        };
        (kind, size)
    } else {
        (b'?', 0)
    };
    let _ = write_std(&[kind]);
    let _ = write_std(b" ");
    write_u32(size);
    let _ = write_std(b" ");
    let _ = write_std(shown);
    let _ = write_std(b"\n");
}

fn compose(cwd: &[u8], shown: &[u8], out: &mut [u8]) -> usize {
    if cwd.is_empty() {
        if shown.len() > out.len() {
            return 0;
        }
        out[..shown.len()].copy_from_slice(shown);
        return shown.len();
    }
    let need = cwd.len() + 1 + shown.len();
    if need > out.len() {
        return 0;
    }
    out[..cwd.len()].copy_from_slice(cwd);
    out[cwd.len()] = b'/';
    out[cwd.len() + 1..need].copy_from_slice(shown);
    need
}

fn write_u32(mut n: u32) {
    if n == 0 {
        let _ = write_std(b"0");
        return;
    }
    let mut tmp = [0u8; 10];
    let mut i = tmp.len();
    while n > 0 {
        i -= 1;
        tmp[i] = b'0' + (n % 10) as u8;
        n /= 10;
    }
    let _ = write_std(&tmp[i..]);
}

fn ls_name<'a>(cwd: &[u8], line: &'a [u8]) -> Option<&'a [u8]> {
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
