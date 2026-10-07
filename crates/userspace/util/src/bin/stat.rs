//! stat — print galfs path metadata.

#![no_std]
#![no_main]

use galexy_abi::{STAT_DIR, STAT_FILE, STAT_LEN, TOKEN_CREATE, TOKEN_LIST, TOKEN_READ, TOKEN_REMOVE, TOKEN_WRITE};
use galexy_rt::{arg, entry, stat, write_console};

entry!(main);

fn main() -> i32 {
    let path = arg();
    if path.is_empty() {
        write_console(b"stat: usage: stat <path>\n");
        return 1;
    }
    let mut buf = [0u8; STAT_LEN];
    let got = stat(path, &mut buf);
    if !got.ok || got.value != STAT_LEN as u64 {
        write_console(b"stat: failed\n");
        return 1;
    }

    write_console(b"path: ");
    write_console(path);
    write_console(b"\n");

    write_console(b"kind: ");
    match buf[0] {
        STAT_FILE => write_console(b"file\n"),
        STAT_DIR => write_console(b"dir\n"),
        _ => write_console(b"?\n"),
    };

    let size = u32::from_le_bytes([buf[4], buf[5], buf[6], buf[7]]);
    write_console(b"size: ");
    write_u32(size);
    write_console(b"\n");

    let owner_len = buf[8] as usize;
    if owner_len > 0 && owner_len <= 32 {
        write_console(b"owner: ");
        write_console(&buf[9..9 + owner_len]);
        write_console(b"\n");
    }

    write_console(b"rights: ");
    let r = buf[1];
    if r & TOKEN_READ as u8 != 0 {
        write_console(b"r");
    }
    if r & TOKEN_WRITE as u8 != 0 {
        write_console(b"w");
    }
    if r & TOKEN_LIST as u8 != 0 {
        write_console(b"l");
    }
    if r & TOKEN_CREATE as u8 != 0 {
        write_console(b"c");
    }
    if r & TOKEN_REMOVE as u8 != 0 {
        write_console(b"x");
    }
    if r == 0 {
        write_console(b"-");
    }
    write_console(b"\n");
    0
}

fn write_u32(mut n: u32) {
    if n == 0 {
        write_console(b"0");
        return;
    }
    let mut buf = [0u8; 10];
    let mut i = 10;
    while n > 0 {
        i -= 1;
        buf[i] = b'0' + (n % 10) as u8;
        n /= 10;
    }
    write_console(&buf[i..]);
}
