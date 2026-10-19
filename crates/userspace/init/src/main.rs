//! init — orphan root, service table, and ordered shutdown (Milestone 67).
//!
//! The fixed table is v1. `/etc/init` is not read. Seats restart. A row
//! whose program is not in the ramdisk is omitted, so a release image
//! that ships only `shell` answers `svc` for the twelve seats. `stamp`
//! runs once at boot when it is packed. `probe` is the restart-storm
//! fixture: it is not started until `svc start`, then a fast exit backs
//! off (250, 500, 1000, then 2000 ms). `spare` is ignore: not started,
//! and a later start does not restart it. `bell` starts when the notice
//! `ring` rises. `chime` is a beat: `svc every` arms a monotonic interval,
//! and `svc at` arms one civil UTC minute per day. `Clock` is not that
//! clock. Operators talk to this program over the init Cap. They never
//! receive a service Cap.
//!
//! Silent on the console on purpose. Init shares TTY 0 with the F1 seat, so
//! log lines go out through the console Cap and the kernel writes them to
//! the serial log only.

#![no_std]
#![no_main]

use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicU8, Ordering};

use galexy_abi::{Cap, SysError};
use galexy_core::{format_utc, parse_utc, utc_to_civil};
use galexy_rt::{
    chan_recv_poll, chan_send, channel, clock_ms, date, date_set, entry, files_cap, kill, read,
    reboot, shutdown, sleep_ms, spawn_with, sync, wait, wait_poll, write_console,
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
    /// Empty: this row is not tied to a notice.
    notice: &'static [u8],
    /// `0`: this row cannot be a beat. Otherwise the default interval.
    every_ms: u64,
}

const fn row(name: &'static [u8], arg: u8, policy: u8, autostart: bool) -> Svc {
    Svc {
        name,
        arg,
        policy,
        autostart,
        notice: b"",
        every_ms: 0,
    }
}

struct Live {
    cap: AtomicU64,
    hold: AtomicBool,
    running: AtomicBool,
    storm: AtomicU8,
    spawned_ms: AtomicU64,
    /// Armed interval. `0` means this row is not a beat right now.
    armed_ms: AtomicU64,
    /// Monotonic deadline. `0` means disarmed.
    next_ms: AtomicU64,
    /// Civil hour, or [`CIVIL_OFF`] when `svc at` is not armed.
    civil_hour: AtomicU8,
    /// Civil minute. Ignored while [`Live::civil_hour`] is [`CIVIL_OFF`].
    civil_minute: AtomicU8,
    /// UTC day (`secs / 86400`) already fired, or `u32::MAX` if not yet.
    civil_day: AtomicU32,
}

/// `civil_hour` value that means this row has no civil beat.
const CIVIL_OFF: u8 = 0xFF;

const fn live() -> Live {
    Live {
        cap: AtomicU64::new(0),
        hold: AtomicBool::new(false),
        running: AtomicBool::new(false),
        storm: AtomicU8::new(0),
        spawned_ms: AtomicU64::new(0),
        armed_ms: AtomicU64::new(0),
        next_ms: AtomicU64::new(0),
        civil_hour: AtomicU8::new(CIVIL_OFF),
        civil_minute: AtomicU8::new(0),
        civil_day: AtomicU32::new(u32::MAX),
    }
}

/// Shortest beat. Matches the first restart-backoff step.
const BEAT_MIN_MS: u64 = 250;
/// Longest monotonic beat: one day.
const BEAT_MAX_MS: u64 = 86_400_000;

/// Twelve seats, one once-service, the backoff fixture, one ignore
/// service, one notice target, and one beat target. Seventeen names.
/// A boot holds twelve seat Caps; stamp exits and frees its Cap. `bell`
/// and `chime` are not started at boot. That stays under the sixteen-Cap
/// ceiling.
const SERVICES: [Svc; 17] = [
    row(b"shell", 1, RESTART, true),
    row(b"shell2", 2, RESTART, true),
    row(b"shell3", 3, RESTART, true),
    row(b"shell4", 4, RESTART, true),
    row(b"shell5", 5, RESTART, true),
    row(b"shell6", 6, RESTART, true),
    row(b"shell7", 7, RESTART, true),
    row(b"shell8", 8, RESTART, true),
    row(b"shell9", 9, RESTART, true),
    row(b"shell10", 10, RESTART, true),
    row(b"shell11", 11, RESTART, true),
    row(b"shell12", 12, RESTART, true),
    row(b"stamp", 0, ONCE, true),
    row(b"probe", 0, RESTART, false),
    row(b"spare", 0, IGNORE, false),
    Svc {
        name: b"bell",
        arg: 0,
        policy: ONCE,
        autostart: false,
        notice: b"ring",
        every_ms: 0,
    },
    Svc {
        name: b"chime",
        arg: 0,
        policy: ONCE,
        autostart: false,
        notice: b"",
        every_ms: 1_000,
    },
];

/// Notice names a row may name. A raise of anything else is `unknown`.
const NOTICE_NAMES: [&[u8]; 1] = [b"ring"];

const fn same(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut i = 0;
    while i < a.len() {
        if a[i] != b[i] {
            return false;
        }
        i += 1;
    }
    true
}

const fn notices_declared() -> bool {
    let mut i = 0;
    while i < SERVICES.len() {
        let n = SERVICES[i].notice;
        if !n.is_empty() {
            let mut found = false;
            let mut j = 0;
            while j < NOTICE_NAMES.len() {
                if same(n, NOTICE_NAMES[j]) {
                    found = true;
                }
                j += 1;
            }
            if !found {
                return false;
            }
        }
        i += 1;
    }
    true
}

const _: () = assert!(notices_declared());

static LIVE: [Live; 17] = [const { live() }; 17];
static NOTICE_UP: [AtomicBool; NOTICE_NAMES.len()] =
    [const { AtomicBool::new(false) }; NOTICE_NAMES.len()];
static SHUTTING_DOWN: AtomicBool = AtomicBool::new(false);
static PENDING_RESTART: AtomicU8 = AtomicU8::new(NONE);
/// Service index to wait next, so a fast exit is not hidden behind a seat.
static WATCH: AtomicU8 = AtomicU8::new(NONE);
static RX: AtomicU64 = AtomicU64::new(0);
/// One `[init] date unset` line per boot, not one per slice.
static DATE_UNSET_LOGGED: AtomicBool = AtomicBool::new(false);
/// A row whose ELF is not in this ramdisk. The release image omits the
/// fixture programs; the test image packs them.
static OMIT: [AtomicBool; 17] = [const { AtomicBool::new(false) }; 17];

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

    note_omissions();
    for (i, svc) in SERVICES.iter().enumerate() {
        if svc.autostart && !OMIT[i].load(Ordering::Acquire) {
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
        if fire_due_beats() {
            continue;
        }
        if fire_due_civil() {
            continue;
        }
        // A beat cannot share a blocking Cap-wait: that park returns
        // only when the child exits or a control message arrives. Sleep
        // in short slices so the interval can fire. No armed beat means
        // the park below is unchanged. A civil beat uses the same slices.
        if let Some(delay) = supervisor_delay() {
            let slice = delay.clamp(1, 1_000);
            let _ = sleep_ms(slice);
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
    // `status` or read `date`. Everything that changes service state or
    // the civil date — start, stop, restart, raise, drop, every, at,
    // date set, power — is the operator's: a non-admin seat must not be
    // able to kill another user's seat or churn the service table.
    let admin = msg[0] != 0;
    match op {
        galexy_abi::INIT_OP_STATUS => status(name),
        galexy_abi::INIT_OP_DATE => date_show(),
        galexy_abi::INIT_OP_START
        | galexy_abi::INIT_OP_STOP
        | galexy_abi::INIT_OP_RESTART
        | galexy_abi::INIT_OP_RAISE
        | galexy_abi::INIT_OP_DROP
        | galexy_abi::INIT_OP_EVERY
        | galexy_abi::INIT_OP_DATE_SET
        | galexy_abi::INIT_OP_AT
            if !admin =>
        {
            reply(b"access denied\n")
        }
        galexy_abi::INIT_OP_START => start_svc(name),
        galexy_abi::INIT_OP_STOP => stop_svc(name),
        galexy_abi::INIT_OP_RESTART => restart_svc(name),
        galexy_abi::INIT_OP_RAISE => raise_notice(name),
        galexy_abi::INIT_OP_DROP => drop_notice(name),
        galexy_abi::INIT_OP_EVERY => every_svc(name, payload_u64(msg, start + name_len)),
        galexy_abi::INIT_OP_DATE_SET => date_set_cmd(name),
        galexy_abi::INIT_OP_AT => {
            let at = start + name_len;
            let Some(hour) = msg.get(at).copied() else {
                reply(b"bad value\n");
                return;
            };
            let Some(minute) = msg.get(at + 1).copied() else {
                reply(b"bad value\n");
                return;
            };
            at_svc(name, hour, minute);
        }
        galexy_abi::INIT_OP_SHUTDOWN => ordered_power(msg, false),
        galexy_abi::INIT_OP_REBOOT => ordered_power(msg, true),
        _ => reply(b"bad value\n"),
    }
}

fn find(name: &[u8]) -> Option<usize> {
    let i = SERVICES.iter().position(|svc| svc.name == name)?;
    if OMIT[i].load(Ordering::Acquire) {
        None
    } else {
        Some(i)
    }
}

/// `shell2` loads the ramdisk file `shell`. Every other row loads a
/// file of the same name.
fn program_file(name: &[u8]) -> &[u8] {
    let Some(rest) = name.strip_prefix(b"shell") else {
        return name;
    };
    if rest.iter().all(|b| b.is_ascii_digit()) {
        b"shell"
    } else {
        name
    }
}

fn line_eq(snap: &[u8], name: &[u8]) -> bool {
    let mut i = 0usize;
    while i < snap.len() {
        let rest = &snap[i..];
        let end = rest.iter().position(|b| *b == b'\n').unwrap_or(rest.len());
        if &rest[..end] == name {
            return true;
        }
        if end == rest.len() {
            break;
        }
        i += end + 1;
    }
    false
}

/// Marks rows whose program is absent. A failed or empty snapshot keeps
/// every row, so a query glitch does not drop the seats.
fn note_omissions() {
    static mut SNAP: [u8; galexy_abi::FILES_SNAPSHOT_MAX] = [0; galexy_abi::FILES_SNAPSHOT_MAX];
    // SAFETY: init is one thread and does not re-enter this buffer.
    let buf = unsafe { &mut *core::ptr::addr_of_mut!(SNAP) };
    let got = read(files_cap(), buf);
    if !got.ok || got.value == 0 {
        return;
    }
    let n = (got.value as usize).min(buf.len());
    let snap = &buf[..n];
    for (i, svc) in SERVICES.iter().enumerate() {
        if !line_eq(snap, program_file(svc.name)) {
            OMIT[i].store(true, Ordering::Release);
        }
    }
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
    if SERVICES[i].every_ms != 0 && LIVE[i].armed_ms.load(Ordering::Acquire) == 0 {
        arm_beat(i, SERVICES[i].every_ms);
    }
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
    disarm_beat(i);
    disarm_civil(i);
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
    if SERVICES[i].every_ms != 0 && LIVE[i].armed_ms.load(Ordering::Acquire) == 0 {
        arm_beat(i, SERVICES[i].every_ms);
    }
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

fn notice_slot(name: &[u8]) -> Option<usize> {
    NOTICE_NAMES.iter().position(|n| *n == name)
}

fn payload_u64(msg: &[u8], at: usize) -> u64 {
    let Some(bytes) = msg.get(at..at + 8) else {
        return 0;
    };
    let mut raw = [0u8; 8];
    raw.copy_from_slice(bytes);
    u64::from_le_bytes(raw)
}

fn raise_notice(name: &[u8]) {
    let Some(slot) = notice_slot(name) else {
        reply(b"unknown\n");
        return;
    };
    if NOTICE_UP[slot].swap(true, Ordering::AcqRel) {
        reply_notice(name, b" up\n");
        return;
    }
    log_notice(name, b" up");
    for (i, svc) in SERVICES.iter().enumerate() {
        if svc.notice != name {
            continue;
        }
        LIVE[i].hold.store(false, Ordering::Release);
        if LIVE[i].running.load(Ordering::Acquire) {
            continue;
        }
        if spawn_svc(i) {
            log_kv(b"[init] start name=", svc.name);
        }
    }
    reply_notice(name, b" up\n");
}

fn drop_notice(name: &[u8]) {
    let Some(slot) = notice_slot(name) else {
        reply(b"unknown\n");
        return;
    };
    if !NOTICE_UP[slot].swap(false, Ordering::AcqRel) {
        reply_notice(name, b" down\n");
        return;
    }
    log_notice(name, b" down");
    for (i, svc) in SERVICES.iter().enumerate() {
        if svc.notice != name {
            continue;
        }
        LIVE[i].hold.store(true, Ordering::Release);
        disarm_beat(i);
        disarm_civil(i);
        if LIVE[i].running.load(Ordering::Acquire) {
            let cap = LIVE[i].cap.load(Ordering::Acquire);
            if cap != 0 {
                let _ = kill(Cap::from_bits(cap));
            }
            reap_cap(i);
        }
    }
    reply_notice(name, b" down\n");
}

fn every_svc(name: &[u8], ms: u64) {
    let Some(i) = find(name) else {
        reply(b"unknown\n");
        return;
    };
    if SERVICES[i].every_ms == 0 {
        reply(b"bad value\n");
        return;
    }
    let interval = if ms == 0 { SERVICES[i].every_ms } else { ms };
    if !(BEAT_MIN_MS..=BEAT_MAX_MS).contains(&interval) {
        reply(b"bad value\n");
        return;
    }
    LIVE[i].hold.store(false, Ordering::Release);
    arm_beat(i, interval);
    let mut buf = [0u8; 48];
    let n = push_name(&mut buf, SERVICES[i].name, b" armed\n");
    reply(&buf[..n]);
}

fn arm_beat(i: usize, ms: u64) {
    LIVE[i].armed_ms.store(ms, Ordering::Release);
    LIVE[i]
        .next_ms
        .store(clock_ms().saturating_add(ms), Ordering::Release);
}

fn disarm_beat(i: usize) {
    LIVE[i].armed_ms.store(0, Ordering::Release);
    LIVE[i].next_ms.store(0, Ordering::Release);
}

/// Start due beats. A row that is still running is skipped, not stacked.
fn fire_due_beats() -> bool {
    let now = clock_ms();
    let mut spawned = false;
    for (i, svc) in SERVICES.iter().enumerate() {
        let next = LIVE[i].next_ms.load(Ordering::Acquire);
        if next == 0 || now < next {
            continue;
        }
        let every = LIVE[i].armed_ms.load(Ordering::Acquire).max(BEAT_MIN_MS);
        LIVE[i]
            .next_ms
            .store(now.saturating_add(every), Ordering::Release);
        if LIVE[i].hold.load(Ordering::Acquire) {
            continue;
        }
        if LIVE[i].running.load(Ordering::Acquire) {
            log_kv(b"[init] beat skip name=", svc.name);
            continue;
        }
        log_kv(b"[init] beat name=", svc.name);
        if spawn_svc(i) {
            spawned = true;
        }
    }
    spawned
}

fn at_svc(name: &[u8], hour: u8, minute: u8) {
    let Some(i) = find(name) else {
        reply(b"unknown\n");
        return;
    };
    if SERVICES[i].every_ms == 0 || hour > 23 || minute > 59 {
        reply(b"bad value\n");
        return;
    }
    LIVE[i].hold.store(false, Ordering::Release);
    LIVE[i].civil_hour.store(hour, Ordering::Release);
    LIVE[i].civil_minute.store(minute, Ordering::Release);
    LIVE[i].civil_day.store(u32::MAX, Ordering::Release);
    let mut buf = [0u8; 48];
    let n = push_name(&mut buf, SERVICES[i].name, b" at\n");
    reply(&buf[..n]);
}

fn disarm_civil(i: usize) {
    LIVE[i].civil_hour.store(CIVIL_OFF, Ordering::Release);
    LIVE[i].civil_day.store(u32::MAX, Ordering::Release);
}

fn date_show() {
    let got = date();
    if !got.ok {
        reply(b"unset\n");
        return;
    }
    let mut text = [0u8; 19];
    if format_utc(got.value, &mut text) != Some(19) {
        reply(b"unset\n");
        return;
    }
    let mut buf = [0u8; 24];
    buf[..19].copy_from_slice(&text);
    buf[19..24].copy_from_slice(b" utc\n");
    reply(&buf);
}

fn date_set_cmd(text: &[u8]) {
    let Some(secs) = parse_utc(text) else {
        reply(b"bad value\n");
        return;
    };
    if !date_set(secs).ok {
        reply(b"bad value\n");
        return;
    }
    date_show();
}

/// Fire civil beats whose UTC minute matches. A still-running program
/// is skipped. The day is stored either way so the minute fires once.
fn fire_due_civil() -> bool {
    if !civil_armed() {
        return false;
    }
    let got = date();
    if !got.ok {
        if DATE_UNSET_LOGGED
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            log(b"[init] date unset\n");
        }
        return false;
    }
    let Some(civil) = utc_to_civil(got.value) else {
        return false;
    };
    let Ok(day) = u32::try_from(got.value / 86_400) else {
        return false;
    };
    let mut spawned = false;
    for (i, svc) in SERVICES.iter().enumerate() {
        let hour = LIVE[i].civil_hour.load(Ordering::Acquire);
        if hour == CIVIL_OFF {
            continue;
        }
        let minute = LIVE[i].civil_minute.load(Ordering::Acquire);
        if civil.hour != hour || civil.minute != minute {
            continue;
        }
        if LIVE[i].civil_day.load(Ordering::Acquire) == day {
            continue;
        }
        if LIVE[i].hold.load(Ordering::Acquire) {
            LIVE[i].civil_day.store(day, Ordering::Release);
            continue;
        }
        if LIVE[i].running.load(Ordering::Acquire) {
            LIVE[i].civil_day.store(day, Ordering::Release);
            log_kv(b"[init] at skip name=", svc.name);
            continue;
        }
        if spawn_svc(i) {
            LIVE[i].civil_day.store(day, Ordering::Release);
            log_kv(b"[init] at name=", svc.name);
            spawned = true;
        }
    }
    spawned
}

fn civil_armed() -> bool {
    LIVE.iter()
        .any(|live| live.civil_hour.load(Ordering::Acquire) != CIVIL_OFF)
}

/// Delay until the next monotonic beat. A civil beat caps that at one
/// second, and sleeps even when no monotonic beat is armed.
fn supervisor_delay() -> Option<u64> {
    let beat = next_beat_delay();
    if civil_armed() {
        Some(beat.unwrap_or(1_000).min(1_000))
    } else {
        beat
    }
}

fn next_beat_delay() -> Option<u64> {
    let now = clock_ms();
    let mut best: Option<u64> = None;
    for live in &LIVE {
        let next = live.next_ms.load(Ordering::Acquire);
        if next == 0 {
            continue;
        }
        let delay = next.saturating_sub(now);
        best = Some(match best {
            Some(so_far) => delay.min(so_far),
            None => delay,
        });
    }
    best
}

fn reply_notice(name: &[u8], tail: &[u8]) {
    let mut buf = [0u8; 48];
    let n = push_name(&mut buf, name, tail);
    reply(&buf[..n]);
}

fn log_kv(prefix: &[u8], name: &[u8]) {
    let mut buf = [0u8; 64];
    let mut n = append(&mut buf, 0, prefix);
    n = append(&mut buf, n, name);
    n = append(&mut buf, n, b"\n");
    log(&buf[..n]);
}

fn log_notice(name: &[u8], tail: &[u8]) {
    let mut buf = [0u8; 64];
    let mut n = append(&mut buf, 0, b"[init] notice name=");
    n = append(&mut buf, n, name);
    n = append(&mut buf, n, tail);
    n = append(&mut buf, n, b"\n");
    log(&buf[..n]);
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
