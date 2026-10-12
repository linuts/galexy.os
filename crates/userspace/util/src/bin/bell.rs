//! bell — notice target. Init starts it when `ring` rises.
//!
//! Trust: ramdisk code, no galfs cards. It exits immediately. A second
//! `svc raise ring` while the notice is already up must not start it
//! again. The body is the whole program.

#![no_std]
#![no_main]

use galexy_rt::entry;

entry!(main);

fn main() -> i32 {
    0
}
