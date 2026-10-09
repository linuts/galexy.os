//! spare — ignore-policy service. Init does not start it, and does not
//! restart it after `svc start`.
//!
//! Trust: ramdisk code, no galfs cards. The body is the whole program.

#![no_std]
#![no_main]

use galexy_rt::entry;

entry!(main);

fn main() -> i32 {
    0
}
