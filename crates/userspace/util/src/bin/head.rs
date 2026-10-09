//! head — first 10 lines of a file, or of the pipe spawn installed.
//!
//! Trust: the shell spawns this with `SPAWN_INHERIT` (the caller's galfs
//! cards). The ramdisk is trusted code.

#![no_std]
#![no_main]

use galexy_abi::{Cap, CapRights, SysError, FILE_CAP_BASE};
use galexy_rt::{args, close, entry, open, read, write_console, write_std};

entry!(main);

const LINES: usize = 10;

fn main() -> i32 {
    let mut any = false;
    let mut code = 0;
    for path in args() {
        any = true;
        if show(path) != 0 {
            code = 1;
        }
    }
    if !any {
        let _ = write_console(b"head: usage: head <name>\n");
        return 1;
    }
    code
}

fn show(path: &[u8]) -> i32 {
    if path == b"-" {
        return copy_cap(Cap::new(FILE_CAP_BASE, CapRights::READ), true);
    }
    let opened = open(path);
    if !opened.ok {
        if opened.value == SysError::NotFound as u64 {
            let _ = write_console(b"head: no such file\n");
        } else {
            let _ = write_console(b"head: failed\n");
        }
        return 1;
    }
    let cap = Cap::from_bits(opened.value);
    let code = copy_cap(cap, false);
    let _ = close(cap);
    code
}

fn copy_cap(cap: Cap, pipe: bool) -> i32 {
    let mut buf = [0u8; 256];
    let mut lines = 0usize;
    loop {
        if lines >= LINES {
            break;
        }
        let got = read(cap, &mut buf);
        if !got.ok {
            if pipe {
                let _ = write_console(b"head: failed\n");
                return 1;
            }
            break;
        }
        if got.value == 0 {
            break;
        }
        let mut n = (got.value as usize).min(buf.len());
        let mut emit = n;
        for (i, byte) in buf[..n].iter().enumerate() {
            if *byte == b'\n' {
                lines += 1;
                if lines >= LINES {
                    emit = i + 1;
                    n = emit;
                    break;
                }
            }
        }
        let wrote = write_std(&buf[..emit]);
        if !wrote.ok {
            let _ = write_console(b"head: failed\n");
            return 1;
        }
        if emit < n {
            break;
        }
    }
    0
}
