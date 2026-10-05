//! Host tests of the tar cursor: handcrafted USTAR archives (galexy-core
//! cannot depend on a tar writer — the no-deps rule).

use super::TarCursor;

/// The test's own copy of the private padding rule.
fn ceil_to_block(size: usize) -> usize {
    (size + BLOCK - 1) / BLOCK * BLOCK
}

const BLOCK: usize = 512;

/// Builds one minimal USTAR header (name, size, typeflag), zero-padded.
fn header(name: &str, size: usize, typeflag: u8) -> [u8; BLOCK] {
    let mut h = [0u8; BLOCK];
    // name
    let name_bytes = name.as_bytes();
    h[..name_bytes.len()].copy_from_slice(name_bytes);
    // size field: 11 octal digits + NUL (standard USTAR: digits then NUL).
    let mut size_field = [b'0'; 11];
    let mut s = size;
    let mut digit_pos = 10;
    loop {
        size_field[digit_pos] = b'0' + (s % 8) as u8;
        s /= 8;
        if s == 0 {
            break;
        }
        digit_pos = digit_pos
            .checked_sub(1)
            .expect("size too large for the test");
    }
    h[124..124 + 11].copy_from_slice(&size_field);
    h[135] = 0; // NUL-terminated size field
    h[156] = typeflag;
    h
}

fn archive(entries: &[([u8; BLOCK], &[u8])]) -> [u8; BLOCK * 8] {
    // Enough for the eased tests (entries + final zeros finish under 8
    // blocks).
    let mut out = [0u8; BLOCK * 8];
    let mut pos = 0usize;
    for (hdr, body) in entries {
        out[pos..pos + BLOCK].copy_from_slice(hdr);
        pos += BLOCK;
        let padded = ceil_to_block(body.len());
        out[pos..pos + body.len()].copy_from_slice(body);
        pos += padded;
    }
    // Trailing zero blocks are already zeros.
    out
}

#[test]
fn single_small_file() {
    let data = archive(&[(header("hello.txt", 5, b'0'), b"world")]);
    let mut c = TarCursor::new(&data);
    let (name, body) = c.next_file().expect("entry present");
    assert_eq!(name, "hello.txt");
    assert_eq!(body, b"world");
    assert!(c.next_file().is_none());
}

#[test]
fn two_files_in_order() {
    let data = archive(&[
        (header("a", 3, b'0'), b"xxx"),
        (header("b", 4, b'0'), b"yyyy"),
    ]);
    let mut c = TarCursor::new(&data);
    let (n1, b1) = c.next_file().expect("first entry");
    assert_eq!(n1, "a");
    assert_eq!(b1, b"xxx");
    let (n2, b2) = c.next_file().expect("second entry");
    assert_eq!(n2, "b");
    assert_eq!(b2, b"yyyy");
    assert!(c.next_file().is_none());
}

#[test]
fn directory_entries_skipped() {
    let data = archive(&[
        (header("dir/", 0, b'5'), b""),
        (header("dir/file", 2, b'0'), b"ok"),
    ]);
    let mut c = TarCursor::new(&data);
    let (name, body) = c.next_file().expect("regular file behind the dir");
    assert_eq!(name, "dir/file");
    assert_eq!(body, b"ok");
    assert!(c.next_file().is_none());
}

#[test]
fn size_spanning_blocks_padded_body() {
    // 600 bytes → padded to 1024; the next entry starts at +1024.
    let body: [u8; 600] = [b'A'; 600];
    let data = archive(&[
        (header("blob", 600, b'0'), &body),
        (header("after", 2, b'0'), b"ok"),
    ]);
    let mut c = TarCursor::new(&data);
    let (n1, b1) = c.next_file().expect("first entry");
    assert_eq!(n1, "blob");
    assert_eq!(b1.len(), 600);
    assert!(b1.iter().all(|&b| b == b'A'));
    let (n2, b2) = c.next_file().expect("second entry (padding skipped)");
    assert_eq!(n2, "after");
    assert_eq!(b2, b"ok");
}

#[test]
fn empty_data_is_none() {
    // An empty slice: cursor yields none immediately.
    let mut c = TarCursor::new(&[]);
    assert!(c.next_file().is_none());
}

#[test]
fn trailing_garbage_final_zero_block() {
    // First entry, then a zero block — cursor terminates there.
    let data = archive(&[(header("only", 1, b'0'), b"1")]);
    let mut c = TarCursor::new(&data);
    let (name, body) = c.next_file().expect("entry");
    assert_eq!(name, "only");
    assert_eq!(body, b"1");
    assert!(c.next_file().is_none());
}
