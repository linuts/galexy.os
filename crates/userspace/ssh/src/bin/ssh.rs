//! The SSH service. `net` hands it a lane when a listen rule names
//! `ssh`. It does not parse TCP.

use galexy_abi::{
    Cap, FILE_CAP_BASE, SPAWN_CAP_SHIFT, SPAWN_NO_FG, SPAWN_WAIT, SPAWN_WITH_CAPS, TOKEN_WRITE,
};
use galexy_rt::{close, pipe, read, sleep_ms, spawn_with, write, write_console};
use galexy_ssh::{wipe, HostKey, Session};
use ssh::Lane;

fn main() {
    ssh::touch();
    let host = HostKey::shipped();
    let mut told_denied = false;
    let mut listening = false;
    loop {
        let Some((text, lane)) = ssh::stream(TOKEN_WRITE as u8) else {
            note_denied(&mut told_denied);
            let _ = sleep_ms(200);
            continue;
        };
        if text.starts_with(b"idle") {
            let _ = sleep_ms(200);
            continue;
        }
        if text.starts_with(b"attached") {
            if !listening {
                write_console(b"ssh: attached\n");
            }
            told_denied = false;
            listening = true;
            continue;
        }
        if lane == 0 {
            if text.starts_with(b"access denied") || text.starts_with(b"missing") {
                note_denied(&mut told_denied);
            }
            let _ = sleep_ms(200);
            continue;
        }
        told_denied = false;
        if one_session(lane, &host) {
            write_console(b"ssh: file\n");
        }
        let _ = sleep_ms(8_000);
    }
}

fn note_denied(told: &mut bool) {
    if !*told {
        write_console(b"ssh: denied\n");
        *told = true;
    }
}

fn one_session(lane: u64, host: &HostKey) -> bool {
    let io = Lane::new(Cap::from_bits(lane));
    let mut session = match Session::server(io, host) {
        Ok(session) => session,
        Err(_) => {
            write_console(b"ssh: refused\n");
            return false;
        }
    };
    let mut saved_user = std::string::String::new();
    let mut saved_pass = std::vec::Vec::new();
    let authed = session.accept_password(|user, password| {
        saved_user = std::string::String::from(user);
        saved_pass = password.to_vec();
        ask(user, password, b"").is_some()
    });
    if authed.is_err() {
        wipe(&mut saved_pass);
        write_console(b"ssh: auth\n");
        return false;
    }
    let mut got_file = false;
    let user = saved_user.clone();
    let _ = session.serve_sftp(|path| {
        let bytes = read_file(&user, &saved_pass, path)?;
        got_file = true;
        Some(bytes)
    });
    wipe(&mut saved_pass);
    got_file
}

fn read_file(user: &str, password: &[u8], path: &str) -> Option<std::vec::Vec<u8>> {
    if path.is_empty() || path.len() > 64 || path.as_bytes().contains(&0) {
        return None;
    }
    ask(user, password, path.as_bytes())
}

/// Login as `user` in a short-lived helper and, when `path` is set, read
/// that file with the tokens the login installed. The password crosses
/// a pipe, not the command line.
fn ask(user: &str, password: &[u8], path: &[u8]) -> Option<std::vec::Vec<u8>> {
    if user.is_empty() || user.len() > 32 || password.is_empty() || password.len() > 64 {
        return None;
    }
    let mut inbound = [0u64; 2];
    let mut outbound = [0u64; 2];
    if !pipe(&mut inbound).ok || !pipe(&mut outbound).ok {
        return None;
    }
    let mut request = std::vec::Vec::new();
    request.extend_from_slice(user.as_bytes());
    request.push(0);
    request.extend_from_slice(password);
    request.push(0);
    request.extend_from_slice(path);
    request.push(0);
    let wrote = write(Cap::from_bits(inbound[1]), &request);
    wipe(&mut request);
    let _ = close(Cap::from_bits(inbound[1]));
    if !wrote.ok {
        close_both(inbound[0], outbound[0], outbound[1]);
        return None;
    }
    let slot_in = file_slot(inbound[0])?;
    let slot_out = file_slot(outbound[1])?;
    let grants = SPAWN_WITH_CAPS
        | SPAWN_WAIT
        | SPAWN_NO_FG
        | (slot_in << SPAWN_CAP_SHIFT)
        | (slot_out << (SPAWN_CAP_SHIFT + 4));
    let child = spawn_with(b"ssh-read", b"", grants);
    if !child.ok || child.value != 0 {
        let _ = close(Cap::from_bits(inbound[0]));
        let _ = close(Cap::from_bits(outbound[0]));
        let _ = close(Cap::from_bits(outbound[1]));
        return None;
    }
    let mut body = std::vec::Vec::new();
    let mut buf = [0u8; 256];
    loop {
        let got = read(Cap::from_bits(outbound[0]), &mut buf);
        if !got.ok || got.value == 0 {
            break;
        }
        body.extend_from_slice(&buf[..got.value as usize]);
        if body.len() > 4096 {
            break;
        }
    }
    let _ = close(Cap::from_bits(outbound[0]));
    Some(body)
}

fn close_both(a: u64, b: u64, c: u64) {
    let _ = close(Cap::from_bits(a));
    let _ = close(Cap::from_bits(b));
    let _ = close(Cap::from_bits(c));
}

fn file_slot(bits: u64) -> Option<u64> {
    let slot = Cap::from_bits(bits).index().checked_sub(FILE_CAP_BASE)?;
    if slot > 14 {
        None
    } else {
        Some(slot)
    }
}
