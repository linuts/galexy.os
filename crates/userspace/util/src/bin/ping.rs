//! `ping` and `nslookup`. The shell spawns this program so the stamp is
//! `ping`, not the shell. A leading `lookup ` asks for addresses only.

#![no_std]
#![no_main]

use galexy_abi::{Cap, NET_OP_PING, PING_LOOKUP};
use galexy_rt::{arg, close, entry, net_rpc, read, write_console};

entry!(main);

fn main() -> i32 {
    let bytes = trim(arg());
    if bytes.is_empty() {
        write_console(b"ping <ip or name>\n");
        return 1;
    }
    let lookup = bytes.strip_prefix(b"lookup ").is_some();
    let name = if lookup {
        trim(&bytes[b"lookup ".len()..])
    } else {
        bytes
    };
    if name.is_empty() || name.len() + 4 > galexy_abi::INIT_RPC_MAX {
        write_console(b"bad\n");
        return 1;
    }
    let mut payload = [0u8; galexy_abi::INIT_RPC_MAX];
    let n = if lookup {
        payload[0] = NET_OP_PING;
        payload[1] = PING_LOOKUP;
        payload[2] = name.len() as u8;
        payload[3..3 + name.len()].copy_from_slice(name);
        3 + name.len()
    } else if let Some(ip) = parse_ip(name) {
        payload[0] = NET_OP_PING;
        payload[1..5].copy_from_slice(&ip);
        5
    } else {
        payload[0] = NET_OP_PING;
        payload[1] = name.len() as u8;
        payload[2..2 + name.len()].copy_from_slice(name);
        2 + name.len()
    };
    show(&payload[..n])
}

fn show(payload: &[u8]) -> i32 {
    let mut reply = [0u8; galexy_abi::CHAN_MSG_MAX];
    let got = net_rpc(payload, &mut reply);
    if !got.ok {
        write_console(b"access denied\n");
        return 1;
    }
    let n = (got.value as usize).min(reply.len());
    if n < 8 {
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
    } else if n > 8 {
        write_console(&reply[8..n]);
    }
    0
}

fn parse_ip(text: &[u8]) -> Option<[u8; 4]> {
    let mut out = [0u8; 4];
    let mut part = 0usize;
    let mut acc = 0u16;
    let mut digits = 0u8;
    for &b in text {
        if b == b'.' {
            if digits == 0 || part == 3 {
                return None;
            }
            out[part] = acc as u8;
            part += 1;
            acc = 0;
            digits = 0;
            continue;
        }
        if !b.is_ascii_digit() {
            return None;
        }
        acc = acc * 10 + u16::from(b - b'0');
        if acc > 255 {
            return None;
        }
        digits += 1;
    }
    if part != 3 || digits == 0 {
        return None;
    }
    out[3] = acc as u8;
    Some(out)
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
