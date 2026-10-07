//! truncate — set a galfs file's length.

#![no_std]
#![no_main]

use galexy_abi::Cap;
use galexy_rt::{arg, close, create, entry, open, truncate, write_console};

entry!(main);

fn main() -> i32 {
    let raw = arg();
    let Some(nul) = raw.iter().position(|b| *b == 0) else {
        write_console(b"truncate: usage: truncate <path> <size>\n");
        return 1;
    };
    let path = &raw[..nul];
    let size_bytes = &raw[nul + 1..];
    if path.is_empty() || size_bytes.is_empty() {
        write_console(b"truncate: usage: truncate <path> <size>\n");
        return 1;
    }
    let Some(size) = parse_u64(size_bytes) else {
        write_console(b"truncate: bad size\n");
        return 1;
    };

    let opened = open(path);
    let cap = if opened.ok {
        Cap::from_bits(opened.value)
    } else {
        let created = create(path);
        if !created.ok {
            write_console(b"truncate: cannot open\n");
            return 1;
        }
        Cap::from_bits(created.value)
    };

    let got = truncate(cap, size);
    let _ = close(cap);
    if !got.ok {
        write_console(b"truncate: failed\n");
        return 1;
    }
    0
}

fn parse_u64(bytes: &[u8]) -> Option<u64> {
    if bytes.is_empty() {
        return None;
    }
    let mut n = 0u64;
    for &b in bytes {
        if !b.is_ascii_digit() {
            return None;
        }
        n = n.checked_mul(10)?.checked_add((b - b'0') as u64)?;
    }
    Some(n)
}
