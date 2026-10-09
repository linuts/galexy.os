//! cat — copy a file, or a pipe spawn installed, to the console.
//!
//! Argument `-` reads the pipe spawn installed on the first file slot.

#![no_std]
#![no_main]

use galexy_abi::{Cap, CapRights, SysError, FILE_CAP_BASE};
use galexy_rt::{arg, close, entry, open, read, write_console};

entry!(main);

fn main() -> i32 {
    let path = arg();
    if path == b"-" {
        return cat_pipe();
    }
    if path.is_empty() {
        write_console(b"cat: usage: cat <name>\n");
        return 1;
    }
    let opened = open(path);
    if !opened.ok {
        if opened.value == SysError::NotFound as u64 {
            write_console(b"cat: no such file\n");
        } else {
            write_console(b"cat: failed\n");
        }
        return 1;
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
            return 1;
        }
        ended_nl = chunk.last() == Some(&b'\n');
    }
    let _ = close(cap);
    if !ended_nl {
        write_console(b"\n");
    }
    0
}

/// Copies the pipe spawn installed at [`FILE_CAP_BASE`].
fn cat_pipe() -> i32 {
    let cap = Cap::new(FILE_CAP_BASE, CapRights::READ);
    let mut buf = [0u8; 256];
    let mut ended_nl = true;
    let mut any = false;
    loop {
        let got = read(cap, &mut buf);
        if !got.ok {
            write_console(b"cat: failed\n");
            return 1;
        }
        if got.value == 0 {
            break;
        }
        any = true;
        let chunk = &buf[..(got.value as usize).min(buf.len())];
        let wrote = write_console(chunk);
        if !wrote.ok {
            write_console(b"\ncat: not text\n");
            let _ = close(cap);
            return 1;
        }
        ended_nl = chunk.last() == Some(&b'\n');
    }
    let _ = close(cap);
    if any && !ended_nl {
        write_console(b"\n");
    }
    0
}
