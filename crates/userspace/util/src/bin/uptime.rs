//! uptime — monotonic milliseconds since boot.
//!
//! Trust: the shell spawns this with `SPAWN_INHERIT` (the caller's galfs
//! cards). The ramdisk is trusted code. The number is [`galexy_rt::clock_ms`],
//! the same counter as `sleep`.

#![no_std]
#![no_main]

use galexy_rt::{clock_ms, entry, write_std};

entry!(main);

fn main() -> i32 {
    let _ = write_std(b"uptime: ");
    write_u64(clock_ms());
    let _ = write_std(b" ms\n");
    0
}

fn write_u64(mut n: u64) {
    if n == 0 {
        let _ = write_std(b"0");
        return;
    }
    let mut tmp = [0u8; 20];
    let mut i = tmp.len();
    while n > 0 {
        i -= 1;
        tmp[i] = b'0' + (n % 10) as u8;
        n /= 10;
    }
    let _ = write_std(&tmp[i..]);
}
