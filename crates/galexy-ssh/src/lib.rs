//! SSH-2 and a small SFTP subset.
//!
//! The only key exchange is `curve25519-sha256`, and only together with
//! the strict marker. The only host key is `ssh-ed25519`. The only
//! cipher is `chacha20-poly1305@openssh.com`. The MAC is that AEAD.
//! SHA-1, CBC, `ssh-rsa`, `ssh-dss`, `hmac-sha1`, and the
//! diffie-hellman groups are not offered and are not selected.

mod msg;
mod wire;

use std::io::{Read, Write};

use ed25519_dalek::{Signer, SigningKey, Verifier, VerifyingKey};
use zeroize::Zeroize;

use crate::msg::{
    Msg, CHANNEL_OPEN, CHANNEL_OPEN_CONFIRMATION, CHANNEL_REQUEST, CHANNEL_SUCCESS, SERVICE_ACCEPT,
    SERVICE_REQUEST, USERAUTH_FAILURE, USERAUTH_REQUEST, USERAUTH_SUCCESS,
};
use crate::wire::{packet_fail, Direction, Pipe};

pub use crate::wire::Fail as Error;

/// Host secret the boot test looks for in the serial log. It is never
/// written there. The public half is what peers verify.
pub const HOST_SECRET: [u8; 32] = [
    0x17, 0x53, 0x48, 0x2d, 0x4b, 0x45, 0x59, 0x21, 0xa7, 0xc3, 0x11, 0x70, 0x9e, 0x44, 0x08, 0xb2,
    0x6d, 0xf1, 0x55, 0x0a, 0x93, 0xe8, 0x27, 0x4c, 0x81, 0x3b, 0xd6, 0x02, 0x7a, 0x19, 0xce, 0x60,
];

/// Ed25519 host key. The secret is wiped when this value is dropped.
pub struct HostKey {
    secret: [u8; 32],
    public: [u8; 32],
}

impl HostKey {
    /// The milestone's fixed host key.
    pub fn shipped() -> Self {
        Self::from_secret(HOST_SECRET)
    }

    /// Builds a host key from 32 secret bytes.
    pub fn from_secret(secret: [u8; 32]) -> Self {
        let signing = SigningKey::from_bytes(&secret);
        let public = signing.verifying_key().to_bytes();
        Self { secret, public }
    }

    fn sign(&self, message: &[u8]) -> [u8; 64] {
        let signing = SigningKey::from_bytes(&self.secret);
        signing.sign(message).to_bytes()
    }

    fn verify(public: &[u8; 32], message: &[u8], sig: &[u8; 64]) -> bool {
        let Ok(key) = VerifyingKey::from_bytes(public) else {
            return false;
        };
        let signature = ed25519_dalek::Signature::from_bytes(sig);
        key.verify(message, &signature).is_ok()
    }
}

impl Drop for HostKey {
    fn drop(&mut self) {
        self.secret.zeroize();
    }
}

/// Overwrites `buf`. Callers use this for password bytes.
pub fn wipe(buf: &mut [u8]) {
    buf.zeroize();
}

/// One SSH session on an already-connected byte pipe.
///
/// The pipe is the TLS plaintext `net` handed over. This code does not
/// parse TCP.
pub struct Session<S> {
    pipe: Pipe<S>,
    user: String,
}

impl<S: Read + Write> Session<S> {
    /// Server handshake. A peer that does not offer the current set is
    /// refused before any user authentication.
    pub fn server(io: S, host: &HostKey) -> Result<Self, Error> {
        let mut pipe = Pipe::new(io);
        pipe.handshake(Direction::Server, Some(host))?;
        Ok(Self {
            pipe,
            user: String::new(),
        })
    }

    /// Client handshake against a server that speaks the same set.
    pub fn client(io: S) -> Result<Self, Error> {
        let mut pipe = Pipe::new(io);
        pipe.handshake(Direction::Client, None)?;
        Ok(Self {
            pipe,
            user: String::new(),
        })
    }

    /// Client handshake that offers only `kex` (a comma-separated list).
    /// Used to prove a refused algorithm gets no session.
    pub fn client_offering(io: S, kex: &str) -> Result<Self, Error> {
        let mut pipe = Pipe::new(io);
        pipe.handshake_offering(Direction::Client, kex)?;
        Ok(Self {
            pipe,
            user: String::new(),
        })
    }

    /// Password authentication. `check` is the existing password hash.
    /// The password copy is wiped before this returns, on every path.
    pub fn accept_password(
        &mut self,
        mut check: impl FnMut(&str, &[u8]) -> bool,
    ) -> Result<(), Error> {
        let service = self.pipe.read_msg()?;
        if service.kind != SERVICE_REQUEST || service.text(0) != Some("ssh-userauth") {
            return packet_fail();
        }
        self.pipe
            .write_msg(&Msg::service(SERVICE_ACCEPT, "ssh-userauth"))?;
        loop {
            let mut req = self.pipe.read_msg()?;
            if req.kind != USERAUTH_REQUEST {
                return packet_fail();
            }
            let user = req.text(0).unwrap_or("").to_string();
            let method = req.text(2).unwrap_or("");
            if method != "password" {
                self.pipe.write_msg(&Msg::auth_failure())?;
                continue;
            }
            let mut password = req.take_password();
            let ok = check(&user, &password);
            password.zeroize();
            req.wipe_password();
            if !ok {
                self.pipe.write_msg(&Msg::auth_failure())?;
                return Err(Error::Auth);
            }
            self.user = user;
            self.pipe.write_msg(&Msg::simple(USERAUTH_SUCCESS))?;
            return Ok(());
        }
    }

    /// Sends one password and waits for success. `password` is wiped here.
    pub fn login(&mut self, user: &str, password: &mut [u8]) -> Result<(), Error> {
        self.pipe
            .write_msg(&Msg::service(SERVICE_REQUEST, "ssh-userauth"))?;
        let accept = self.pipe.read_msg()?;
        if accept.kind != SERVICE_ACCEPT {
            password.zeroize();
            return packet_fail();
        }
        self.pipe
            .write_msg(&Msg::password_request(user, password))?;
        password.zeroize();
        let reply = self.pipe.read_msg()?;
        if reply.kind == USERAUTH_SUCCESS {
            self.user = user.to_string();
            Ok(())
        } else if reply.kind == USERAUTH_FAILURE {
            Err(Error::Auth)
        } else {
            packet_fail()
        }
    }

    /// Authenticated user, when the password check succeeded.
    pub fn user(&self) -> &str {
        &self.user
    }

    /// Serves one SFTP read. `read_file` uses the seat's existing tokens.
    /// Shell, exec, and agent channels are refused.
    pub fn serve_sftp(
        &mut self,
        mut read_file: impl FnMut(&str) -> Option<Vec<u8>>,
    ) -> Result<(), Error> {
        let open = self.pipe.read_msg()?;
        if open.kind != CHANNEL_OPEN || open.text(0) != Some("session") {
            return packet_fail();
        }
        let theirs = open.open_sender().ok_or(Error::Proto)?;
        self.pipe.write_msg(&Msg::channel_confirm(theirs))?;
        let req = self.pipe.read_msg()?;
        if req.kind != CHANNEL_REQUEST || req.subsystem_name() != Some("sftp") {
            return packet_fail();
        }
        self.pipe.write_msg(&Msg::channel_ok(theirs))?;
        sftp_server(&mut self.pipe, theirs, &mut read_file)
    }

    /// Opens an SFTP subsystem and reads one file.
    pub fn sftp_read(&mut self, path: &str) -> Result<Vec<u8>, Error> {
        self.pipe.write_msg(&Msg::channel_open())?;
        let confirm = self.pipe.read_msg()?;
        if confirm.kind != CHANNEL_OPEN_CONFIRMATION {
            return packet_fail();
        }
        let theirs = confirm.confirm_sender().ok_or(Error::Proto)?;
        self.pipe.write_msg(&Msg::subsystem(theirs))?;
        let ok = self.pipe.read_msg()?;
        if ok.kind != CHANNEL_SUCCESS {
            return packet_fail();
        }
        sftp_client(&mut self.pipe, theirs, path)
    }
}

fn sftp_server<S: Read + Write>(
    pipe: &mut Pipe<S>,
    channel: u32,
    read_file: &mut impl FnMut(&str) -> Option<Vec<u8>>,
) -> Result<(), Error> {
    let init = pipe.read_channel(channel)?;
    if init.first().copied() != Some(1) {
        return packet_fail();
    }
    pipe.write_channel(channel, &sftp_version())?;
    let open = pipe.read_channel(channel)?;
    if open.first().copied() != Some(3) {
        return packet_fail();
    }
    let (_id_off, id, name) = sftp_open_name(&open)?;
    let Some(bytes) = read_file(&name) else {
        pipe.write_channel(channel, &sftp_status(id, 2))?;
        return Ok(());
    };
    pipe.write_channel(channel, &sftp_handle(id))?;
    let read = pipe.read_channel(channel)?;
    if read.first().copied() != Some(5) {
        return packet_fail();
    }
    let rid = u32_at(&read, 1).ok_or(Error::Proto)?;
    pipe.write_channel(channel, &sftp_data(rid, &bytes))?;
    Ok(())
}

fn sftp_client<S: Read + Write>(
    pipe: &mut Pipe<S>,
    channel: u32,
    path: &str,
) -> Result<Vec<u8>, Error> {
    let mut init = vec![1];
    init.extend_from_slice(&3u32.to_be_bytes());
    pipe.write_channel(channel, &init)?;
    let version = pipe.read_channel(channel)?;
    if version.first().copied() != Some(2) {
        return packet_fail();
    }
    pipe.write_channel(channel, &sftp_open(1, path))?;
    let handle = pipe.read_channel(channel)?;
    if handle.first().copied() != Some(102) {
        return packet_fail();
    }
    pipe.write_channel(channel, &sftp_read_req(2))?;
    let data = pipe.read_channel(channel)?;
    if data.first().copied() != Some(103) {
        return packet_fail();
    }
    let n = u32_at(&data, 5).ok_or(Error::Proto)? as usize;
    let start = 9;
    if data.len() < start + n {
        return packet_fail();
    }
    Ok(data[start..start + n].to_vec())
}

fn sftp_version() -> Vec<u8> {
    let mut out = vec![2];
    out.extend_from_slice(&3u32.to_be_bytes());
    out
}

fn sftp_open(id: u32, path: &str) -> Vec<u8> {
    let mut out = vec![3];
    out.extend_from_slice(&id.to_be_bytes());
    push_str(&mut out, path.as_bytes());
    out.extend_from_slice(&1u32.to_be_bytes());
    out.extend_from_slice(&0u32.to_be_bytes());
    out
}

fn sftp_open_name(body: &[u8]) -> Result<(usize, u32, String), Error> {
    let id = u32_at(body, 1).ok_or(Error::Proto)?;
    let (name, next) = ssh_string(body, 5)?;
    let name = String::from_utf8(name).map_err(|_| Error::Proto)?;
    Ok((next, id, name))
}

fn sftp_handle(id: u32) -> Vec<u8> {
    let mut out = vec![102];
    out.extend_from_slice(&id.to_be_bytes());
    push_str(&mut out, b"1");
    out
}

fn sftp_read_req(id: u32) -> Vec<u8> {
    let mut out = vec![5];
    out.extend_from_slice(&id.to_be_bytes());
    push_str(&mut out, b"1");
    out.extend_from_slice(&0u64.to_be_bytes());
    out.extend_from_slice(&4096u32.to_be_bytes());
    out
}

fn sftp_data(id: u32, bytes: &[u8]) -> Vec<u8> {
    let mut out = vec![103];
    out.extend_from_slice(&id.to_be_bytes());
    push_str(&mut out, bytes);
    out
}

fn sftp_status(id: u32, code: u32) -> Vec<u8> {
    let mut out = vec![101];
    out.extend_from_slice(&id.to_be_bytes());
    out.extend_from_slice(&code.to_be_bytes());
    push_str(&mut out, b"");
    push_str(&mut out, b"");
    out
}

fn push_str(out: &mut Vec<u8>, bytes: &[u8]) {
    out.extend_from_slice(&(bytes.len() as u32).to_be_bytes());
    out.extend_from_slice(bytes);
}

fn u32_at(body: &[u8], at: usize) -> Option<u32> {
    let bytes: [u8; 4] = body.get(at..at + 4)?.try_into().ok()?;
    Some(u32::from_be_bytes(bytes))
}

fn ssh_string(body: &[u8], at: usize) -> Result<(Vec<u8>, usize), Error> {
    let n = u32_at(body, at).ok_or(Error::Proto)? as usize;
    let start = at + 4;
    let end = start + n;
    if body.len() < end {
        return Err(Error::Proto);
    }
    Ok((body[start..end].to_vec(), end))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixStream;

    fn pair() -> (UnixStream, UnixStream) {
        UnixStream::pair().expect("socket pair")
    }

    #[test]
    fn client_reads_the_file_the_server_holds() {
        let (client_io, server_io) = pair();
        let host = HostKey::shipped();
        let server = std::thread::spawn(move || {
            let mut session = Session::server(server_io, &host).unwrap();
            session
                .accept_password(|user, pass| user == "ada" && pass == b"secret")
                .unwrap();
            session
                .serve_sftp(|path| {
                    if path == "note" {
                        Some(b"hi\n".to_vec())
                    } else {
                        None
                    }
                })
                .unwrap();
        });
        let mut session = Session::client(client_io).unwrap();
        let mut password = b"secret".to_vec();
        session.login("ada", &mut password).unwrap();
        assert!(password.iter().all(|b| *b == 0));
        let body = session.sftp_read("note").unwrap();
        assert_eq!(body, b"hi\n");
        server.join().unwrap();
    }

    #[test]
    fn a_refused_group_gets_no_session() {
        let (client_io, server_io) = pair();
        let host = HostKey::shipped();
        let server = std::thread::spawn(move || Session::server(server_io, &host).map(|_| ()));
        let err = Session::client_offering(client_io, "diffie-hellman-group14-sha1");
        assert!(err.is_err());
        let _ = server.join().unwrap();
    }

    #[test]
    fn a_wrong_password_is_not_a_session() {
        let (client_io, server_io) = pair();
        let host = HostKey::shipped();
        let server = std::thread::spawn(move || {
            let mut session = Session::server(server_io, &host).unwrap();
            session.accept_password(|_, _| false).unwrap_err();
        });
        let mut session = Session::client(client_io).unwrap();
        let mut password = b"nope".to_vec();
        assert!(session.login("ada", &mut password).is_err());
        assert!(password.iter().all(|b| *b == 0));
        let _ = server.join();
    }
}
