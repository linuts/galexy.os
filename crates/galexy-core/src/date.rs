//! Civil UTC, counted in seconds since 1970-01-01 00:00:00.
//!
//! No timezone, no leap seconds. A day is 86400 seconds. Years outside
//! 1970..=2099 are refused rather than normalized. The conversion is the
//! days-from-civil algorithm (Howard Hinnant, public domain).

/// First year [`civil_to_utc`] accepts.
pub const YEAR_MIN: i32 = 1970;
/// Last year [`civil_to_utc`] accepts.
pub const YEAR_MAX: i32 = 2099;

/// A civil UTC date. Fields are the numbers a person writes, not a packed
/// wire word: month and day start at 1.
#[derive(Clone, Copy)]
pub struct Civil {
    /// Full year, 1970..=2099.
    pub year: i32,
    /// Month, 1..=12.
    pub month: u8,
    /// Day of the month, 1..=31, and only when that month has the day.
    pub day: u8,
    /// Hour, 0..=23.
    pub hour: u8,
    /// Minute, 0..=59.
    pub minute: u8,
    /// Second, 0..=59. A leap second is `None`, not 60.
    pub second: u8,
}

/// Seconds since 1970-01-01 00:00:00 UTC, or `None` when the fields are
/// not a real civil time in [`YEAR_MIN`]..=[`YEAR_MAX`].
pub fn civil_to_utc(c: Civil) -> Option<u64> {
    if !(YEAR_MIN..=YEAR_MAX).contains(&c.year) {
        return None;
    }
    if c.month < 1 || c.month > 12 || c.hour > 23 || c.minute > 59 || c.second > 59 {
        return None;
    }
    let dim = days_in_month(c.year, c.month)?;
    if c.day < 1 || c.day > dim {
        return None;
    }
    let days = days_from_civil(c.year, u32::from(c.month), u32::from(c.day));
    if days < 0 {
        return None;
    }
    let day_secs = u64::from(c.hour) * 3600 + u64::from(c.minute) * 60 + u64::from(c.second);
    Some(u64::from(days as u32) * 86_400 + day_secs)
}

/// The civil fields of `secs`, or `None` when the year would fall outside
/// [`YEAR_MIN`]..=[`YEAR_MAX`].
pub fn utc_to_civil(secs: u64) -> Option<Civil> {
    let days = u32::try_from(secs / 86_400).ok()?;
    let tod = (secs % 86_400) as u32;
    let (year, month, day) = civil_from_days(days as i32);
    if !(YEAR_MIN..=YEAR_MAX).contains(&year) {
        return None;
    }
    Some(Civil {
        year,
        month: month as u8,
        day: day as u8,
        hour: (tod / 3600) as u8,
        minute: ((tod % 3600) / 60) as u8,
        second: (tod % 60) as u8,
    })
}

/// `YYYY-MM-DD HH:MM:SS`, exactly 19 bytes, UTC. Anything else is `None`.
pub fn parse_utc(text: &[u8]) -> Option<u64> {
    if text.len() != 19
        || text[4] != b'-'
        || text[7] != b'-'
        || text[10] != b' '
        || text[13] != b':'
        || text[16] != b':'
    {
        return None;
    }
    civil_to_utc(Civil {
        year: i32::from(parse_u16(&text[0..4])?),
        month: u8::try_from(parse_u16(&text[5..7])?).ok()?,
        day: u8::try_from(parse_u16(&text[8..10])?).ok()?,
        hour: u8::try_from(parse_u16(&text[11..13])?).ok()?,
        minute: u8::try_from(parse_u16(&text[14..16])?).ok()?,
        second: u8::try_from(parse_u16(&text[17..19])?).ok()?,
    })
}

/// Writes `YYYY-MM-DD HH:MM:SS` into `buf`. Returns 19, or `None` when
/// `secs` is not a civil time or `buf` is shorter than 19.
pub fn format_utc(secs: u64, buf: &mut [u8]) -> Option<usize> {
    if buf.len() < 19 {
        return None;
    }
    let c = utc_to_civil(secs)?;
    put_digits(&mut buf[0..4], c.year as u32, 4);
    buf[4] = b'-';
    put_digits(&mut buf[5..7], u32::from(c.month), 2);
    buf[7] = b'-';
    put_digits(&mut buf[8..10], u32::from(c.day), 2);
    buf[10] = b' ';
    put_digits(&mut buf[11..13], u32::from(c.hour), 2);
    buf[13] = b':';
    put_digits(&mut buf[14..16], u32::from(c.minute), 2);
    buf[16] = b':';
    put_digits(&mut buf[17..19], u32::from(c.second), 2);
    Some(19)
}

fn parse_u16(bytes: &[u8]) -> Option<u16> {
    let mut n: u16 = 0;
    for &b in bytes {
        if !b.is_ascii_digit() {
            return None;
        }
        n = n.checked_mul(10)?.checked_add(u16::from(b - b'0'))?;
    }
    Some(n)
}

fn put_digits(buf: &mut [u8], mut value: u32, width: usize) {
    for slot in buf.iter_mut().rev().take(width) {
        *slot = b'0' + (value % 10) as u8;
        value /= 10;
    }
}

fn is_leap(year: i32) -> bool {
    year % 4 == 0 && (year % 100 != 0 || year % 400 == 0)
}

fn days_in_month(year: i32, month: u8) -> Option<u8> {
    Some(match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if is_leap(year) => 29,
        2 => 28,
        _ => return None,
    })
}

/// Days since 1970-01-01. `month` is 1..=12.
fn days_from_civil(year: i32, month: u32, day: u32) -> i32 {
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = (y - era * 400) as u32;
    let mp = if month > 2 { month - 3 } else { month + 9 };
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe as i32 - 719468
}

fn civil_from_days(z: i32) -> (i32, u32, u32) {
    let z = z + 719468;
    let era = z.div_euclid(146097);
    let doe = (z - era * 146097) as u32;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe as i32 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if month <= 2 { y + 1 } else { y };
    (year, month, day)
}
