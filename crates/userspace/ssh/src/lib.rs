//! Shared lane adapter. The packet stack stays in `net`.

use std::io::{self, Read, Write};

use galexy_abi::Cap;
use galexy_rt::{close, read, write};

/// Plaintext end of a lane `net` is already relaying.
pub struct Lane {
    cap: Cap,
}

impl Lane {
    /// Wraps a lane capability from a net reply.
    pub fn new(cap: Cap) -> Self {
        Self { cap }
    }
}

impl Read for Lane {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let got = read(self.cap, buf);
        if !got.ok {
            return Err(io::Error::new(io::ErrorKind::Other, "lane"));
        }
        Ok(got.value as usize)
    }
}

impl Write for Lane {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        let got = write(self.cap, buf);
        if !got.ok {
            return Err(io::Error::new(io::ErrorKind::Other, "lane"));
        }
        Ok(got.value as usize)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Drop for Lane {
    fn drop(&mut self) {
        let _ = close(self.cap);
    }
}

/// Asks `net` for a live lane on the name `ssh`. `dir` is read or write.
/// The name is the card. No address is passed.
pub fn stream(dir: u8) -> Option<(Vec<u8>, u64)> {
    let mut payload = [0u8; 5];
    payload[0] = galexy_abi::NET_OP_STREAM;
    payload[1] = dir;
    payload[2..].copy_from_slice(b"ssh");
    let mut reply = [0u8; galexy_abi::CHAN_MSG_MAX];
    let got = galexy_rt::net_rpc(&payload, &mut reply);
    if !got.ok || (got.value as usize) < 8 {
        return None;
    }
    let n = (got.value as usize).min(reply.len());
    let mut bits = [0u8; 8];
    bits.copy_from_slice(&reply[..8]);
    let lane = u64::from_le_bytes(bits);
    Some((reply[8..n].to_vec(), lane))
}

/// Links the guest `getrandom` backend.
pub fn touch() {
    let _ = RNG;
}

#[used]
static RNG: unsafe extern "Rust" fn(*mut u8, usize) -> Result<(), getrandom::Error> =
    __getrandom_v03_custom;

/// `getrandom` custom backend. `rdrand`, same as `net`, because the
/// target has no OS entropy source.
#[unsafe(no_mangle)]
unsafe extern "Rust" fn __getrandom_v03_custom(
    dest: *mut u8,
    len: usize,
) -> Result<(), getrandom::Error> {
    let buf = unsafe {
        core::ptr::write_bytes(dest, 0, len);
        core::slice::from_raw_parts_mut(dest, len)
    };
    for chunk in buf.chunks_mut(8) {
        let mut val: u64 = 0;
        let mut ok = 0u8;
        for _ in 0..16 {
            unsafe {
                core::arch::asm!(
                    "rdrand {v}",
                    "setc {c}",
                    v = out(reg) val,
                    c = lateout(reg_byte) ok,
                    options(nomem, nostack, preserves_flags),
                );
            }
            if ok == 1 {
                break;
            }
        }
        if ok != 1 {
            return Err(getrandom::Error::new_custom(1));
        }
        let bytes = val.to_ne_bytes();
        chunk.copy_from_slice(&bytes[..chunk.len()]);
    }
    Ok(())
}
