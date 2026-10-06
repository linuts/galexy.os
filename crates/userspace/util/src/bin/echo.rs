//! echo — print text, or write it into a scratch file.
//!
//! Argument: mode byte, path length, path, then the text. Mode 0 prints.
//! Mode 1 replaces a scratch file. Mode 2 appends.

#![no_std]
#![no_main]

use galexy_abi::{Cap, SysError};
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
