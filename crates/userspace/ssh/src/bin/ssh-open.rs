//! Local SSH client. `ssh user@address` dials that address on port 22
//! when a `sec` rule names `ssh-open`. A port other than 22 is
//! `ssh user@address:port`. The password is the peer account's
//! password, typed, not an argument.
//!
//! An empty pin asks once. Yes stores the peer key. A later dial has to
//! present that same key.

use galexy_abi::{Cap, TOKEN_READ, USER_WHOAMI};
use galexy_rt::{
    close, create, create_replace, keyboard_cap, open, read, user, write, write_console, yield_now,
};
use galexy_ssh::{fingerprint, unseen_key, wipe, Session};
use ssh::Lane;

const KNOWN: &[u8] = b"ssh.known";

fn main() {
    ssh::touch();
    // `std` owns the spawn blob. `galexy_rt::arg` stays empty here, so an
    // address typed at the shell would dial the stored name.
    let owned = spawn_arg();
    let typed = trim_arg(&owned);
    let mut name = [0u8; 32];
    let (user_name, dial) = if typed.is_empty() {
        let who = user(&mut name, USER_WHOAMI);
        if !who.ok || who.value == 0 {
            write_console(b"access denied\n");
            return;
        }
        let user_len = (who.value as usize).min(name.len());
        (
            core::str::from_utf8(&name[..user_len]).unwrap_or(""),
            &b"ssh"[..],
        )
    } else {
        let Some(at) = typed.iter().position(|b| *b == b'@') else {
            write_console(b"ssh <user>@<address>[:port]\n");
            return;
        };
        if at == 0 || at + 1 >= typed.len() {
            write_console(b"ssh <user>@<address>[:port]\n");
            return;
        }
        let user_len = at.min(name.len());
        name[..user_len].copy_from_slice(&typed[..user_len]);
        (
            core::str::from_utf8(&name[..user_len]).unwrap_or(""),
            &typed[at + 1..],
        )
    };
    let Some(lane) = open_lane(dial) else {
        return;
    };
    write_console(b"Password: ");
    let mut password = [0u8; 64];
    let Some(plen) = read_secret(&mut password) else {
        wipe(&mut password);
        write_console(b"access denied\n");
        return;
    };
    let io = Lane::new(Cap::from_bits(lane));
    let mut session = match Session::client(io) {
        Ok(session) => session,
        Err(_) => {
            wipe(&mut password);
            write_console(b"access denied\n");
            return;
        }
    };
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

/// The address from the shell. `std` puts this task's name first.
fn spawn_arg() -> Vec<u8> {
    let mut args = std::env::args();
    let _ = args.next();
    args.next()
        .map(|text| text.into_bytes())
        .unwrap_or_default()
}

/// Dials the stored peer. An unseen key asks, and yes is the only
/// answer that stores it. A changed key stops here.
fn trim_arg(mut text: &[u8]) -> &[u8] {
    while text.first() == Some(&0) || text.first() == Some(&b' ') {
        text = &text[1..];
    }
    while text.last() == Some(&0) || text.last() == Some(&b' ') || text.last() == Some(&b'\n') {
        text = &text[..text.len() - 1];
    }
    text
}

fn open_lane(name: &[u8]) -> Option<u64> {
    let (text, lane) = ssh::stream_name(TOKEN_READ as u8, name)?;
    if text.starts_with(b"changed") {
        write_console(b"The host key has changed. The connection is closed.\n");
        return None;
    }
    if let Some(key) = unseen_key(&text) {
        if !confirm(&key) {
            let _ = ssh::stream_name(4, name);
            write_console(b"access denied\n");
            return None;
        }
        let (answer, _) = ssh::stream_name(3, name)?;
        if !answer.starts_with(b"ok") {
            write_console(b"access denied\n");
            return None;
        }
        store_known(&key);
        let (again, lane) = ssh::stream_name(TOKEN_READ as u8, name)?;
        if again.starts_with(b"changed") || lane == 0 {
            write_console(b"access denied\n");
            return None;
        }
        return Some(lane);
    }
    if lane == 0 {
        write_console(b"access denied\n");
        return None;
    }
    Some(lane)
}

fn confirm(key: &[u8; 32]) -> bool {
    let fp = fingerprint(key);
    write_console(b"The authenticity of this host cannot be established.\n");
    write_console(b"fp=");
    let mut hex = [0u8; 16];
    const H: &[u8] = b"0123456789abcdef";
    for (i, byte) in fp.iter().enumerate() {
        hex[i * 2] = H[(byte >> 4) as usize];
        hex[i * 2 + 1] = H[(byte & 0xf) as usize];
    }
    write_console(&hex);
    write_console(b"\nAre you sure you want to continue connecting (yes/no)? ");
    let mut line = [0u8; 8];
    let n = read_line(&mut line);
    n == 3 && &line[..3] == b"yes"
}

/// Rewrites the `ssh` line in the seat's list. The bytes are the public
/// key. The console shows the fingerprint, not this line.
fn store_known(key: &[u8; 32]) {
    let mut hex = [0u8; 64];
    const H: &[u8] = b"0123456789abcdef";
    for (i, byte) in key.iter().enumerate() {
        hex[i * 2] = H[(byte >> 4) as usize];
        hex[i * 2 + 1] = H[(byte & 0xf) as usize];
    }
    let mut prev = [0u8; 512];
    let mut out = [0u8; 512];
    let mut n = 0usize;
    if let Some(got) = read_known(&mut prev) {
        let mut at = 0usize;
        while at < got {
            let end = prev[at..got]
                .iter()
                .position(|b| *b == b'\n')
                .map(|i| at + i + 1)
                .unwrap_or(got);
            let line = &prev[at..end];
            if !line.starts_with(b"ssh ") && n + line.len() <= out.len() {
                out[n..n + line.len()].copy_from_slice(line);
                n += line.len();
            }
            if end == at {
                break;
            }
            at = end;
        }
    }
    if n + 4 + hex.len() + 1 > out.len() {
        return;
    }
    out[n..n + 4].copy_from_slice(b"ssh ");
    n += 4;
    out[n..n + hex.len()].copy_from_slice(&hex);
    n += hex.len();
    out[n] = b'\n';
    n += 1;
    let made = create_replace(KNOWN);
    let bits = if made.ok {
        made.value
    } else {
        let made = create(KNOWN);
        if !made.ok {
            return;
        }
        made.value
    };
    let cap = Cap::from_bits(bits);
    let _ = write(cap, &out[..n]);
    let _ = close(cap);
}

fn read_known(buf: &mut [u8]) -> Option<usize> {
    let got = open(KNOWN);
    if !got.ok {
        return None;
    }
    let cap = Cap::from_bits(got.value);
    let mut n = 0usize;
    loop {
        if n >= buf.len() {
            break;
        }
        let got = read(cap, &mut buf[n..]);
        if !got.ok || got.value == 0 {
            break;
        }
        n += got.value as usize;
    }
    let _ = close(cap);
    Some(n)
}

fn read_line(buf: &mut [u8]) -> usize {
    let kbd = keyboard_cap();
    let mut len = 0usize;
    loop {
        let mut chunk = [0u8; 8];
        let got = read(kbd, &mut chunk);
        if !got.ok {
            return 0;
        }
        if got.value == 0 {
            let _ = yield_now();
            continue;
        }
        for &byte in &chunk[..got.value as usize] {
            if byte == b'\n' || byte == b'\r' {
                write_console(b"\n");
                return len;
            }
            if byte.is_ascii_graphic() {
                if len < buf.len() {
                    buf[len] = byte;
                    len += 1;
                }
                write_console(&[byte]);
            }
        }
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
