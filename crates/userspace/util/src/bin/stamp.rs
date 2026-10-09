//! stamp — one-shot service init starts at boot.
//!
//! Trust: init spawns this from the ramdisk with no galfs cards. It exits
//! immediately so the once-policy can reap it. The body is the whole program.

#![no_std]
#![no_main]

use galexy_rt::entry;

entry!(main);

fn main() -> i32 {
    0
}
