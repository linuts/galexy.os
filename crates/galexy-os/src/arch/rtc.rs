//! CMOS real-time clock, used only to anchor the civil date.
//!
//! The scheduler never reads this. [`crate::arch::timer_ticks`] stays the
//! monotonic clock. A date is an anchor `(utc seconds, ticks)` plus the
//! ticks that have elapsed since. Setting the date does not move sleep,
//! lockout, or a monotonic beat.
//!
//! The chip is the MC146818 at ports `0x70` / `0x71`. Index bit 7 stays
//! set so the write does not enable NMI. A halted clock, a year outside
//! 2000..=2099, or a register that changes mid-read leaves the date unset.

use core::sync::atomic::{AtomicU64, Ordering};

use x86_64::instructions::port::Port;

use crate::serial_println;

const INDEX: u16 = 0x70;
const DATA: u16 = 0x71;
const SECONDS: u8 = 0x00;
const MINUTES: u8 = 0x02;
const HOURS: u8 = 0x04;
const DAY: u8 = 0x07;
const MONTH: u8 = 0x08;
const YEAR: u8 = 0x09;
const STATUS_A: u8 = 0x0A;
const STATUS_B: u8 = 0x0B;
const CENTURY: u8 = 0x32;
const UIP: u8 = 0x80;
const HOUR_24: u8 = 0x02;
const BINARY: u8 = 0x04;
const SET: u8 = 0x80;

/// Even and non-zero once an anchor exists. Odd while a writer is publishing.
static SEQ: AtomicU64 = AtomicU64::new(0);
static UTC_BASE: AtomicU64 = AtomicU64::new(0);
static TICKS_BASE: AtomicU64 = AtomicU64::new(0);

/// Reads the CMOS clock once. A plausible date becomes the anchor.
pub fn init() {
    match read_chip() {
        Some(secs) => {
            publish(secs);
            log_date("", secs);
        }
        None => serial_println!("[date] unset"),
    }
}

/// UTC seconds now, or `None` when no anchor has been published.
pub fn now_secs() -> Option<u64> {
    for _ in 0..16 {
        let seq = SEQ.load(Ordering::Acquire);
        if seq == 0 {
            return None;
        }
        if (seq & 1) == 1 {
            core::hint::spin_loop();
            continue;
        }
        let utc = UTC_BASE.load(Ordering::Acquire);
        let ticks = TICKS_BASE.load(Ordering::Acquire);
        if SEQ.load(Ordering::Acquire) != seq {
            continue;
        }
        let elapsed = crate::arch::timer_ticks().saturating_sub(ticks) / 1000;
        return Some(utc.saturating_add(elapsed));
    }
    None
}

/// Anchors `secs` and copies the civil fields into the CMOS clock.
///
/// `false` means the seconds are not a civil time this kernel accepts.
/// The monotonic counter is not written.
pub fn set_secs(secs: u64) -> bool {
    if galexy_core::utc_to_civil(secs).is_none() {
        return false;
    }
    publish(secs);
    let _ = write_chip(secs);
    log_date("set ", secs);
    true
}

fn publish(secs: u64) {
    let ticks = crate::arch::timer_ticks();
    let seq = SEQ.load(Ordering::Relaxed) & !1;
    SEQ.store(seq.wrapping_add(1), Ordering::Release);
    UTC_BASE.store(secs, Ordering::Relaxed);
    TICKS_BASE.store(ticks, Ordering::Relaxed);
    SEQ.store(seq.wrapping_add(2), Ordering::Release);
}

fn log_date(prefix: &str, secs: u64) {
    let mut buf = [0u8; 19];
    let Some(n) = galexy_core::format_utc(secs, &mut buf) else {
        serial_println!("[date] unset");
        return;
    };
    let text = core::str::from_utf8(&buf[..n]).unwrap_or("");
    serial_println!("[date] {}{} utc", prefix, text);
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct Fields {
    second: u8,
    minute: u8,
    hour: u8,
    day: u8,
    month: u8,
    year: i32,
}

fn read_chip() -> Option<u64> {
    if !wait_settled() {
        return None;
    }
    let status = cmos_read(STATUS_B);
    if (status & SET) != 0 {
        return None;
    }
    let first = read_fields(status)?;
    if !wait_settled() {
        return None;
    }
    let second = read_fields(cmos_read(STATUS_B))?;
    if first != second {
        return None;
    }
    // A zeroed century is not a date. Admin set may still store 1970 later.
    if !(2000..=galexy_core::YEAR_MAX).contains(&first.year) {
        return None;
    }
    galexy_core::civil_to_utc(galexy_core::Civil {
        year: first.year,
        month: first.month,
        day: first.day,
        hour: first.hour,
        minute: first.minute,
        second: first.second,
    })
}

fn read_fields(status: u8) -> Option<Fields> {
    let binary = (status & BINARY) != 0;
    let hour_24 = (status & HOUR_24) != 0;
    let second = decode(cmos_read(SECONDS), binary)?;
    let minute = decode(cmos_read(MINUTES), binary)?;
    let hour_raw = cmos_read(HOURS);
    let hour = decode_hour(hour_raw, binary, hour_24)?;
    let day = decode(cmos_read(DAY), binary)?;
    let month = decode(cmos_read(MONTH), binary)?;
    let year = decode(cmos_read(YEAR), binary)?;
    let century = decode(cmos_read(CENTURY), binary)?;
    if century == 0 || year > 99 {
        return None;
    }
    Some(Fields {
        second,
        minute,
        hour,
        day,
        month,
        year: i32::from(century) * 100 + i32::from(year),
    })
}

fn write_chip(secs: u64) -> bool {
    let Some(c) = galexy_core::utc_to_civil(secs) else {
        return false;
    };
    let status = cmos_read(STATUS_B);
    cmos_write(STATUS_B, status | SET);
    cmos_write(SECONDS, bin_to_bcd(c.second));
    cmos_write(MINUTES, bin_to_bcd(c.minute));
    cmos_write(HOURS, bin_to_bcd(c.hour));
    cmos_write(DAY, bin_to_bcd(c.day));
    cmos_write(MONTH, bin_to_bcd(c.month));
    cmos_write(YEAR, bin_to_bcd((c.year % 100) as u8));
    cmos_write(CENTURY, bin_to_bcd((c.year / 100) as u8));
    // Leave the chip in BCD, 24-hour, running. DST is not our bit.
    let kept = status & !SET & !BINARY;
    cmos_write(STATUS_B, kept | HOUR_24);
    true
}

fn wait_settled() -> bool {
    for _ in 0..10_000 {
        if (cmos_read(STATUS_A) & UIP) == 0 {
            return true;
        }
    }
    false
}

fn decode(raw: u8, binary: bool) -> Option<u8> {
    if binary {
        Some(raw)
    } else {
        bcd_to_bin(raw)
    }
}

fn decode_hour(raw: u8, binary: bool, hour_24: bool) -> Option<u8> {
    if hour_24 {
        return decode(raw, binary);
    }
    let pm = (raw & 0x80) != 0;
    let hour = decode(raw & 0x7f, binary)?;
    match (hour, pm) {
        (12, false) => Some(0),
        (12, true) => Some(12),
        (1..=11, true) => Some(hour + 12),
        (1..=11, false) => Some(hour),
        _ => None,
    }
}

fn bcd_to_bin(v: u8) -> Option<u8> {
    let hi = v >> 4;
    let lo = v & 0x0f;
    if hi > 9 || lo > 9 {
        None
    } else {
        Some(hi * 10 + lo)
    }
}

fn bin_to_bcd(v: u8) -> u8 {
    (v / 10) << 4 | (v % 10)
}

fn cmos_read(index: u8) -> u8 {
    // SAFETY: the CMOS address/data pair. Bit 7 of the index holds NMI off.
    unsafe {
        let mut index_port = Port::<u8>::new(INDEX);
        let mut data = Port::<u8>::new(DATA);
        index_port.write(index | 0x80);
        data.read()
    }
}

fn cmos_write(index: u8, value: u8) {
    // SAFETY: same ports as [`cmos_read`].
    unsafe {
        let mut index_port = Port::<u8>::new(INDEX);
        let mut data = Port::<u8>::new(DATA);
        index_port.write(index | 0x80);
        data.write(value);
    }
}
