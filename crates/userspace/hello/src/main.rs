//! hello — the first real Rust user program on galexy.os.
//!
//! Links galexy-rt (never the kernel); talks to the OS exclusively through
//! the syscall ABI.

#![no_std]
#![no_main]

use galexy_rt::{entry, write_console};

entry!(main);

fn main() -> i32 {
    write_console(b"Hello from a real Rust user program!\n");
    0
}
