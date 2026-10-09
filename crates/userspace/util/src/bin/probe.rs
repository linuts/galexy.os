//! probe — crashing service used to exercise init's restart backoff.
//!
//! Trust: init does not start this at boot. `svc start probe` runs it.
//! It exits immediately, so a restart policy backs off. The body is the
//! whole program.

#![no_std]
#![no_main]

use galexy_rt::entry;

entry!(main);

fn main() -> i32 {
    0
}
