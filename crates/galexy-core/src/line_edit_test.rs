use super::{
    backspace_cell, line_backspace, line_delete, line_insert, line_left, line_right, shift_cursor,
    Key, KeyDec,
};

fn keys_of(bytes: &[u8]) -> [Key; 8] {
    let mut dec = KeyDec::new();
    let mut out = [Key::Ignore; 8];
    let mut n = 0usize;
    for &byte in bytes {
        let key = dec.feed(byte);
        if key == Key::More {
            continue;
        }
        let key = if key == Key::Escape {
            if let Some(b) = dec.take_replay() {
                let again = dec.feed(b);
                if n < out.len() {
                    out[n] = Key::Escape;
                    n += 1;
                }
                if again == Key::More {
                    continue;
                }
                again
            } else {
                Key::Escape
            }
        } else {
            key
        };
        if n < out.len() {
            out[n] = key;
            n += 1;
        }
    }
    out
}

#[test]
fn bs_and_del_are_backspace() {
    let got = keys_of(&[0x08, 0x7f]);
    assert_eq!(got[0], Key::Backspace);
    assert_eq!(got[1], Key::Backspace);
}

#[test]
fn arrows_include_parameters_and_ss3() {
    let got = keys_of(b"\x1b[D\x1b[C\x1b[A\x1b[B");
    assert_eq!(
        [got[0], got[1], got[2], got[3]],
        [Key::Left, Key::Right, Key::Up, Key::Down]
    );
    assert_eq!(keys_of(b"\x1b[1D")[0], Key::Left);
    assert_eq!(keys_of(b"\x1b[1;2C")[0], Key::Right);
    let ss3 = keys_of(b"\x1bOD\x1bOC");
    assert_eq!(ss3[0], Key::Left);
    assert_eq!(ss3[1], Key::Right);
}

#[test]
fn delete_home_end_and_csi_u_backspace() {
    assert_eq!(keys_of(b"\x1b[3~")[0], Key::Delete);
    let ends = keys_of(b"\x1b[H\x1b[F");
    assert_eq!(ends[0], Key::Home);
    assert_eq!(ends[1], Key::End);
    let tilde = keys_of(b"\x1b[1~\x1b[4~");
    assert_eq!(tilde[0], Key::Home);
    assert_eq!(tilde[1], Key::End);
    assert_eq!(keys_of(b"\x1b[127u")[0], Key::Backspace);
    assert_eq!(keys_of(b"\x1b[8u")[0], Key::Backspace);
}

#[test]
fn bare_escape_replays_the_next_byte() {
    let mut dec = KeyDec::new();
    assert_eq!(dec.feed(0x1b), Key::More);
    assert_eq!(dec.feed(b'a'), Key::Escape);
    assert_eq!(dec.take_replay(), Some(b'a'));
    assert_eq!(dec.feed(b'a'), Key::Char(b'a'));
}

#[test]
fn backspace_in_the_middle_and_past_the_end() {
    let mut buf = [0u8; 8];
    let mut len = 0usize;
    let mut pos = 0usize;
    assert!(line_insert(&mut buf, &mut len, &mut pos, b'a'));
    assert!(line_insert(&mut buf, &mut len, &mut pos, b'c'));
    assert!(line_left(&mut pos, len));
    assert!(line_insert(&mut buf, &mut len, &mut pos, b'b'));
    assert_eq!(&buf[..len], b"abc");
    assert_eq!(pos, 2);
    assert!(line_backspace(&mut buf, &mut len, &mut pos));
    assert_eq!(&buf[..len], b"ac");
    assert_eq!(pos, 1);
    assert!(line_delete(&mut buf, &mut len, &mut pos));
    assert_eq!(&buf[..len], b"a");
    assert_eq!(pos, 1);
    assert!(!line_delete(&mut buf, &mut len, &mut pos));
    assert!(line_backspace(&mut buf, &mut len, &mut pos));
    assert_eq!(len, 0);
    assert!(!line_backspace(&mut buf, &mut len, &mut pos));

    // A cursor past the line is pulled back. The delete still runs.
    pos = 9;
    len = 1;
    buf[0] = b'z';
    assert!(line_backspace(&mut buf, &mut len, &mut pos));
    assert_eq!(len, 0);
    assert_eq!(pos, 0);
}

#[test]
fn backspace_of_a_full_buffer_stays_inside() {
    let mut buf = [0u8; 4];
    let mut len = 0usize;
    let mut pos = 0usize;
    for b in b"abcd" {
        assert!(line_insert(&mut buf, &mut len, &mut pos, *b));
    }
    assert!(!line_insert(&mut buf, &mut len, &mut pos, b'e'));
    assert_eq!(&buf[..], b"abcd");
    assert!(line_left(&mut pos, len));
    assert!(line_left(&mut pos, len));
    assert!(line_backspace(&mut buf, &mut len, &mut pos));
    assert_eq!(&buf[..len], b"acd");
    assert_eq!(pos, 1);
    assert!(!line_right(&mut pos, len) || pos <= len);
    while line_right(&mut pos, len) {}
    assert_eq!(pos, len);
}

#[test]
fn cursor_wraps_left_onto_the_previous_row() {
    // 10 columns. Column 0 of row 1 steps back onto the last cell of row 0.
    assert_eq!(shift_cursor(0, 1, 10, 3, -1), (9, 0));
    // Sitting just past the last cell is the same as the next row's start.
    assert_eq!(shift_cursor(10, 0, 10, 3, -1), (9, 0));
    assert_eq!(shift_cursor(0, 0, 10, 3, -1), (0, 0));
    assert_eq!(backspace_cell(0, 0, 10, 3), None);
    assert_eq!(backspace_cell(0, 1, 10, 3), Some((9, 0)));
    assert_eq!(backspace_cell(10, 0, 10, 3), Some((9, 0)));
    assert_eq!(shift_cursor(8, 0, 10, 3, 5), (3, 1));
}

#[test]
fn random_bytes_keep_the_cursor_inside_the_line() {
    let mut dec = KeyDec::new();
    let mut buf = [0u8; 16];
    let mut len = 0usize;
    let mut pos = 0usize;
    for i in 0u16..400 {
        let byte = (i.wrapping_mul(17).wrapping_add(3)) as u8;
        match dec.feed(byte) {
            Key::Char(c) => {
                let _ = line_insert(&mut buf, &mut len, &mut pos, c);
            }
            Key::Backspace => {
                let _ = line_backspace(&mut buf, &mut len, &mut pos);
            }
            Key::Delete => {
                let _ = line_delete(&mut buf, &mut len, &mut pos);
            }
            Key::Left => {
                let _ = line_left(&mut pos, len);
            }
            Key::Right => {
                let _ = line_right(&mut pos, len);
            }
            Key::Home => pos = 0,
            Key::End => pos = len,
            Key::Escape => {
                if let Some(b) = dec.take_replay() {
                    let _ = dec.feed(b);
                }
            }
            _ => {}
        }
        assert!(pos <= len && len <= buf.len());
    }
}
