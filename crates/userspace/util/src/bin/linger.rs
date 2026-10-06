//! linger — print `up`, then `beat` across yields, and never exit.
//!
//! The shell-restart boot types this, kills the shell, and still sees `beat`.

#![no_std]
#![no_main]

use galexy_rt::{entry, write_console, yield_now};

entry!(main);

fn main() -> i32 {
    write_console(b"up\n");
    let mut n = 0u32;
    loop {
        yield_now();
        n = n.wrapping_add(1);
        if n.is_multiple_of(10) {
            write_console(b"beat\n");
        }
    }
}
