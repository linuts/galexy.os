//! init — orphan root, service table, and ordered shutdown (Milestone 67).
//!
//! The fixed table is v1. `/etc/init` is not read. Seats restart. `stamp`
//! runs once at boot. `probe` is the restart-storm fixture: it is not
//! started until `svc start`, then a fast exit backs off (250, 500, 1000,
//! then 2000 ms). `spare` is ignore: not started, and a later start does
//! not restart it. Operators talk to this program over the init Cap. They
//! never receive a service Cap.
//!
//! Silent on the console on purpose. Init shares TTY 0 with the F1 seat, so
//! log lines go out through the console Cap and the kernel writes them to
//! the serial log only.

#![no_std]
#![no_main]

use core::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering};

use galexy_abi::{Cap, SysError};
use galexy_rt::{
    chan_recv_poll, chan_send, channel, clock_ms, entry, kill, reboot, shutdown, sleep_ms,
    spawn_with, sync, wait, wait_poll, write_console,
};

entry!(main);

const RESTART: u8 = 0;
const ONCE: u8 = 1;
const IGNORE: u8 = 2;
/// A restart inside this window counts toward the storm.
const FAST_MS: u64 = 2_000;
const NONE: u8 = 0xFF;

struct Svc {
    name: &'static [u8],
    /// 1-based console index for a seat. `0` means no argument.
    arg: u8,
    policy: u8,
    autostart: bool,
}

struct Live {
    cap: AtomicU64,
    hold: AtomicBool,
    running: AtomicBool,
    storm: AtomicU8,
    spawned_ms: AtomicU64,
}

const fn live() -> Live {
    Live {
        cap: AtomicU64::new(0),
        hold: AtomicBool::new(false),
        running: AtomicBool::new(false),
        storm: AtomicU8::new(0),
        spawned_ms: AtomicU64::new(0),
    }
}

/// Twelve seats, one once-service, the backoff fixture, and one ignore
/// service. Fifteen names. A boot holds twelve seat Caps; stamp exits
/// and frees its Cap. That stays under the sixteen-Cap ceiling.
const SERVICES: [Svc; 15] = [
    Svc {
        name: b"shell",
        arg: 1,
        policy: RESTART,
        autostart: true,
    },
    Svc {
        name: b"shell2",
        arg: 2,
        policy: RESTART,
        autostart: true,
    },
    Svc {
        name: b"shell3",
        arg: 3,
        policy: RESTART,
        autostart: true,
    },
    Svc {
        name: b"shell4",
        arg: 4,
        policy: RESTART,
        autostart: true,
    },
    Svc {
        name: b"shell5",
        arg: 5,
        policy: RESTART,
        autostart: true,
    },
    Svc {
        name: b"shell6",
        arg: 6,
        policy: RESTART,
        autostart: true,
    },
    Svc {
        name: b"shell7",
        arg: 7,
        policy: RESTART,
        autostart: true,
    },
    Svc {
        name: b"shell8",
        arg: 8,
        policy: RESTART,
        autostart: true,
    },
    Svc {
        name: b"shell9",
        arg: 9,
        policy: RESTART,
        autostart: true,
    },
    Svc {
        name: b"shell10",
        arg: 10,
        policy: RESTART,
        autostart: true,
    },
    Svc {
        name: b"shell11",
        arg: 11,
        policy: RESTART,
        autostart: true,
    },
    Svc {
        name: b"shell12",
        arg: 12,
        policy: RESTART,
        autostart: true,
    },
    Svc {
        name: b"stamp",
        arg: 0,
        policy: ONCE,
        autostart: true,
    },
    Svc {
        name: b"probe",
        arg: 0,
        policy: RESTART,
        autostart: false,
    },
    Svc {
        name: b"spare",
        arg: 0,
        policy: IGNORE,
        autostart: false,
    },
];

static LIVE: [Live; 15] = [const { live() }; 15];
static SHUTTING_DOWN: AtomicBool = AtomicBool::new(false);
static PENDING_RESTART: AtomicU8 = AtomicU8::new(NONE);
/// Service index to wait next, so a fast exit is not hidden behind a seat.
static WATCH: AtomicU8 = AtomicU8::new(NONE);
static RX: AtomicU64 = AtomicU64::new(0);

fn main() -> i32 {
    let mut ends = [0u64; 2];
    if !channel(&mut ends).ok {
        loop {
            let _ = sleep_ms(1_000);
        }
    }
    RX.store(ends[0], Ordering::Release);
    // End 1 stays open. The kernel enqueues control messages from that
    // end; closing it would make every `svc` / shutdown `Unsupported`.
    let _peer = ends[1];

    for (i, svc) in SERVICES.iter().enumerate() {
        if svc.autostart {
            let _ = spawn_svc(i);
        }
    }
    // Autostart used the single pending-spawn slot. Say so only after
    // those spawns have returned, so a later spawn is not `NoResource`
    // because stamp is still being loaded.
    log(b"[init] ready\n");

    let mut cursor = 0usize;
    loop {
        drain();
        let owed = PENDING_RESTART.swap(NONE, Ordering::AcqRel);
        if owed != NONE {
            let i = owed as usize;
            if i < SERVICES.len()
                && !SHUTTING_DOWN.load(Ordering::Acquire)
                && !LIVE[i].hold.load(Ordering::Acquire)
                && !LIVE[i].running.load(Ordering::Acquire)
            {
                let _ = spawn_svc(i);
            }
        }
        if reap_exited() {
            continue;
        }
        let watch = WATCH.swap(NONE, Ordering::AcqRel);
        let start = if watch == NONE {
            cursor
        } else {
            watch as usize
        };
        if let Some(i) = next_running(start) {
            cursor = (i + 1) % SERVICES.len();
            let cap = LIVE[i].cap.load(Ordering::Acquire);
            let got = wait(Cap::from_bits(cap));
            if !got.ok {
                if SysError::from_code(got.value) == SysError::Interrupted {
                    continue;
                }
                LIVE[i].running.store(false, Ordering::Release);
                LIVE[i].cap.store(0, Ordering::Release);
                continue;
            }
            on_exit(i);
        } else {
            let _ = sleep_ms(1_000);
        }
    }
}

/// Collect children that have already exited. A live child is left alone
/// so the supervisor can park on one of them afterwards.
fn reap_exited() -> bool {
    let mut saw = false;
    for (i, _) in SERVICES.iter().enumerate() {
        if !LIVE[i].running.load(Ordering::Acquire) {
            continue;
        }
        let cap = LIVE[i].cap.load(Ordering::Acquire);
        if cap == 0 {
            continue;
        }
        let got = wait_poll(Cap::from_bits(cap));
        if !got.ok {
            if SysError::from_code(got.value) != SysError::NoResource {
                LIVE[i].running.store(false, Ordering::Release);
                LIVE[i].cap.store(0, Ordering::Release);
            }
            continue;
        }
        on_exit(i);
        saw = true;
    }
    saw
}

fn next_running(start: usize) -> Option<usize> {
    for step in 0..SERVICES.len() {
        let i = (start + step) % SERVICES.len();
        if LIVE[i].running.load(Ordering::Acquire) && LIVE[i].cap.load(Ordering::Acquire) != 0 {
            return Some(i);
        }
    }
    None
}

fn spawn_svc(i: usize) -> bool {
    let svc = &SERVICES[i];
    let arg = [svc.arg];
    let args: &[u8] = if svc.arg == 0 { &[] } else { &arg };
    let got = spawn_with(svc.name, args, 0);
    if !got.ok {
        return false;
    }
    LIVE[i].cap.store(got.value, Ordering::Release);
    LIVE[i].running.store(true, Ordering::Release);
    LIVE[i].spawned_ms.store(clock_ms(), Ordering::Release);
    WATCH.store(i as u8, Ordering::Release);
    true
}

fn on_exit(i: usize) {
    LIVE[i].running.store(false, Ordering::Release);
    LIVE[i].cap.store(0, Ordering::Release);
    if SHUTTING_DOWN.load(Ordering::Acquire) || LIVE[i].hold.load(Ordering::Acquire) {
        return;
    }
    if SERVICES[i].policy != RESTART {
        return;
    }
    let lived = clock_ms().saturating_sub(LIVE[i].spawned_ms.load(Ordering::Acquire));
    let mut storm = LIVE[i].storm.load(Ordering::Acquire);
    if lived >= FAST_MS {
        storm = 0;
    }
    let delay: u64 = match storm {
        0 => 0,
        1 => 250,
        2 => 500,
        3 => 1_000,
        _ => 2_000,
    };
    LIVE[i]
        .storm
        .store(storm.saturating_add(1), Ordering::Release);
    log_backoff(SERVICES[i].name, delay);
    if delay > 0 {
        let slept = sleep_ms(delay);
        if !slept.ok {
            PENDING_RESTART.store(i as u8, Ordering::Release);
            return;
        }
    }
    if SHUTTING_DOWN.load(Ordering::Acquire) || LIVE[i].hold.load(Ordering::Acquire) {
        return;
    }
    let _ = spawn_svc(i);
}

fn drain() {
    let rx = Cap::from_bits(RX.load(Ordering::Acquire));
    loop {
        let mut buf = [0u8; 256];
        let got = chan_recv_poll(rx, &mut buf);
        if !got.ok || got.value == 0 {
            return;
        }
        let n = (got.value as usize).min(buf.len());
        handle(&buf[..n]);
    }
}

fn handle(msg: &[u8]) {
    if msg.len() < galexy_abi::INIT_RPC_HDR + 2 {
        reply(b"bad value\n");
        return;
    }
    let name_len = msg[galexy_abi::INIT_RPC_HDR + 1] as usize;
    let start = galexy_abi::INIT_RPC_HDR + 2;
    if msg.len() < start + name_len {
        reply(b"bad value\n");
        return;
    }
    let op = msg[galexy_abi::INIT_RPC_HDR];
    let name = &msg[start..start + name_len];
    // The kernel stamps `msg[0]` from the sender's session (admin root or
    // not); the sender cannot forge it. Any logged-in seat may ask
    // `status`. Everything that changes service state — start, stop,
    // restart, power — is the operator's: a non-admin seat must not be
    // able to kill another user's seat or churn the service table.
    let admin = msg[0] != 0;
    match op {
        galexy_abi::INIT_OP_STATUS => status(name),
        galexy_abi::INIT_OP_START | galexy_abi::INIT_OP_STOP | galexy_abi::INIT_OP_RESTART
            if !admin =>
        {
            reply(b"access denied\n")
        }
        galexy_abi::INIT_OP_START => start_svc(name),
        galexy_abi::INIT_OP_STOP => stop_svc(name),
        galexy_abi::INIT_OP_RESTART => restart_svc(name),
        galexy_abi::INIT_OP_SHUTDOWN => ordered_power(msg, false),
        galexy_abi::INIT_OP_REBOOT => ordered_power(msg, true),
        _ => reply(b"bad value\n"),
    }
}

fn find(name: &[u8]) -> Option<usize> {
    SERVICES.iter().position(|svc| svc.name == name)
}

fn status(name: &[u8]) {
    let Some(i) = find(name) else {
        reply(b"unknown\n");
        return;
    };
    reply_state(i);
}

fn start_svc(name: &[u8]) {
    let Some(i) = find(name) else {
        reply(b"unknown\n");
        return;
    };
    LIVE[i].hold.store(false, Ordering::Release);
    if LIVE[i].running.load(Ordering::Acquire) || spawn_svc(i) {
        reply_state(i);
    } else {
        reply(b"no resource\n");
    }
}

fn stop_svc(name: &[u8]) {
    let Some(i) = find(name) else {
        reply(b"unknown\n");
        return;
    };
    LIVE[i].hold.store(true, Ordering::Release);
    if LIVE[i].running.load(Ordering::Acquire) {
        let cap = LIVE[i].cap.load(Ordering::Acquire);
        if cap != 0 {
            let _ = kill(Cap::from_bits(cap));
        }
        // Reap before the reply. The supervisor loop drains the next
        // RPC before it reaps, so a following `status` must already
        // see this service down, and the proc Cap must not leak.
        reap_cap(i);
    }
    let mut buf = [0u8; 48];
    let n = push_name(&mut buf, SERVICES[i].name, b" stopped\n");
    reply(&buf[..n]);
}

fn restart_svc(name: &[u8]) {
    let Some(i) = find(name) else {
        reply(b"unknown\n");
        return;
    };
    LIVE[i].hold.store(false, Ordering::Release);
    if LIVE[i].running.load(Ordering::Acquire) {
        let cap = LIVE[i].cap.load(Ordering::Acquire);
        if cap != 0 {
            let _ = kill(Cap::from_bits(cap));
        }
        reap_cap(i);
    }
    // Operator restart starts the service now and clears the storm.
    // Crash policy and backoff apply to unexpected exits, not to this
    // command.
    LIVE[i].storm.store(0, Ordering::Release);
    if spawn_svc(i) {
        reply_state(i);
    } else {
        reply(b"no resource\n");
    }
}

/// Wait out `i`'s exit and drop the proc Cap. Does not apply policy.
fn reap_cap(i: usize) {
    let cap = LIVE[i].cap.load(Ordering::Acquire);
    if cap != 0 {
        loop {
            let got = wait(Cap::from_bits(cap));
            if got.ok || SysError::from_code(got.value) != SysError::Interrupted {
                break;
            }
        }
    }
    LIVE[i].running.store(false, Ordering::Release);
    LIVE[i].cap.store(0, Ordering::Release);
}

fn ordered_power(msg: &[u8], reboot_flag: bool) {
    if msg[0] == 0 {
        reply(b"access denied\n");
        return;
    }
    SHUTTING_DOWN.store(true, Ordering::Release);
    if reboot_flag {
        log(b"[init] reboot\n");
    } else {
        log(b"[init] shutdown\n");
    }
    let spare_tty = msg[1];
    for (i, _) in SERVICES.iter().enumerate() {
        if spared(i, spare_tty) || !LIVE[i].running.load(Ordering::Acquire) {
            continue;
        }
        let cap = LIVE[i].cap.load(Ordering::Acquire);
        if cap != 0 {
            let _ = kill(Cap::from_bits(cap));
        }
    }
    for (i, _) in SERVICES.iter().enumerate() {
        if spared(i, spare_tty) || !LIVE[i].running.load(Ordering::Acquire) {
            continue;
        }
        let cap = LIVE[i].cap.load(Ordering::Acquire);
        if cap != 0 {
            loop {
                let got = wait(Cap::from_bits(cap));
                if got.ok || SysError::from_code(got.value) != SysError::Interrupted {
                    break;
                }
            }
        }
        LIVE[i].running.store(false, Ordering::Release);
        LIVE[i].cap.store(0, Ordering::Release);
    }
    let _ = sync();
    log(b"[init] power\n");
    let stayed = if reboot_flag { reboot() } else { shutdown() };
    let _ = stayed;
    log(b"[init] the machine stayed up\n");
    reply(b"the machine stayed up\n");
    SHUTTING_DOWN.store(false, Ordering::Release);
    for (i, svc) in SERVICES.iter().enumerate() {
        if svc.autostart
            && svc.policy == RESTART
            && !LIVE[i].hold.load(Ordering::Acquire)
            && !LIVE[i].running.load(Ordering::Acquire)
        {
            let _ = spawn_svc(i);
        }
    }
}

fn spared(i: usize, tty: u8) -> bool {
    let arg = SERVICES[i].arg;
    arg != 0 && arg - 1 == tty
}

fn reply_state(i: usize) {
    let mut buf = [0u8; 48];
    let tail: &[u8] = if LIVE[i].running.load(Ordering::Acquire) {
        b" running\n"
    } else {
        b" stopped\n"
    };
    let n = push_name(&mut buf, SERVICES[i].name, tail);
    reply(&buf[..n]);
}

fn reply(bytes: &[u8]) {
    let rx = Cap::from_bits(RX.load(Ordering::Acquire));
    let _ = chan_send(rx, bytes, 0, 0);
}

fn log(bytes: &[u8]) {
    let _ = write_console(bytes);
}

fn log_backoff(name: &[u8], ms: u64) {
    let mut buf = [0u8; 64];
    let mut n = 0usize;
    n = append(&mut buf, n, b"[init] backoff name=");
    n = append(&mut buf, n, name);
    n = append(&mut buf, n, b" ms=");
    n = append_u64(&mut buf, n, ms);
    n = append(&mut buf, n, b"\n");
    log(&buf[..n]);
}

fn push_name(buf: &mut [u8], name: &[u8], tail: &[u8]) -> usize {
    let mut n = append(buf, 0, name);
    n = append(buf, n, tail);
    n
}

fn append(buf: &mut [u8], n: usize, src: &[u8]) -> usize {
    let room = buf.len().saturating_sub(n);
    let take = src.len().min(room);
    buf[n..n + take].copy_from_slice(&src[..take]);
    n + take
}

fn append_u64(buf: &mut [u8], mut n: usize, mut value: u64) -> usize {
    if value == 0 {
        return append(buf, n, b"0");
    }
    let mut tmp = [0u8; 20];
    let mut i = tmp.len();
    while value > 0 {
        i -= 1;
        tmp[i] = b'0' + (value % 10) as u8;
        value /= 10;
    }
    n = append(buf, n, &tmp[i..]);
    n
}
