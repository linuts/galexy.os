use super::console_commit_len;

#[test]
fn plain_text_cuts_at_the_budget() {
    let bytes = b"abcdefghijklmnopqrstuvwxyz";
    assert_eq!(console_commit_len(bytes, 4), 4);
    assert_eq!(console_commit_len(bytes, 100), bytes.len());
    assert_eq!(console_commit_len(bytes, 0), 0);
}

#[test]
fn csi_is_not_split() {
    // `ESC [ 2 K` is four bytes. A 3-byte budget ends inside the sequence.
    let bytes = b"\x1b[2Khello";
    assert_eq!(console_commit_len(bytes, 3), 0);
    assert_eq!(console_commit_len(bytes, 4), 4);
    assert_eq!(console_commit_len(bytes, 5), 5);
}

#[test]
fn cut_falls_back_to_the_previous_ground_byte() {
    let bytes = b"ab\x1b[2Kcd";
    // room 5 ends on '2' inside CSI. Last ground byte is 'b'.
    assert_eq!(console_commit_len(bytes, 5), 2);
    assert_eq!(console_commit_len(bytes, 6), 6);
}

#[test]
fn esc_esc_stays_an_escape() {
    let bytes = b"\x1b\x1b[H";
    assert_eq!(console_commit_len(bytes, 2), 0);
    assert_eq!(console_commit_len(bytes, 4), 4);
}
