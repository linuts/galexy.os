//! cat — copy files, or a pipe spawn installed, to stdout.
//!
//! Trust: the shell spawns this with `SPAWN_INHERIT` (the caller's galfs
//! cards). The ramdisk is trusted code.
//!
//! Arguments are NUL-separated paths. `-` reads the pipe spawn installed
//! on the first file slot. Stdout is that pipe's peer when this program
//! is a pipeline stage, and the console otherwise.

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
        if path == b"-" {
            if cat_pipe() != 0 {
                code = 1;
            }
        } else if cat_file(path) != 0 {
            code = 1;
        }
    }
    if !any {
        let _ = write_console(b"cat: usage: cat <name>\n");
        return 1;
    }
    code
}

fn cat_file(path: &[u8]) -> i32 {
    let opened = open(path);
    if !opened.ok {
        if opened.value == SysError::NotFound as u64 {
            let _ = write_console(b"cat: no such file\n");
        } else {
            let _ = write_console(b"cat: failed\n");
        }
        return 1;
    }
    let cap = Cap::from_bits(opened.value);
    let code = copy(cap, false);
    let _ = close(cap);
    code
}

/// Copies the pipe spawn installed at [`FILE_CAP_BASE`].
fn cat_pipe() -> i32 {
    copy(Cap::new(FILE_CAP_BASE, CapRights::READ), true)
}

fn copy(cap: Cap, pipe: bool) -> i32 {
    let mut buf = [0u8; 256];
    let mut ended_nl = true;
    let mut any = false;
    loop {
        let got = read(cap, &mut buf);
        if !got.ok {
            if pipe {
                let _ = write_console(b"cat: failed\n");
                return 1;
            }
            break;
        }
        if got.value == 0 {
            break;
        }
        any = true;
        let chunk = &buf[..(got.value as usize).min(buf.len())];
        let wrote = write_std(chunk);
        if !wrote.ok {
            let _ = write_console(b"\ncat: not text\n");
            return 1;
        }
        ended_nl = chunk.last() == Some(&b'\n');
    }
    if any && !ended_nl {
        let _ = write_std(b"\n");
    }
    0
}
