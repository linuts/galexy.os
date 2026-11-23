//! Local SSH client. Read on `/card/ssh` opens one session. The name
//! is the card. No address is passed, and the password is not an argument.

use galexy_abi::{Cap, TOKEN_READ, USER_WHOAMI};
use galexy_rt::{keyboard_cap, read, user, write_console, yield_now};
use galexy_ssh::{wipe, Session};
use ssh::Lane;

fn main() {
    ssh::touch();
    let mut name = [0u8; 32];
    let who = user(&mut name, USER_WHOAMI);
    if !who.ok || who.value == 0 {
        write_console(b"access denied\n");
        return;
    }
    let user_len = (who.value as usize).min(name.len());
    write_console(b"Password: ");
    let mut password = [0u8; 64];
    let Some(plen) = read_secret(&mut password) else {
        wipe(&mut password);
        write_console(b"access denied\n");
        return;
    };
    let Some((_text, lane)) = ssh::stream(TOKEN_READ as u8) else {
        wipe(&mut password);
        write_console(b"access denied\n");
        return;
    };
    if lane == 0 {
        wipe(&mut password);
        write_console(b"access denied\n");
        return;
    }
    let io = Lane::new(Cap::from_bits(lane));
    let mut session = match Session::client(io) {
        Ok(session) => session,
        Err(_) => {
            wipe(&mut password);
            write_console(b"access denied\n");
            return;
        }
    };
    let user_name = core::str::from_utf8(&name[..user_len]).unwrap_or("");
    let mut owned = password[..plen].to_vec();
    wipe(&mut password);
    let ok = session.login(user_name, &mut owned).is_ok();
    wipe(&mut owned);
    if ok {
        write_console(b"ssh: session\n");
    } else {
        write_console(b"access denied\n");
    }
}

fn read_secret(buf: &mut [u8]) -> Option<usize> {
    let kbd = keyboard_cap();
    let mut len = 0usize;
    loop {
        let mut chunk = [0u8; 8];
        let got = read(kbd, &mut chunk);
        if !got.ok {
            return None;
        }
        if got.value == 0 {
            let _ = yield_now();
            continue;
        }
        for &byte in &chunk[..got.value as usize] {
            if byte == b'\n' || byte == b'\r' {
                write_console(b"\n");
                return Some(len);
            }
            if byte.is_ascii_graphic() && len < buf.len() {
                buf[len] = byte;
                len += 1;
                write_console(b"*");
            }
        }
    }
}
