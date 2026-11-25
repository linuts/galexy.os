//! dial — ask `net` for a name, twice, in one run.
//!
//! A missing sec rule is `access denied`. The program stays up.

#![no_std]
#![no_main]

use galexy_abi::{Cap, USER_WHOAMI};
use galexy_rt::{args, close, entry, net_rpc, read, user, write_console};

entry!(main);

fn main() -> i32 {
    let mut name = [0u8; 64];
    let mut nlen = 0usize;
    if let Some(arg) = args().next() {
        if !arg.is_empty() && arg.len() <= name.len() {
            name[..arg.len()].copy_from_slice(arg);
            nlen = arg.len();
        }
    }
    if nlen == 0 {
        let _ = write_console(b"dial: usage: dial <name>\n");
        return 1;
    }
    let first = once(&name[..nlen]);
    let second = once(&name[..nlen]);
    if first != 0 {
        first
    } else {
        second
    }
}

fn once(name: &[u8]) -> i32 {
    let mut who = [0u8; 32];
    let got = user(&mut who, USER_WHOAMI);
    let who = if got.ok {
        let n = (got.value as usize).min(who.len());
        &who[..n]
    } else {
        b"eve".as_slice()
    };
    let mut payload = [0u8; 96];
    if name.len() + who.len() + 3 > payload.len() {
        let _ = write_console(b"bad\n");
        return 1;
    }
    payload[0] = galexy_abi::NET_OP_DIAL;
    payload[1] = name.len() as u8;
    payload[2..2 + name.len()].copy_from_slice(name);
    let at = 2 + name.len();
    payload[at] = who.len() as u8;
    payload[at + 1..at + 1 + who.len()].copy_from_slice(who);
    let n = at + 1 + who.len();
    let mut reply = [0u8; galexy_abi::CHAN_MSG_MAX];
    let got = net_rpc(&payload[..n], &mut reply);
    if !got.ok {
        let _ = write_console(b"access denied\n");
        let _ = write_console(b"still up\n");
        return 1;
    }
    let n = (got.value as usize).min(reply.len());
    if n < 8 {
        let _ = write_console(b"bad\n");
        return 1;
    }
    let mut bits = [0u8; 8];
    bits.copy_from_slice(&reply[..8]);
    let lane = u64::from_le_bytes(bits);
    if lane != 0 {
        let cap = Cap::from_bits(lane);
        loop {
            let mut buf = [0u8; 64];
            let got = read(cap, &mut buf);
            if !got.ok || got.value == 0 {
                break;
            }
            let _ = write_console(&buf[..got.value as usize]);
        }
        let _ = close(cap);
        let _ = write_console(b"\n");
    } else if n > 8 {
        let text = &reply[8..n];
        let _ = write_console(text);
        if text.starts_with(b"access denied") {
            let _ = write_console(b"still up\n");
            return 1;
        }
    }
    0
}
