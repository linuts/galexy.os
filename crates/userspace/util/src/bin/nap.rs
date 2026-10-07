//! nap — sleep briefly, print, exit (Milestone 56).

#![no_std]
#![no_main]

use galexy_rt::{entry, sleep_ms, write_console};

entry!(main);

fn main() -> i32 {
    write_console(b"napping\n");
    let _ = sleep_ms(50);
    write_console(b"awake\n");
    0
}
