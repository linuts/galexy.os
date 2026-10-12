use crate::date::*;

#[test]
fn epoch_and_known_days() {
    assert_eq!(civil_to_utc(ymd(1970, 1, 1, 0, 0, 0)), Some(0));
    assert_eq!(civil_to_utc(ymd(2024, 2, 29, 0, 0, 0)), Some(1_709_164_800));
    assert_eq!(civil_to_utc(ymd(2024, 3, 1, 0, 0, 0)), Some(1_709_251_200));
    assert_eq!(
        civil_to_utc(ymd(2026, 10, 12, 15, 4, 0)),
        Some(1_791_817_440)
    );
    assert_eq!(
        civil_to_utc(ymd(2099, 12, 31, 23, 59, 59)),
        Some(4_102_444_799)
    );
    assert_eq!(civil_to_utc(ymd(2000, 2, 29, 0, 0, 0)), Some(951_782_400));
}

#[test]
fn roundtrip_month_ends() {
    let samples = [
        ymd(1970, 1, 1, 0, 0, 0),
        ymd(1999, 12, 31, 23, 59, 59),
        ymd(2000, 2, 29, 12, 0, 0),
        ymd(2024, 2, 28, 0, 0, 1),
        ymd(2024, 2, 29, 23, 59, 59),
        ymd(2026, 10, 12, 15, 4, 30),
        ymd(2099, 12, 31, 23, 59, 59),
    ];
    for c in samples {
        let secs = civil_to_utc(c).expect("sample");
        let back = utc_to_civil(secs).expect("back");
        assert_eq!(
            (
                back.year,
                back.month,
                back.day,
                back.hour,
                back.minute,
                back.second
            ),
            (c.year, c.month, c.day, c.hour, c.minute, c.second)
        );
        let mut buf = [0u8; 19];
        assert_eq!(format_utc(secs, &mut buf), Some(19));
        assert_eq!(parse_utc(&buf), Some(secs));
    }
}

#[test]
fn refuses_impossible_dates() {
    assert!(civil_to_utc(ymd(1969, 12, 31, 23, 59, 59)).is_none());
    assert!(civil_to_utc(ymd(2100, 1, 1, 0, 0, 0)).is_none());
    assert!(civil_to_utc(ymd(2025, 2, 29, 0, 0, 0)).is_none());
    assert!(civil_to_utc(ymd(2026, 4, 31, 0, 0, 0)).is_none());
    assert!(civil_to_utc(ymd(2026, 10, 12, 24, 0, 0)).is_none());
    assert!(civil_to_utc(ymd(2026, 10, 12, 15, 4, 60)).is_none());
    assert!(parse_utc(b"2026-10-12T15:04:00").is_none());
    assert!(parse_utc(b"2026-10-12 15:04:00 ").is_none());
    assert!(utc_to_civil(4_102_444_800).is_none());
}

fn ymd(year: i32, month: u8, day: u8, hour: u8, minute: u8, second: u8) -> Civil {
    Civil {
        year,
        month,
        day,
        hour,
        minute,
        second,
    }
}
