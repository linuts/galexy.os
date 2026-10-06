//! ls — list the current directory. The argument is that path, or empty at `/`.

#![no_std]
#![no_main]

use galexy_rt::{arg, entry, files_cap, read, write_console};

entry!(main);

fn main() -> i32 {
    let cwd = arg();
    let mut buf = [0u8; 1024];
    let got = read(files_cap(), &mut buf);
    if !got.ok {
        write_console(b"ls: denied\n");
        return 1;
    }
    let n = (got.value as usize).min(buf.len());
    let mut i = 0usize;
    while i < n {
        let rest = &buf[i..n];
        let end = rest.iter().position(|b| *b == b'\n').unwrap_or(rest.len());
        if let Some(shown) = ls_name(cwd, &rest[..end]) {
            write_console(shown);
            write_console(b"\n");
        }
        i += end + 1;
    }
    0
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
