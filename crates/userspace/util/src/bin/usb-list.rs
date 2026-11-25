//! `usb list`. The stamp is `usb-list`, so a rule names this program.

#![no_std]
#![no_main]

use galexy_abi::USB_OP_LIST;
use galexy_rt::{entry, usb_rpc, write_console};

entry!(main);

fn main() -> i32 {
    let mut reply = [0u8; galexy_abi::CHAN_MSG_MAX];
    let got = usb_rpc(&[USB_OP_LIST], &mut reply);
    if !got.ok {
        write_console(b"access denied\n");
        return 1;
    }
    let n = (got.value as usize).min(reply.len());
    if n == 0 {
        write_console(b"none\n");
    } else {
        write_console(&reply[..n]);
    }
    0
}
