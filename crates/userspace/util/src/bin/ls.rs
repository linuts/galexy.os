//! ls — list one directory. The argument is that path, or empty at `/`.
//!
//! Trust: the shell spawns this with `SPAWN_INHERIT` (the caller's galfs
//! cards) and the files snapshot. The ramdisk is trusted code.
//!
//! `ls -l` (optional NUL and the same directory path) prints `f <size> name`
//! or `d <size> name` from `stat`. A name `stat` cannot read is `? 0 name`.
//! Plain `ls` is unchanged.

#![no_std]
#![no_main]

use galexy_abi::{STAT_DIR, STAT_FILE, STAT_LEN};
use galexy_rt::{arg, entry, files_cap, read, stat, write_console, write_std};

entry!(main);

fn main() -> i32 {
    let raw = arg();
    if raw.starts_with(b"-l") && (raw.len() == 2 || raw.get(2) == Some(&0)) {
        let cwd = if raw.len() > 3 { &raw[3..] } else { &[] };
        return ls_long(trim_nul(cwd));
    }
    ls_plain(raw)
}

/// The desk ramdisk lists the sysroot before any file the user created.
/// A 1 KiB read stops inside `srlib/`, so `test.rs` and the binary
/// `rustc` just wrote never appear. The buffer stays off the 16 KiB
/// leader stack.
fn read_snapshot() -> Option<&'static [u8]> {
    static mut BUF: [u8; galexy_abi::FILES_SNAPSHOT_MAX] = [0; galexy_abi::FILES_SNAPSHOT_MAX];
    let (ptr, n) = {
        // SAFETY: `ls` is one thread and does not re-enter this buffer.
        let buf = unsafe { &mut *core::ptr::addr_of_mut!(BUF) };
        let got = read(files_cap(), buf);
        if !got.ok {
            return None;
        }
        (buf.as_ptr(), (got.value as usize).min(buf.len()))
    };
    // SAFETY: `ptr` addresses the static. The slice stays valid until
    // the next call, and `ls` does not call this again while walking it.
    Some(unsafe { core::slice::from_raw_parts(ptr, n) })
}

fn trim_nul(bytes: &[u8]) -> &[u8] {
    match bytes.iter().position(|b| *b == 0) {
        Some(i) => &bytes[..i],
        None => bytes,
    }
}

fn ls_plain(cwd: &[u8]) -> i32 {
    let Some(buf) = read_snapshot() else {
        let _ = write_console(b"ls: denied\n");
        return 1;
    };
    let (off, len, n) = collect(buf, cwd);
    for i in 0..n {
        let shown = &buf[off[i] as usize..off[i] as usize + len[i] as usize];
        let _ = write_std(shown);
        let _ = write_std(b"\n");
    }
    0
}

fn ls_long(cwd: &[u8]) -> i32 {
    let Some(buf) = read_snapshot() else {
        let _ = write_console(b"ls: denied\n");
        return 1;
    };
    let (off, len, n) = collect(buf, cwd);
    for i in 0..n {
        let shown = &buf[off[i] as usize..off[i] as usize + len[i] as usize];
        print_long(cwd, shown);
    }
    0
}

/// Visible names in this directory. Directories come first, then files,
/// each group in byte order. A ramdisk name that the volume also has is
/// listed once.
fn collect(buf: &[u8], cwd: &[u8]) -> ([u16; 160], [u16; 160], usize) {
    let mut off = [0u16; 160];
    let mut len = [0u16; 160];
    let mut n = 0usize;
    let mut i = 0usize;
    let total = buf.len();
    let base = buf.as_ptr() as usize;
    while i < total {
        let rest = &buf[i..total];
        let end = rest.iter().position(|b| *b == b'\n').unwrap_or(rest.len());
        if let Some(shown) = ls_name(cwd, &rest[..end]) {
            if shown.len() <= u16::MAX as usize && n < off.len() {
                let at = shown.as_ptr() as usize - base;
                let mut dup = false;
                let mut k = 0usize;
                while k < n {
                    let prev = &buf[off[k] as usize..off[k] as usize + len[k] as usize];
                    if prev == shown {
                        dup = true;
                        break;
                    }
                    k += 1;
                }
                if !dup {
                    off[n] = at as u16;
                    len[n] = shown.len() as u16;
                    n += 1;
                }
            }
        }
        i += end + 1;
    }
    let mut a = 1usize;
    while a < n {
        let mut b = a;
        while b > 0 && name_before(buf, off[b], len[b], off[b - 1], len[b - 1]) {
            off.swap(b, b - 1);
            len.swap(b, b - 1);
            b -= 1;
        }
        a += 1;
    }
    (off, len, n)
}

fn name_before(buf: &[u8], a_off: u16, a_len: u16, b_off: u16, b_len: u16) -> bool {
    let a = &buf[a_off as usize..a_off as usize + a_len as usize];
    let b = &buf[b_off as usize..b_off as usize + b_len as usize];
    let ad = a.ends_with(b"/");
    let bd = b.ends_with(b"/");
    match (ad, bd) {
        (true, false) => true,
        (false, true) => false,
        _ => a < b,
    }
}

fn print_long(cwd: &[u8], shown: &[u8]) {
    let mut path = [0u8; 96];
    let plen = compose(cwd, shown, &mut path);
    let mut rec = [0u8; STAT_LEN];
    let got = if plen == 0 {
        None
    } else {
        Some(stat(&path[..plen], &mut rec))
    };
    let (kind, size) = if got.is_some_and(|got| got.ok && got.value == STAT_LEN as u64) {
        let size = u32::from_le_bytes([rec[4], rec[5], rec[6], rec[7]]);
        let kind = match rec[0] {
            STAT_FILE => b'f',
            STAT_DIR => b'd',
            _ => b'?',
        };
        (kind, size)
    } else {
        (b'?', 0)
    };
    let _ = write_std(&[kind]);
    let _ = write_std(b" ");
    write_u32(size);
    let _ = write_std(b" ");
    let _ = write_std(shown);
    let _ = write_std(b"\n");
}

fn compose(cwd: &[u8], shown: &[u8], out: &mut [u8]) -> usize {
    if cwd.is_empty() {
        if shown.len() > out.len() {
            return 0;
        }
        out[..shown.len()].copy_from_slice(shown);
        return shown.len();
    }
    let need = cwd.len() + 1 + shown.len();
    if need > out.len() {
        return 0;
    }
    out[..cwd.len()].copy_from_slice(cwd);
    out[cwd.len()] = b'/';
    out[cwd.len() + 1..need].copy_from_slice(shown);
    need
}

fn write_u32(mut n: u32) {
    if n == 0 {
        let _ = write_std(b"0");
        return;
    }
    let mut tmp = [0u8; 10];
    let mut i = tmp.len();
    while n > 0 {
        i -= 1;
        tmp[i] = b'0' + (n % 10) as u8;
        n /= 10;
    }
    let _ = write_std(&tmp[i..]);
}

fn ls_name<'a>(cwd: &[u8], line: &'a [u8]) -> Option<&'a [u8]> {
    if line.is_empty() {
        return None;
    }
    if cwd.is_empty() {
        let slashes = line.iter().filter(|b| **b == b'/').count();
        if slashes == 0 || (slashes == 1 && line.ends_with(b"/")) {
            return Some(line);
        }
        return None;
    }
    if line.len() <= cwd.len() + 1 {
        return None;
    }
    if &line[..cwd.len()] != cwd || line[cwd.len()] != b'/' {
        return None;
    }
    let rest = &line[cwd.len() + 1..];
    let slashes = rest.iter().filter(|b| **b == b'/').count();
    if slashes == 0 || (slashes == 1 && rest.ends_with(b"/")) {
        Some(rest)
    } else {
        None
    }
}
