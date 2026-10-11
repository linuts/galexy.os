//! thread-test — `ThreadSpawn` / `ThreadExit` from ring 3, no `std`.
//!
//! Four threads share this image's heap, statics, and console Cap. Each
//! pushes into its own `Vec` (the `galexy-rt` heap lock), bumps a shared
//! atomic, prints its number, and `ThreadExit`s with a distinct code that
//! the leader reads back through `Wait` on the join Cap. Then the error
//! paths: an entry outside the image and an oversized stack are both
//! `BadValue`. Last, a thread that never exits is left spinning while the
//! leader exits — the kernel has to take the sibling down with the
//! process (the `test-thread` kernel checks threads and frames afterwards).
//!
//! Every line is `thread-test: ...` so the kernel test can grep serial.

#![no_std]
#![no_main]

extern crate alloc;

use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, Ordering};

use galexy_abi::{Cap, SysError, THREAD_STACK_PAGES_MAX};
use galexy_rt::{entry, exit, thread_exit, thread_spawn, wait, write_console, yield_now};

entry!(main);

const WORKERS: u64 = 4;
const BUMPS: u64 = 1000;
static COUNTER: AtomicU64 = AtomicU64::new(0);
static SPINNER_UP: AtomicU64 = AtomicU64::new(0);

fn write_u64(mut n: u64) {
    if n == 0 {
        write_console(b"0");
        return;
    }
    let mut tmp = [0u8; 20];
    let mut i = tmp.len();
    while n > 0 {
        i -= 1;
        tmp[i] = b'0' + (n % 10) as u8;
        n /= 10;
    }
    write_console(&tmp[i..]);
}

fn fail(what: &[u8]) -> ! {
    write_console(b"thread-test: FAIL ");
    write_console(what);
    write_console(b"\n");
    exit(1)
}

extern "C" fn worker(arg: u64) -> ! {
    let mut local: Vec<u64> = Vec::new();
    for i in 0..BUMPS {
        local.push(i ^ arg);
        COUNTER.fetch_add(1, Ordering::SeqCst);
        if i % 250 == 0 {
            yield_now();
        }
    }
    let sum: u64 = local.iter().sum();
    let expect: u64 = (0..BUMPS).map(|i| i ^ arg).sum();
    if sum != expect {
        fail(b"worker heap");
    }
    write_console(b"thread-test: worker ");
    write_u64(arg);
    write_console(b" done\n");
    thread_exit(10 + arg)
}

extern "C" fn spinner(_arg: u64) -> ! {
    SPINNER_UP.store(1, Ordering::SeqCst);
    loop {
        yield_now();
    }
}

fn main() -> i32 {
    write_console(b"thread-test: start\n");

    let mut joins: [Option<Cap>; WORKERS as usize] = [None; WORKERS as usize];
    for (k, slot) in joins.iter_mut().enumerate() {
        let r = thread_spawn(worker, k as u64, 0);
        if !r.ok {
            fail(b"spawn");
        }
        *slot = Some(Cap::from_bits(r.value));
    }
    for (k, slot) in joins.iter().enumerate() {
        let cap = slot.expect("spawned");
        let r = wait(cap);
        if !r.ok {
            fail(b"join");
        }
        if r.value != 10 + k as u64 {
            fail(b"join code");
        }
    }
    let total = COUNTER.load(Ordering::SeqCst);
    if total != WORKERS * BUMPS {
        fail(b"counter");
    }
    write_console(b"thread-test: joined ");
    write_u64(WORKERS);
    write_console(b" counter=");
    write_u64(total);
    write_console(b"\n");

    // A second round reuses the thread slots the first round freed.
    let r = thread_spawn(worker, 7, 4);
    if !r.ok {
        fail(b"respawn");
    }
    let r = wait(Cap::from_bits(r.value));
    if !r.ok || r.value != 17 {
        fail(b"rejoin");
    }

    // Error paths.
    let bad_entry: extern "C" fn(u64) -> ! = unsafe { core::mem::transmute(0x1000usize) };
    let r = thread_spawn(bad_entry, 0, 0);
    if r.ok || r.value != SysError::BadValue as u64 {
        fail(b"bad entry");
    }
    let r = thread_spawn(worker, 0, THREAD_STACK_PAGES_MAX + 1);
    if r.ok || r.value != SysError::BadValue as u64 {
        fail(b"stack too big");
    }
    write_console(b"thread-test: errors ok\n");

    // Leave a sibling running; the leader's exit must end it.
    let r = thread_spawn(spinner, 0, 0);
    if !r.ok {
        fail(b"spinner");
    }
    while SPINNER_UP.load(Ordering::SeqCst) == 0 {
        yield_now();
    }
    write_console(b"thread-test: passed\n");
    0
}
