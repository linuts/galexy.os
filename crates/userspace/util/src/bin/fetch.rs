//! `fetch <name>`. The dashboard with no name stays in the shell.
//! This program is the dial, so the stamp is `fetch`.

#![no_std]
#![no_main]

use galexy_abi::{Cap, NET_OP_DIAL, USER_WHOAMI};
use galexy_rt::{arg, close, entry, net_rpc, read, user, write_console};

entry!(main);

fn main() -> i32 {
    let name = trim(arg());
    if name.is_empty() {
        write_console(b"fetch <name>\n");
        return 1;
    }
    let mut who = [0u8; 32];
    let got = user(&mut who, USER_WHOAMI);
    let who = if got.ok {
        trim(&who[..(got.value as usize).min(who.len())])
    } else {
        b"admin"
    };
    let mut payload = [0u8; 96];
    if name.len() + who.len() + 3 > payload.len() {
        write_console(b"bad\n");
        return 1;
    }
    payload[0] = NET_OP_DIAL;
    payload[1] = name.len() as u8;
    payload[2..2 + name.len()].copy_from_slice(name);
    let at = 2 + name.len();
    payload[at] = who.len() as u8;
    payload[at + 1..at + 1 + who.len()].copy_from_slice(who);
    let n = at + 1 + who.len();
    let mut reply = [0u8; galexy_abi::CHAN_MSG_MAX];
    let got = net_rpc(&payload[..n], &mut reply);
    if !got.ok {
        write_console(b"access denied\n");
        return 1;
    }
    let rn = (got.value as usize).min(reply.len());
    if rn < 8 {
        write_console(b"bad\n");
        return 1;
    }
    let mut bits = [0u8; 8];
    bits.copy_from_slice(&reply[..8]);
    let lane = u64::from_le_bytes(bits);
    if lane != 0 {
        let cap = Cap::from_bits(lane);
        loop {
            let mut buf = [0u8; 256];
            let got = read(cap, &mut buf);
            if !got.ok || got.value == 0 {
                break;
            }
            write_console(&buf[..got.value as usize]);
        }
        let _ = close(cap);
        write_console(b"\n");
    } else if rn > 8 {
        write_console(&reply[8..rn]);
    }
    0
}

fn trim(mut text: &[u8]) -> &[u8] {
    while text.first() == Some(&b' ') || text.first() == Some(&0) {
        text = &text[1..];
    }
    while text.last() == Some(&b' ') || text.last() == Some(&0) || text.last() == Some(&b'\n') {
        text = &text[..text.len() - 1];
    }
    text
}
