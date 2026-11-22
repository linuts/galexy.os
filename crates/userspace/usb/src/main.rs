//! The `usb` service. It is the only task that reads the controller.
//!
//! The kernel moves transfer rings. This task checks descriptors and
//! walks [`drivers::TABLE`]. A HID boot keyboard is claimed and armed.
//! Any other device is listed and left unclaimed.

#![no_std]
#![no_main]

use galexy_abi::{Cap, INIT_RPC_HDR, USB_OP_LIST};
use galexy_rt::{
    chan_recv_poll, chan_send, channel, entry, sleep_ms, usb_arm, usb_read, write_console,
};
use galexy_usb::{decode_string, dispatch, ids_of, interface_count, list_line};

mod drivers;

entry!(main);

const MAX_DEV: usize = 8;
const LINE: usize = 96;

fn main() -> i32 {
    let mut ends = [0u64; 2];
    if !channel(&mut ends).ok {
        loop {
            let _ = sleep_ms(1_000);
        }
    }
    let rx = Cap::from_bits(ends[0]);
    let _peer = ends[1];
    let mut lines = [[0u8; LINE]; MAX_DEV];
    let mut lens = [0usize; MAX_DEV];
    let mut ports = [0u8; MAX_DEV];
    let mut count = 0usize;
    let mut logged_miss = false;
    loop {
        let mut rec = [0u8; 512];
        let got = usb_read(&mut rec);
        if !got.ok {
            if !logged_miss {
                write_console(b"[usb] no controller\n");
                logged_miss = true;
            }
        } else if got.value > 0 {
            let n = (got.value as usize).min(rec.len());
            if let Some((line, len, port)) = accept(&rec[..n]) {
                if count < MAX_DEV {
                    lines[count][..len].copy_from_slice(&line[..len]);
                    lens[count] = len;
                    ports[count] = port;
                    count += 1;
                }
            }
        }
        let mut msg = [0u8; galexy_abi::CHAN_MSG_MAX];
        let got = chan_recv_poll(rx, &mut msg);
        if got.ok {
            let n = got.value as usize;
            let op = if n > INIT_RPC_HDR {
                msg[INIT_RPC_HDR]
            } else {
                0
            };
            if op == USB_OP_LIST {
                let mask = u16::from_le_bytes([msg[2], msg[3]]);
                let mut reply = [0u8; galexy_abi::CHAN_MSG_MAX];
                let mut at = 0usize;
                let mut shown = 0usize;
                for i in 0..count {
                    let port = ports[i];
                    let bit = if port == 0 || port > 16 {
                        0
                    } else {
                        1u16 << (port - 1)
                    };
                    if mask & bit == 0 {
                        continue;
                    }
                    let len = lens[i];
                    if at + len > reply.len() {
                        break;
                    }
                    reply[at..at + len].copy_from_slice(&lines[i][..len]);
                    at += len;
                    shown += 1;
                }
                if count > 0 && shown == 0 {
                    let _ = chan_send(rx, b"access denied\n", 0, 0);
                } else {
                    let _ = chan_send(rx, &reply[..at], 0, 0);
                }
            } else {
                let _ = chan_send(rx, b"bad\n", 0, 0);
            }
        }
        let _ = sleep_ms(20);
    }
}

/// One list line, or nothing when the descriptor is refused.
///
/// A matching table row's function runs here. No row means the device
/// stays unclaimed. The line is also what `usb list` prints later.
fn accept(buf: &[u8]) -> Option<([u8; LINE], usize, u8)> {
    if buf.len() < 24 {
        return None;
    }
    let port = buf[0];
    let addr = buf[1];
    let flags = buf[2];
    let dev = &buf[3..21];
    let cfg_len = u16::from_le_bytes([buf[21], buf[22]]) as usize;
    if buf.len() < 23 + cfg_len + 1 {
        return None;
    }
    let cfg = &buf[23..23 + cfg_len];
    let str_at = 23 + cfg_len;
    let str_len = buf[str_at] as usize;
    if buf.len() < str_at + 1 + str_len {
        return None;
    }
    let raw = &buf[str_at + 1..str_at + 1 + str_len];
    if flags & 1 != 0 {
        write_console(b"[usb] bad\n");
        return None;
    }
    let ids = match ids_of(dev, cfg) {
        Ok(ids) => ids,
        Err(_) => {
            write_console(b"[usb] bad\n");
            return None;
        }
    };
    if !cfg.is_empty() && interface_count(cfg).is_err() {
        write_console(b"[usb] bad\n");
        return None;
    }
    let mut name = [0u8; 64];
    let name_len = if raw.is_empty() {
        0
    } else {
        match decode_string(raw, &mut name) {
            Ok(n) => n,
            Err(_) => {
                write_console(b"[usb] bad\n");
                return None;
            }
        }
    };
    let name = core::str::from_utf8(&name[..name_len]).unwrap_or("");
    let mut line = [0u8; LINE];
    let n = list_line(port, addr, &ids, name, &mut line);
    if galexy_usb::claims(&ids, drivers::TABLE) && !usb_arm(port).ok {
        write_console(b"[usb] arm failed\n");
    }
    write_console(&line[..n]);
    dispatch(core::slice::from_ref(&ids), drivers::TABLE, |_| {
        write_console(b"unclaimed\n");
    });
    Some((line, n, port))
}
