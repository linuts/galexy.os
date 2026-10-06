use crate::crc32;

#[test]
fn empty_is_zero() {
    assert_eq!(crc32(b""), 0);
}

#[test]
fn known_vectors() {
    assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
    assert_eq!(crc32(b"galexy"), 0xF1B2_BBAA);
}
