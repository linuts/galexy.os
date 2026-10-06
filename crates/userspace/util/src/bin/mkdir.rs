//! mkdir — create a galfs directory. The path ends in `/`.

#![no_std]
#![no_main]

use galexy_abi::SysError;
use galexy_rt::{arg, create, entry, write_console};

entry!(main);

fn main() -> i32 {
    let path = arg();
    if path.is_empty() {
        write_console(b"mkdir: usage: mkdir <name>\n");
        return 1;
    }
    let made = create(path);
    if made.ok {
        return 0;
    }
    if made.value == SysError::Unsupported as u64 {
        write_console(b"mkdir: cannot replace\n");
    } else if made.value == SysError::NotFound as u64 {
        write_console(b"mkdir: no such directory\n");
    } else {
        write_console(b"mkdir: failed\n");
    }
    1
}
