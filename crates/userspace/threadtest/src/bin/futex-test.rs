//! futex-test — `Futex` wait/wake between two threads of one process.
//!
//! A ping-pong over one `AtomicU32` word for `ROUNDS` turns: the leader
//! sets the word to its turn number and wakes, the worker waits for it,
//! answers with the next number, and wakes back. A lost wakeup hangs the
//! program, which the `test-futex` kernel turns into a timeout failure.
//! Then the edges: a wait on a word that already differs returns at once,
//! a wake with no waiter wakes zero, a wait with a timeout and no waker
//! comes back `TimedOut` no sooner than asked, an unaligned word is
//! `BadBuffer`, and a bad op is `BadValue`. A final `wake_all` releases
//! three parked workers at once and reports the count.
//!
//! Every line is `futex-test: ...` so the kernel test can grep serial.

#![no_std]
#![no_main]

use core::sync::atomic::{AtomicU32, Ordering};

use galexy_abi::{Cap, SysError, Syscall, FUTEX_WAIT};
use galexy_rt::{
    clock_ms, entry, exit, futex_wait, futex_wake, syscall_r8r9, thread_exit, thread_spawn, wait,
    write_console, yield_now,
};

entry!(main);

const ROUNDS: u32 = 200;
static TURN: AtomicU32 = AtomicU32::new(0);
static GATE: AtomicU32 = AtomicU32::new(0);
static PARKED: AtomicU32 = AtomicU32::new(0);

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
    write_console(b"futex-test: FAIL ");
    write_console(what);
    write_console(b"\n");
    exit(1)
}

/// Blocks until `word` leaves `expected`; retries spurious wakes.
fn wait_until_not(word: &AtomicU32, expected: u32) {
    while word.load(Ordering::SeqCst) == expected {
        let r = futex_wait(word, expected, 0);
        if !r.ok {
            fail(b"wait error");
        }
    }
}

extern "C" fn ponger(_arg: u64) -> ! {
    // Odd turns are the worker's; it answers with the next even one.
    let mut mine = 1u32;
    while mine <= ROUNDS * 2 {
        wait_until_not(&TURN, mine - 1);
        if TURN.load(Ordering::SeqCst) != mine {
            fail(b"worker turn");
        }
        TURN.store(mine + 1, Ordering::SeqCst);
        let r = futex_wake(&TURN, 1);
        if !r.ok {
            fail(b"worker wake");
        }
        mine += 2;
    }
    thread_exit(5)
}

extern "C" fn gated(arg: u64) -> ! {
    PARKED.fetch_add(1, Ordering::SeqCst);
    wait_until_not(&GATE, 0);
    thread_exit(20 + arg)
}

fn main() -> i32 {
    write_console(b"futex-test: start\n");

    let r = thread_spawn(ponger, 0, 0);
    if !r.ok {
        fail(b"spawn");
    }
    let ponger_cap = Cap::from_bits(r.value);
    // Even turns are the leader's.
    let mut mine = 0u32;
    while mine < ROUNDS * 2 {
        wait_until_not(&TURN, mine.wrapping_sub(1));
        if mine != 0 && TURN.load(Ordering::SeqCst) != mine {
            fail(b"leader turn");
        }
        TURN.store(mine + 1, Ordering::SeqCst);
        if !futex_wake(&TURN, 1).ok {
            fail(b"leader wake");
        }
        mine += 2;
    }
    wait_until_not(&TURN, mine - 1);
    let r = wait(ponger_cap);
    if !r.ok || r.value != 5 {
        fail(b"ponger join");
    }
    write_console(b"futex-test: rounds=");
    write_u64(u64::from(ROUNDS));
    write_console(b"\n");

    // Already-different word: returns at once, ok.
    let word = AtomicU32::new(7);
    let r = futex_wait(&word, 8, 0);
    if !r.ok {
        fail(b"mismatch wait");
    }
    // No waiter: wakes zero.
    let r = futex_wake(&word, u64::MAX);
    if !r.ok || r.value != 0 {
        fail(b"empty wake");
    }
    // Timeout with nobody to wake us.
    let t0 = clock_ms();
    let r = futex_wait(&word, 7, 30);
    let dt = clock_ms().saturating_sub(t0);
    if r.ok || r.value != SysError::TimedOut as u64 {
        fail(b"timeout code");
    }
    if dt < 25 {
        fail(b"timeout too early");
    }
    // Unaligned word and unknown op.
    let pair = [AtomicU32::new(0), AtomicU32::new(0)];
    let unaligned = pair.as_ptr() as u64 + 2;
    let r = syscall_r8r9(Syscall::Futex as u64, FUTEX_WAIT, unaligned, 0, 0, 0);
    if r.ok || r.value != SysError::BadBuffer as u64 {
        fail(b"unaligned");
    }
    let r = syscall_r8r9(Syscall::Futex as u64, 99, word.as_ptr() as u64, 0, 0, 0);
    if r.ok || r.value != SysError::BadValue as u64 {
        fail(b"bad op");
    }
    write_console(b"futex-test: edges ok\n");

    // wake_all: three parked workers leave together.
    let mut caps = [Cap::from_bits(0); 3];
    for (k, cap) in caps.iter_mut().enumerate() {
        let r = thread_spawn(gated, k as u64, 0);
        if !r.ok {
            fail(b"gated spawn");
        }
        *cap = Cap::from_bits(r.value);
    }
    while PARKED.load(Ordering::SeqCst) < 3 {
        yield_now();
    }
    // Let the last one reach its futex_wait, then open the gate.
    for _ in 0..20 {
        yield_now();
    }
    GATE.store(1, Ordering::SeqCst);
    let r = futex_wake(&GATE, u64::MAX);
    if !r.ok {
        fail(b"wake_all");
    }
    write_console(b"futex-test: woke ");
    write_u64(r.value);
    write_console(b"\n");
    for (k, cap) in caps.iter().enumerate() {
        let r = wait(*cap);
        if !r.ok || r.value != 20 + k as u64 {
            fail(b"gated join");
        }
    }

    write_console(b"futex-test: passed\n");
    0
}
