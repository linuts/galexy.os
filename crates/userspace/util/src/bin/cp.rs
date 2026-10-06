//! cp — copy a galfs or ramdisk file to a new galfs path.

#![no_std]
#![no_main]

use galexy_abi::{Cap, SysError};
use galexy_rt::{arg, close, create_replace, entry, open, read, write, write_console};

entry!(main);

fn main() -> i32 {
    let raw = arg();
    let Some(nul) = raw.iter().position(|b| *b == 0) else {
        write_console(b"cp: usage: cp <src> <dst>\n");
        return 1;
    };
    let src = &raw[..nul];
    let dst = &raw[nul + 1..];
    if src.is_empty() || dst.is_empty() {
        write_console(b"cp: usage: cp <src> <dst>\n");
        return 1;
    }
    let opened = open(src);
    if !opened.ok {
        write_console(b"cp: cannot open source\n");
        return 1;
    }
    let src_cap = Cap::from_bits(opened.value);
    let created = create_replace(dst);
    if !created.ok {
        let _ = close(src_cap);
        if created.value == SysError::Unsupported as u64 {
            write_console(b"cp: cannot create destination\n");
        } else {
            write_console(b"cp: failed\n");
        }
        return 1;
    }
    let dst_cap = Cap::from_bits(created.value);
    let mut buf = [0u8; 256];
    loop {
        let got = read(src_cap, &mut buf);
        if !got.ok {
            write_console(b"cp: read failed\n");
            let _ = close(src_cap);
            let _ = close(dst_cap);
            return 1;
        }
        if got.value == 0 {
            break;
        }
        let n = got.value as usize;
        let wrote = write(dst_cap, &buf[..n]);
        if !wrote.ok || wrote.value != got.value {
            write_console(b"cp: write failed\n");
            let _ = close(src_cap);
            let _ = close(dst_cap);
            return 1;
        }
    }
    let _ = close(src_cap);
    let _ = close(dst_cap);
    0
}
