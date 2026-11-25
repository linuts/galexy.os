//! Reads one file as the user who just authenticated.
//!
//! The password arrives on the pipe, not argv. Login installs that
//! user's tokens. This task does not keep the service's card, and it
//! does not stay root.

use galexy_abi::{Cap, CapRights, FILE_CAP_BASE, USER_LOGIN};
use galexy_rt::{exit, open, read, user_name_pass, write};

fn main() {
    exit(run());
}

fn run() -> u64 {
    let stdin = Cap::new(FILE_CAP_BASE, CapRights::READ.union(CapRights::WRITE));
    let stdout = Cap::new(FILE_CAP_BASE + 1, CapRights::READ.union(CapRights::WRITE));
    let mut raw = [0u8; 256];
    let mut n = 0usize;
    loop {
        let mut buf = [0u8; 64];
        let got = read(stdin, &mut buf);
        if !got.ok || got.value == 0 {
            break;
        }
        let take = (got.value as usize).min(raw.len() - n);
        raw[n..n + take].copy_from_slice(&buf[..take]);
        n += take;
        if n == raw.len() {
            break;
        }
    }
    let Some((user, password, path)) = split(&raw[..n]) else {
        galexy_ssh::wipe(&mut raw);
        return 1;
    };
    let user = user.to_vec();
    let mut password = password.to_vec();
    let path = path.to_vec();
    galexy_ssh::wipe(&mut raw);
    let logged = user_name_pass(&user, &password, USER_LOGIN);
    galexy_ssh::wipe(&mut password);
    if !logged.ok {
        return 1;
    }
    if path.is_empty() {
        return 0;
    }
    let opened = open(&path);
    if !opened.ok {
        return 1;
    }
    let file = Cap::from_bits(opened.value);
    loop {
        let mut buf = [0u8; 256];
        let got = read(file, &mut buf);
        if !got.ok || got.value == 0 {
            break;
        }
        let _ = write(stdout, &buf[..got.value as usize]);
    }
    0
}

fn split(raw: &[u8]) -> Option<(&[u8], &[u8], &[u8])> {
    let user_end = raw.iter().position(|b| *b == 0)?;
    let pass_end = raw[user_end + 1..].iter().position(|b| *b == 0)? + user_end + 1;
    let path_end = raw[pass_end + 1..].iter().position(|b| *b == 0)? + pass_end + 1;
    Some((
        &raw[..user_end],
        &raw[user_end + 1..pass_end],
        &raw[pass_end + 1..path_end],
    ))
}
