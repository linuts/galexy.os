//! grep — print lines that contain a fixed string.
//!
//! Trust: the shell spawns this with `SPAWN_INHERIT` (the caller's galfs
//! cards). The ramdisk is trusted code. The pattern is a byte string, not
//! a regular expression. It lives on the user heap.

#![no_std]
#![no_main]

extern crate alloc;

use alloc::vec::Vec;

use galexy_abi::{Cap, CapRights, SysError, FILE_CAP_BASE};
use galexy_rt::{args, close, entry, open, read, write_console, write_std};

entry!(main);

fn main() -> i32 {
    let mut it = args();
    let Some(pattern) = it.next() else {
        let _ = write_console(b"grep: usage: grep <text> [name]\n");
        return 1;
    };
    if pattern.is_empty() {
        let _ = write_console(b"grep: usage: grep <text> [name]\n");
        return 1;
    }
    let pattern = Vec::from(pattern);
    let mut files: Vec<Vec<u8>> = Vec::new();
    for path in it {
        files.push(Vec::from(path));
    }
    if files.is_empty() {
        return scan(b"-", &pattern);
    }
    let mut code = 0;
    for path in &files {
        if scan(path, &pattern) != 0 {
            code = 1;
        }
    }
    code
}

fn scan(path: &[u8], pattern: &[u8]) -> i32 {
    let cap = if path == b"-" {
        Cap::new(FILE_CAP_BASE, CapRights::READ)
    } else {
        let opened = open(path);
        if !opened.ok {
            if opened.value == SysError::NotFound as u64 {
                let _ = write_console(b"grep: no such file\n");
            } else {
                let _ = write_console(b"grep: failed\n");
            }
            return 1;
        }
        Cap::from_bits(opened.value)
    };
    let mut line = Vec::new();
    let mut buf = [0u8; 256];
    let mut code = 0;
    loop {
        let got = read(cap, &mut buf);
        if !got.ok {
            let _ = write_console(b"grep: failed\n");
            code = 1;
            break;
        }
        if got.value == 0 {
            if !line.is_empty() {
                emit(&line, pattern);
            }
            break;
        }
        let n = (got.value as usize).min(buf.len());
        for &byte in &buf[..n] {
            if byte == b'\n' {
                emit(&line, pattern);
                line.clear();
            } else if line.len() < 512 {
                line.push(byte);
            }
        }
    }
    if path != b"-" {
        let _ = close(cap);
    }
    code
}

fn emit(line: &[u8], pattern: &[u8]) {
    if !contains(line, pattern) {
        return;
    }
    let _ = write_std(line);
    let _ = write_std(b"\n");
}

fn contains(hay: &[u8], needle: &[u8]) -> bool {
    if needle.is_empty() || needle.len() > hay.len() {
        return false;
    }
    hay.windows(needle.len()).any(|w| w == needle)
}
