//! touch — create a galfs file, or leave an existing one in place.

#![no_std]
#![no_main]

use galexy_abi::{Cap, SysError};
use galexy_rt::{arg, close, create, entry, open, write_console};

entry!(main);

fn main() -> i32 {
    let path = arg();
    if path.is_empty() {
        write_console(b"touch: usage: touch <name>\n");
        return 1;
    }
    let existing = open(path);
    if existing.ok {
        let _ = close(Cap::from_bits(existing.value));
        return 0;
    }
    let made = create(path);
    if made.ok {
        if made.value != 0 {
            let _ = close(Cap::from_bits(made.value));
        }
        return 0;
    }
    if made.value == SysError::Unsupported as u64 {
        write_console(b"touch: cannot replace\n");
    } else if made.value == SysError::NotFound as u64 {
        write_console(b"touch: no such directory\n");
    } else {
        write_console(b"touch: failed\n");
    }
    1
}
