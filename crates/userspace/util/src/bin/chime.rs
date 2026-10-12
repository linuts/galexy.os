//! chime — beat target. Init does not start it. `svc every` arms a
//! monotonic interval. `svc at` arms one civil UTC minute per day.
//!
//! Trust: ramdisk code, no galfs cards. The sleep is longer than the
//! short interval the init test arms, so the next tick overlaps and
//! init must skip instead of stacking a second copy.

#![no_std]
#![no_main]

use galexy_rt::{entry, sleep_ms};

entry!(main);

fn main() -> i32 {
    let _ = sleep_ms(1_000);
    0
}
