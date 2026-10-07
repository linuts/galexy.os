//! init — userspace orphan root (Milestone 53).
//!
//! Cap-waits / reaps children and orphans transferred from exiting parents.
//! Seat spawn / restart is Milestone 54.

#![no_std]
#![no_main]

use galexy_abi::{Cap, CapRights, MAX_PROC_CAPS, PROC_CAP_BASE};
use galexy_rt::{entry, sleep_ms, wait, write_console};

entry!(main);

fn main() -> i32 {
    write_console(b"[init] ready\n");
    loop {
        let mut reaped = false;
        for i in 0..MAX_PROC_CAPS {
            let cap = Cap::new(PROC_CAP_BASE + i, CapRights::PROC_WAIT);
            let r = wait(cap);
            if r.ok {
                write_console(b"[init] reaped child\n");
                reaped = true;
            }
        }
        if !reaped {
            sleep_ms(50);
        }
    }
}
