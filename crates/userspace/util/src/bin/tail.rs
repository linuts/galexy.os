//! tail — last 10 lines of a file, or of the pipe spawn installed.
//!
//! Trust: the shell spawns this with `SPAWN_INHERIT` (the caller's galfs
//! cards). The ramdisk is trusted code. Long lines are truncated to 128
//! bytes. The ring lives in this image.

#![no_std]
#![no_main]

use galexy_abi::{Cap, CapRights, SysError, FILE_CAP_BASE};
use galexy_rt::{args, close, entry, open, read, write_console, write_std};

entry!(main);

const KEEP: usize = 10;
const LINE: usize = 128;

struct Ring {
    lines: [[u8; LINE]; KEEP],
    lens: [usize; KEEP],
    count: usize,
    cur: [u8; LINE],
    cur_len: usize,
}

impl Ring {
    const fn new() -> Self {
        Self {
            lines: [[0; LINE]; KEEP],
            lens: [0; KEEP],
            count: 0,
            cur: [0; LINE],
            cur_len: 0,
        }
    }

    fn push(&mut self, byte: u8) {
        if byte == b'\n' {
            self.commit();
            return;
        }
        if self.cur_len < LINE {
            self.cur[self.cur_len] = byte;
            self.cur_len += 1;
        }
    }

    fn commit(&mut self) {
        let slot = self.count % KEEP;
        let n = self.cur_len;
        self.lines[slot][..n].copy_from_slice(&self.cur[..n]);
        self.lens[slot] = n;
        self.cur_len = 0;
        self.count += 1;
    }

    fn finish(&mut self) {
        if self.cur_len > 0 {
            self.commit();
        }
    }

    fn write(&self) -> i32 {
        let n = self.count.min(KEEP);
        let start = self.count.saturating_sub(n);
        for i in start..self.count {
            let slot = i % KEEP;
            let len = self.lens[slot];
            let wrote = write_std(&self.lines[slot][..len]);
            let nl = write_std(b"\n");
            if !wrote.ok || !nl.ok {
                let _ = write_console(b"tail: failed\n");
                return 1;
            }
        }
        0
    }
}

fn main() -> i32 {
    let mut any = false;
    let mut code = 0;
    for path in args() {
        any = true;
        if show(path) != 0 {
            code = 1;
        }
    }
    if !any {
        let _ = write_console(b"tail: usage: tail <name>\n");
        return 1;
    }
    code
}

fn show(path: &[u8]) -> i32 {
    if path == b"-" {
        return copy_cap(Cap::new(FILE_CAP_BASE, CapRights::READ));
    }
    let opened = open(path);
    if !opened.ok {
        if opened.value == SysError::NotFound as u64 {
            let _ = write_console(b"tail: no such file\n");
        } else {
            let _ = write_console(b"tail: failed\n");
        }
        return 1;
    }
    let cap = Cap::from_bits(opened.value);
    let code = copy_cap(cap);
    let _ = close(cap);
    code
}

fn copy_cap(cap: Cap) -> i32 {
    let mut ring = Ring::new();
    let mut buf = [0u8; 256];
    loop {
        let got = read(cap, &mut buf);
        if !got.ok {
            let _ = write_console(b"tail: failed\n");
            return 1;
        }
        if got.value == 0 {
            break;
        }
        let n = (got.value as usize).min(buf.len());
        for byte in &buf[..n] {
            ring.push(*byte);
        }
    }
    ring.finish();
    ring.write()
}
