//! wc — line, word, and byte counts.
//!
//! Trust: the shell spawns this with `SPAWN_INHERIT` (the caller's galfs
//! cards). The ramdisk is trusted code.

#![no_std]
#![no_main]

use galexy_abi::{Cap, CapRights, SysError, FILE_CAP_BASE};
use galexy_rt::{args, close, entry, open, read, write_console, write_std};

entry!(main);

fn main() -> i32 {
    let mut any = false;
    let mut code = 0;
    for path in args() {
        any = true;
        if count(path) != 0 {
            code = 1;
        }
    }
    if !any {
        let _ = write_console(b"wc: usage: wc <name>\n");
        return 1;
    }
    code
}

fn count(path: &[u8]) -> i32 {
    let cap = if path == b"-" {
        Cap::new(FILE_CAP_BASE, CapRights::READ)
    } else {
        let opened = open(path);
        if !opened.ok {
            if opened.value == SysError::NotFound as u64 {
                let _ = write_console(b"wc: no such file\n");
            } else {
                let _ = write_console(b"wc: failed\n");
            }
            return 1;
        }
        Cap::from_bits(opened.value)
    };
    let mut lines = 0u64;
    let mut words = 0u64;
    let mut bytes = 0u64;
    let mut in_word = false;
    let mut buf = [0u8; 256];
    loop {
        let got = read(cap, &mut buf);
        if !got.ok {
            if path != b"-" {
                let _ = close(cap);
            }
            let _ = write_console(b"wc: failed\n");
            return 1;
        }
        if got.value == 0 {
            break;
        }
        let n = (got.value as usize).min(buf.len());
        bytes += n as u64;
        for byte in &buf[..n] {
            if *byte == b'\n' {
                lines += 1;
            }
            if byte.is_ascii_whitespace() {
                in_word = false;
            } else if !in_word {
                in_word = true;
                words += 1;
            }
        }
    }
    if path != b"-" {
        let _ = close(cap);
    }
    write_u64(lines);
    let _ = write_std(b" ");
    write_u64(words);
    let _ = write_std(b" ");
    write_u64(bytes);
    let _ = write_std(b" ");
    let _ = write_std(path);
    let _ = write_std(b"\n");
    0
}

fn write_u64(mut n: u64) {
    if n == 0 {
        let _ = write_std(b"0");
        return;
    }
    let mut tmp = [0u8; 20];
    let mut i = tmp.len();
    while n > 0 {
        i -= 1;
        tmp[i] = b'0' + (n % 10) as u8;
        n /= 10;
    }
    let _ = write_std(&tmp[i..]);
}
