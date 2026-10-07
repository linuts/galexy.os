//! init — userspace orphan root and seat supervisor (Milestones 53–54).
//!
//! Spawns one login seat per F-key console, Cap-waits, and restarts seats
//! that exit. Round-robin Cap-wait is v1: a crashed seat is restarted when
//! init reaches its Cap in the loop (DESIGN note).

#![no_std]
#![no_main]

use galexy_abi::Cap;
use galexy_rt::{entry, spawn_with, wait, write_console};

entry!(main);

/// Reserved seat names (F1…F12). ELF body is always ramdisk `shell`.
const SEATS: [&[u8]; 12] = [
    b"shell", b"shell2", b"shell3", b"shell4", b"shell5", b"shell6", b"shell7", b"shell8",
    b"shell9", b"shell10", b"shell11", b"shell12",
];

fn main() -> i32 {
    write_console(b"[init] ready\n");
    let mut caps = [0u64; 12];
    for (i, name) in SEATS.iter().enumerate() {
        caps[i] = spawn_seat(name, i as u8);
    }
    write_console(b"[init] seats up\n");

    // Round-robin Cap-wait + restart. Orphan Caps transferred into free
    // table slots are collected when a seat Cap slot is reused after wait.
    loop {
        for (i, name) in SEATS.iter().enumerate() {
            if caps[i] == 0 {
                caps[i] = spawn_seat(name, i as u8);
                continue;
            }
            let r = wait(Cap::from_bits(caps[i]));
            write_console(b"[init] seat restart\n");
            let _ = r;
            caps[i] = spawn_seat(name, i as u8);
        }
    }
}

fn spawn_seat(name: &[u8], tty: u8) -> u64 {
    let arg = [tty.wrapping_add(1)];
    let r = spawn_with(name, &arg, 0);
    if r.ok {
        r.value
    } else {
        write_console(b"[init] seat spawn fail err=");
        let d = b'0' + (r.value.min(9) as u8);
        write_console(&[d, b'\n']);
        0
    }
}
