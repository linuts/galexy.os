//! Where a console write may be cut without tearing an escape sequence.
//!
//! COM1 is shared with kernel logs. A budget cut in the middle of CSI
//! lets the next line land inside the sequence, and the host terminal
//! stops painting until a later byte resyncs it.

/// Longest prefix of `bytes`, at most `room` bytes, that ends in ground state.
///
/// `0` means the next complete sequence does not fit in `room`. Plain text
/// can be cut on any byte. ESC, CSI (`ESC [`), and the final byte of a
/// sequence stay together.
pub fn console_commit_len(bytes: &[u8], room: usize) -> usize {
    let mut state = Seq::Ground;
    let mut last_ground = 0usize;
    let n = room.min(bytes.len());
    for (i, &byte) in bytes.iter().enumerate().take(n) {
        state = match state {
            Seq::Ground => {
                if byte == 0x1b {
                    Seq::Esc
                } else {
                    Seq::Ground
                }
            }
            Seq::Esc => {
                if byte == b'[' {
                    Seq::Csi
                } else if byte == 0x1b {
                    Seq::Esc
                } else {
                    Seq::Ground
                }
            }
            Seq::Csi => {
                if byte == 0x1b {
                    Seq::Esc
                } else if (0x40..=0x7e).contains(&byte) {
                    Seq::Ground
                } else {
                    Seq::Csi
                }
            }
        };
        if state == Seq::Ground {
            last_ground = i + 1;
        }
    }
    last_ground
}

/// Parser state for [`console_commit_len`].
#[derive(PartialEq, Eq)]
enum Seq {
    Ground,
    Esc,
    Csi,
}
