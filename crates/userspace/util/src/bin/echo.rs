//! echo — print text, write it into a galfs file, or push it into a pipe.
//!
//! Argument: mode byte, path length, path, then the text. Mode 0 prints.
//! Mode 1 replaces a galfs file. Mode 2 appends. Mode 3 writes the text
//! plus a newline to the pipe spawn installed on the first file slot.

#![no_std]
#![no_main]

use galexy_abi::{Cap, CapRights, SysError, FILE_CAP_BASE};
use galexy_rt::{arg, close, create, create_replace, entry, open, write, write_console};

entry!(main);

fn main() -> i32 {
    let bytes = arg();
    if bytes.len() < 2 {
        write_console(b"echo: failed\n");
        return 1;
    }
    let mode = bytes[0];
    let path_len = bytes[1] as usize;
    if bytes.len() < 2 + path_len {
        write_console(b"echo: failed\n");
        return 1;
    }
    let path = &bytes[2..2 + path_len];
    let text = &bytes[2 + path_len..];
    if mode == 0 {
        write_console(text);
        write_console(b"\n");
        return 0;
    }
    if mode == 3 {
        return write_pipe(text);
    }
    if path.is_empty() {
        write_console(b"echo: usage: echo [text] > name\n");
        return 1;
    }
    let opened = if mode == 2 {
        let existing = open(path);
        if existing.ok {
            existing
        } else if existing.value == SysError::NotFound as u64 {
            create(path)
        } else {
            existing
        }
    } else if mode == 1 {
        create_replace(path)
    } else {
        write_console(b"echo: failed\n");
        return 1;
    };
    if !opened.ok {
        if opened.value == SysError::Unsupported as u64 {
            write_console(b"echo: cannot replace\n");
        } else if opened.value == SysError::NotFound as u64 {
            write_console(b"echo: no such directory\n");
        } else {
            write_console(b"echo: failed\n");
        }
        return 1;
    }
    let cap = Cap::from_bits(opened.value);
    let mut payload = [0u8; 81];
    let ncopy = text.len().min(80);
    payload[..ncopy].copy_from_slice(&text[..ncopy]);
    payload[ncopy] = b'\n';
    let wrote = write(cap, &payload[..=ncopy]);
    let _ = close(cap);
    if !wrote.ok || wrote.value != (ncopy + 1) as u64 {
        write_console(b"echo: failed\n");
        return 1;
    }
    0
}

/// Writes `text` and a newline to the pipe end spawn already installed.
fn write_pipe(text: &[u8]) -> i32 {
    let cap = Cap::new(FILE_CAP_BASE, CapRights::WRITE);
    let mut payload = [0u8; 81];
    let n = text.len().min(80);
    payload[..n].copy_from_slice(&text[..n]);
    payload[n] = b'\n';
    let bytes = &payload[..=n];
    let wrote = write(cap, bytes);
    let _ = close(cap);
    if wrote.ok && wrote.value == bytes.len() as u64 {
        return 0;
    }
    write_console(b"echo: failed\n");
    1
}
