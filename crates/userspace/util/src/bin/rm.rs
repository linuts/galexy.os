//! rm — remove a galfs file or an empty directory.

#![no_std]
#![no_main]

use galexy_abi::SysError;
use galexy_rt::{arg, entry, files_cap, read, remove, write_console};

entry!(main);

fn main() -> i32 {
    let path = arg();
    if path.is_empty() {
        write_console(b"rm: usage: rm <name>\n");
        return 1;
    }
    let removed = remove(path);
    if removed.ok {
        return 0;
    }
    if removed.value == SysError::NotFound as u64 {
        write_console(b"rm: no such file\n");
    } else if removed.value == SysError::Unsupported as u64 && snapshot_has_child(path) {
        write_console(b"rm: directory not empty\n");
    } else if removed.value == SysError::Unsupported as u64 {
        write_console(b"rm: cannot remove\n");
    } else {
        write_console(b"rm: failed\n");
    }
    1
}

/// True when some snapshot line is strictly inside `dir` (`box/leaf`).
fn snapshot_has_child(dir: &[u8]) -> bool {
    if dir.len() + 1 > 65 {
        return false;
    }
    let mut prefix = [0u8; 66];
    prefix[..dir.len()].copy_from_slice(dir);
    prefix[dir.len()] = b'/';
    let prefix = &prefix[..=dir.len()];
    let mut buf = [0u8; 1024];
    let got = read(files_cap(), &mut buf);
    if !got.ok {
        return false;
    }
    let n = (got.value as usize).min(buf.len());
    let mut i = 0usize;
    while i < n {
        let rest = &buf[i..n];
        let end = rest.iter().position(|b| *b == b'\n').unwrap_or(rest.len());
        let line = &rest[..end];
        if line.starts_with(prefix) && line.len() > prefix.len() {
            return true;
        }
        i += end + 1;
    }
    false
}
