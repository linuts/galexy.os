//! SSH binary packets. Cleartext until NEWKEYS, then ChaCha20-Poly1305.
//!
//! The construction is the OpenSSH one: a header key encrypts the
//! 4-byte length, a main key encrypts the rest starting at block 1,
//! and Poly1305 covers both. The sequence number is the nonce.

use std::io::{Read, Write};

use chacha20::cipher::{KeyInit, KeyIvInit, StreamCipher, StreamCipherSeek};
use chacha20::ChaCha20Legacy;
use poly1305::Poly1305;
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;
use x25519_dalek::{PublicKey, StaticSecret};
use zeroize::Zeroize;

use crate::msg::{
    choose, name_has, push_str, Msg, CIPHER, CLIENT_KEX, CURVE, HOSTKEY, KEXINIT, KEX_ECDH_INIT,
    KEX_ECDH_REPLY, NEWKEYS, SERVER_KEX, STRICT_CLIENT, STRICT_SERVER,
};
use crate::HostKey;

/// Why a session stopped.
#[derive(Debug, PartialEq, Eq)]
pub enum Error {
    /// The peer's algorithms and ours do not meet.
    Algo,
    /// A password was refused.
    Auth,
    /// The bytes were not this protocol.
    Proto,
    /// The pipe closed.
    Io,
}

pub use Error as Fail;

pub fn packet_fail<T>() -> Result<T, Error> {
    Err(Error::Proto)
}

#[derive(Clone, Copy)]
pub enum Direction {
    Client,
    Server,
}

struct Keys {
    main: [u8; 32],
    header: [u8; 32],
}

impl Drop for Keys {
    fn drop(&mut self) {
        self.main.zeroize();
        self.header.zeroize();
    }
}

pub struct Pipe<S> {
    io: S,
    incoming: Vec<u8>,
    channel_buf: Vec<u8>,
    send_seq: u32,
    recv_seq: u32,
    send_key: Option<Keys>,
    recv_key: Option<Keys>,
    session_id: [u8; 32],
    strict: bool,
}

impl<S: Read + Write> Pipe<S> {
    pub fn new(io: S) -> Self {
        Self {
            io,
            incoming: Vec::new(),
            channel_buf: Vec::new(),
            send_seq: 0,
            recv_seq: 0,
            send_key: None,
            recv_key: None,
            session_id: [0; 32],
            strict: true,
        }
    }

    pub fn handshake(&mut self, role: Direction, host: Option<&HostKey>) -> Result<(), Error> {
        let kex = match role {
            Direction::Client => CLIENT_KEX,
            Direction::Server => SERVER_KEX,
        };
        self.handshake_with(role, kex, host)
    }

    pub fn handshake_offering(&mut self, role: Direction, kex: &str) -> Result<(), Error> {
        self.handshake_with(role, kex, None)
    }

    fn handshake_with(
        &mut self,
        role: Direction,
        kex_list: &str,
        host: Option<&HostKey>,
    ) -> Result<(), Error> {
        self.write_version()?;
        let peer_version = self.read_version()?;
        if !peer_version.starts_with("SSH-2.0-") {
            return Err(Error::Algo);
        }
        let mut cookie = [0u8; 16];
        fill(&mut cookie)?;
        let mine = Msg::kexinit(&cookie, kex_list);
        self.write_payload(&mine.body)?;
        let theirs = self.read_payload()?;
        if theirs.first().copied() != Some(KEXINIT) {
            return Err(Error::Proto);
        }
        let peer = Msg::from_payload(theirs.clone()).ok_or(Error::Proto)?;
        self.accept_kex(&peer, role)?;
        let (client_version, server_version, client_init, server_init) = match role {
            Direction::Client => (
                version_token(b"SSH-2.0-galexy"),
                version_token(peer_version.as_bytes()),
                mine.body.clone(),
                theirs,
            ),
            Direction::Server => (
                version_token(peer_version.as_bytes()),
                version_token(b"SSH-2.0-galexy"),
                theirs,
                mine.body.clone(),
            ),
        };
        match role {
            Direction::Client => {
                self.client_kex(&client_version, &server_version, &client_init, &server_init)?
            }
            Direction::Server => {
                let host = host.ok_or(Error::Proto)?;
                self.server_kex(
                    host,
                    &client_version,
                    &server_version,
                    &client_init,
                    &server_init,
                )?
            }
        }
        Ok(())
    }

    fn accept_kex(&mut self, peer: &Msg, role: Direction) -> Result<(), Error> {
        let kex = peer.kex_list(0).ok_or(Error::Proto)?;
        let host = peer.kex_list(1).ok_or(Error::Proto)?;
        let cipher_c = peer.kex_list(2).ok_or(Error::Proto)?;
        let cipher_s = peer.kex_list(3).ok_or(Error::Proto)?;
        let strict = match role {
            Direction::Client => STRICT_SERVER,
            Direction::Server => STRICT_CLIENT,
        };
        if !name_has(kex, strict) || choose(CURVE, kex).as_deref() != Some(CURVE) {
            let _ = self.write_payload(&Msg::disconnect("kex").body);
            return Err(Error::Algo);
        }
        if choose(HOSTKEY, host).as_deref() != Some(HOSTKEY)
            || choose(CIPHER, cipher_c).as_deref() != Some(CIPHER)
            || choose(CIPHER, cipher_s).as_deref() != Some(CIPHER)
        {
            let _ = self.write_payload(&Msg::disconnect("kex").body);
            return Err(Error::Algo);
        }
        self.strict = true;
        Ok(())
    }

    fn client_kex(
        &mut self,
        client_version: &[u8],
        server_version: &[u8],
        client_init: &[u8],
        server_init: &[u8],
    ) -> Result<(), Error> {
        let (secret, public) = ephemeral()?;
        self.write_payload(&Msg::ecdh_init(&public).body)?;
        let reply = self.read_payload()?;
        if reply.first().copied() != Some(KEX_ECDH_REPLY) {
            return Err(Error::Proto);
        }
        let msg = Msg::from_payload(reply).ok_or(Error::Proto)?;
        let host_blob = msg.raw_field(0).ok_or(Error::Proto)?.to_vec();
        let server_public = msg.raw_field(1).ok_or(Error::Proto)?;
        if server_public.len() != 32 {
            return Err(Error::Proto);
        }
        let mut server_q = [0u8; 32];
        server_q.copy_from_slice(server_public);
        let signature = msg.raw_field(2).ok_or(Error::Proto)?.to_vec();
        let shared = shared_secret(&secret, &server_q);
        let hash = exchange_hash(
            client_version,
            server_version,
            client_init,
            server_init,
            &host_blob,
            &public,
            &server_q,
            &shared,
        );
        if !verify_host(&host_blob, &signature, &hash) {
            return Err(Error::Proto);
        }
        self.finish_newkeys(Direction::Client, &shared, &hash)
    }

    fn server_kex(
        &mut self,
        host: &HostKey,
        client_version: &[u8],
        server_version: &[u8],
        client_init: &[u8],
        server_init: &[u8],
    ) -> Result<(), Error> {
        let init = self.read_payload()?;
        if init.first().copied() != Some(KEX_ECDH_INIT) {
            return Err(Error::Proto);
        }
        let msg = Msg::from_payload(init).ok_or(Error::Proto)?;
        let client_public = msg.raw_field(0).ok_or(Error::Proto)?;
        if client_public.len() != 32 {
            return Err(Error::Proto);
        }
        let mut client_q = [0u8; 32];
        client_q.copy_from_slice(client_public);
        let (secret, public) = ephemeral()?;
        let shared = shared_secret(&secret, &client_q);
        let host_blob = host_blob(&host.public);
        let hash = exchange_hash(
            client_version,
            server_version,
            client_init,
            server_init,
            &host_blob,
            &client_q,
            &public,
            &shared,
        );
        let signature = sign_host(host, &hash);
        self.write_payload(&Msg::ecdh_reply(&host_blob, &public, &signature).body)?;
        self.finish_newkeys(Direction::Server, &shared, &hash)
    }

    fn finish_newkeys(
        &mut self,
        role: Direction,
        shared: &Shared,
        hash: &[u8; 32],
    ) -> Result<(), Error> {
        let k_mpint = mpint(&shared.bytes);
        let mut c_to_s = derive(&k_mpint, hash, b'C', hash);
        let mut s_to_c = derive(&k_mpint, hash, b'D', hash);
        let (send, recv) = match role {
            Direction::Client => (split_keys(&c_to_s), split_keys(&s_to_c)),
            Direction::Server => (split_keys(&s_to_c), split_keys(&c_to_s)),
        };
        c_to_s.zeroize();
        s_to_c.zeroize();
        self.session_id = *hash;
        self.write_payload(&[NEWKEYS])?;
        self.send_key = Some(send);
        if self.strict {
            self.send_seq = 0;
        }
        let theirs = self.read_payload()?;
        if theirs.first().copied() != Some(NEWKEYS) {
            return Err(Error::Proto);
        }
        self.recv_key = Some(recv);
        if self.strict {
            self.recv_seq = 0;
        }
        Ok(())
    }

    pub fn read_msg(&mut self) -> Result<Msg, Error> {
        let payload = self.read_payload()?;
        Msg::from_payload(payload).ok_or(Error::Proto)
    }

    pub fn write_msg(&mut self, msg: &Msg) -> Result<(), Error> {
        self.write_payload(&msg.body)
    }

    pub fn write_channel(&mut self, theirs: u32, data: &[u8]) -> Result<(), Error> {
        let mut framed = (data.len() as u32).to_be_bytes().to_vec();
        framed.extend_from_slice(data);
        self.write_msg(&Msg::channel_data(theirs, &framed))
    }

    pub fn read_channel(&mut self, ours: u32) -> Result<Vec<u8>, Error> {
        loop {
            if let Some(msg) = pop_sftp(&mut self.channel_buf) {
                return Ok(msg);
            }
            let packet = self.read_msg()?;
            if packet.kind != crate::msg::CHANNEL_DATA {
                continue;
            }
            let (recipient, data) = packet.data_bytes().ok_or(Error::Proto)?;
            if recipient != ours {
                return Err(Error::Proto);
            }
            self.channel_buf.extend_from_slice(data);
        }
    }

    fn write_version(&mut self) -> Result<(), Error> {
        self.io
            .write_all(b"SSH-2.0-galexy\r\n")
            .map_err(|_| Error::Io)
    }

    fn read_version(&mut self) -> Result<String, Error> {
        let mut line = Vec::new();
        loop {
            let mut byte = [0u8; 1];
            self.io.read_exact(&mut byte).map_err(|_| Error::Io)?;
            if byte[0] == b'\n' {
                break;
            }
            if line.len() > 255 {
                return Err(Error::Proto);
            }
            line.push(byte[0]);
        }
        if line.last() == Some(&b'\r') {
            line.pop();
        }
        String::from_utf8(line).map_err(|_| Error::Proto)
    }

    fn write_payload(&mut self, payload: &[u8]) -> Result<(), Error> {
        let packet = seal(payload, self.send_seq, self.send_key.as_ref())?;
        self.send_seq = self.send_seq.wrapping_add(1);
        self.io.write_all(&packet).map_err(|_| Error::Io)
    }

    fn read_payload(&mut self) -> Result<Vec<u8>, Error> {
        let (payload, used) = open_packet(&self.incoming, self.recv_seq, self.recv_key.as_ref())?;
        if used == 0 {
            let mut buf = [0u8; 1024];
            let n = self.io.read(&mut buf).map_err(|_| Error::Io)?;
            if n == 0 {
                return Err(Error::Io);
            }
            self.incoming.extend_from_slice(&buf[..n]);
            return self.read_payload();
        }
        self.incoming.drain(..used);
        self.recv_seq = self.recv_seq.wrapping_add(1);
        Ok(payload)
    }
}

fn pop_sftp(buf: &mut Vec<u8>) -> Option<Vec<u8>> {
    if buf.len() < 4 {
        return None;
    }
    let n = u32::from_be_bytes(buf[..4].try_into().ok()?) as usize;
    if buf.len() < 4 + n {
        return None;
    }
    let msg = buf[4..4 + n].to_vec();
    buf.drain(..4 + n);
    Some(msg)
}

fn version_token(line: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    push_str(&mut out, line);
    out
}

struct Shared {
    bytes: [u8; 32],
}

impl Drop for Shared {
    fn drop(&mut self) {
        self.bytes.zeroize();
    }
}

fn ephemeral() -> Result<(StaticSecret, [u8; 32]), Error> {
    let mut raw = [0u8; 32];
    fill(&mut raw)?;
    let secret = StaticSecret::from(raw);
    raw.zeroize();
    let public = PublicKey::from(&secret);
    Ok((secret, *public.as_bytes()))
}

fn shared_secret(secret: &StaticSecret, peer: &[u8; 32]) -> Shared {
    let shared = secret.diffie_hellman(&PublicKey::from(*peer));
    Shared {
        bytes: *shared.as_bytes(),
    }
}

fn host_blob(public: &[u8; 32]) -> Vec<u8> {
    let mut out = Vec::new();
    push_str(&mut out, b"ssh-ed25519");
    push_str(&mut out, public);
    out
}

fn sign_host(host: &HostKey, hash: &[u8; 32]) -> Vec<u8> {
    let sig = host.sign(hash);
    let mut inner = Vec::new();
    push_str(&mut inner, b"ssh-ed25519");
    push_str(&mut inner, &sig);
    inner
}

fn verify_host(host_blob: &[u8], signature_blob: &[u8], hash: &[u8; 32]) -> bool {
    let Ok(msg) = parse_blob(host_blob) else {
        return false;
    };
    if msg.0 != "ssh-ed25519" || msg.1.len() != 32 {
        return false;
    }
    let mut public = [0u8; 32];
    public.copy_from_slice(&msg.1);
    let Ok(sig) = parse_blob(signature_blob) else {
        return false;
    };
    if sig.0 != "ssh-ed25519" || sig.1.len() != 64 {
        return false;
    }
    let mut raw = [0u8; 64];
    raw.copy_from_slice(&sig.1);
    HostKey::verify(&public, hash, &raw)
}

fn parse_blob(bytes: &[u8]) -> Result<(&str, Vec<u8>), ()> {
    if bytes.len() < 4 {
        return Err(());
    }
    let n = u32::from_be_bytes(bytes[..4].try_into().map_err(|_| ())?) as usize;
    if bytes.len() < 4 + n {
        return Err(());
    }
    let name = std::str::from_utf8(&bytes[4..4 + n]).map_err(|_| ())?;
    let rest = &bytes[4 + n..];
    if rest.len() < 4 {
        return Err(());
    }
    let m = u32::from_be_bytes(rest[..4].try_into().map_err(|_| ())?) as usize;
    if rest.len() < 4 + m {
        return Err(());
    }
    Ok((name, rest[4..4 + m].to_vec()))
}

fn exchange_hash(
    client_version: &[u8],
    server_version: &[u8],
    client_init: &[u8],
    server_init: &[u8],
    host_key: &[u8],
    client_q: &[u8; 32],
    server_q: &[u8; 32],
    shared: &Shared,
) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(client_version);
    hasher.update(server_version);
    hasher.update(ssh_string(client_init));
    hasher.update(ssh_string(server_init));
    hasher.update(ssh_string(host_key));
    hasher.update(ssh_string(client_q));
    hasher.update(ssh_string(server_q));
    hasher.update(mpint(&shared.bytes));
    let out = hasher.finalize();
    let mut hash = [0u8; 32];
    hash.copy_from_slice(&out);
    hash
}

fn ssh_string(bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    push_str(&mut out, bytes);
    out
}

fn mpint(bytes: &[u8; 32]) -> Vec<u8> {
    let mut body = Vec::new();
    if bytes[0] & 0x80 != 0 {
        body.push(0);
    }
    body.extend_from_slice(bytes);
    ssh_string(&body)
}

fn derive(k: &[u8], h: &[u8; 32], letter: u8, session: &[u8; 32]) -> Vec<u8> {
    let mut hasher = Sha256::new();
    hasher.update(k);
    hasher.update(h);
    hasher.update([letter]);
    hasher.update(session);
    let first = hasher.finalize();
    let mut second = Sha256::new();
    second.update(k);
    second.update(h);
    second.update(&first);
    let more = second.finalize();
    let mut out = Vec::with_capacity(64);
    out.extend_from_slice(&first);
    out.extend_from_slice(&more);
    out
}

fn split_keys(bytes: &[u8]) -> Keys {
    let mut main = [0u8; 32];
    let mut header = [0u8; 32];
    main.copy_from_slice(&bytes[..32]);
    header.copy_from_slice(&bytes[32..64]);
    Keys { main, header }
}

fn seal(payload: &[u8], seq: u32, keys: Option<&Keys>) -> Result<Vec<u8>, Error> {
    let pad = padding_len(payload.len());
    let packet_len = 1 + payload.len() + pad;
    let mut plain = Vec::with_capacity(4 + packet_len);
    plain.extend_from_slice(&(packet_len as u32).to_be_bytes());
    plain.push(pad as u8);
    plain.extend_from_slice(payload);
    plain.extend(std::iter::repeat(0).take(pad));
    if let Some(keys) = keys {
        encrypt(&mut plain, seq, keys)?;
    }
    Ok(plain)
}

fn padding_len(payload: usize) -> usize {
    let block = 8usize;
    let mut pad = block - ((4 + 1 + payload) % block);
    if pad < 4 {
        pad += block;
    }
    pad
}

fn encrypt(packet: &mut Vec<u8>, seq: u32, keys: &Keys) -> Result<(), Error> {
    let mut header = chacha(&keys.header, seq);
    let mut poly_key = [0u8; 32];
    header.apply_keystream(&mut poly_key);
    let mut length = [0u8; 4];
    length.copy_from_slice(&packet[..4]);
    let mut length_stream = chacha(&keys.header, seq);
    length_stream.apply_keystream(&mut length);
    packet[..4].copy_from_slice(&length);
    let mut body = chacha(&keys.main, seq);
    body.seek(64);
    body.apply_keystream(&mut packet[4..]);
    let tag = poly(&poly_key, packet);
    packet.extend_from_slice(&tag);
    poly_key.zeroize();
    Ok(())
}

/// Returns the payload and how many bytes of `buf` it consumed.
/// `used == 0` means `buf` does not hold a whole packet yet.
fn open_packet(buf: &[u8], seq: u32, keys: Option<&Keys>) -> Result<(Vec<u8>, usize), Error> {
    if buf.len() < 4 {
        return Ok((Vec::new(), 0));
    }
    let mut length_bytes = [0u8; 4];
    length_bytes.copy_from_slice(&buf[..4]);
    if let Some(keys) = keys {
        if buf.len() < 4 + 16 {
            return Ok((Vec::new(), 0));
        }
        let mut header = chacha(&keys.header, seq);
        let mut poly_key = [0u8; 32];
        header.apply_keystream(&mut poly_key);
        let mut length_stream = chacha(&keys.header, seq);
        length_stream.apply_keystream(&mut length_bytes);
        let packet_len = u32::from_be_bytes(length_bytes) as usize;
        if packet_len > 35000 || packet_len < 8 {
            return Err(Error::Proto);
        }
        let total = 4 + packet_len + 16;
        if buf.len() < total {
            return Ok((Vec::new(), 0));
        }
        let tag = poly(&poly_key, &buf[..4 + packet_len]);
        let got = &buf[4 + packet_len..total];
        if tag.ct_eq(got).unwrap_u8() != 1 {
            return Err(Error::Proto);
        }
        let mut plain = buf[4..4 + packet_len].to_vec();
        let mut body = chacha(&keys.main, seq);
        body.seek(64);
        body.apply_keystream(&mut plain);
        poly_key.zeroize();
        let payload = unpad(&plain)?;
        return Ok((payload, total));
    }
    let packet_len = u32::from_be_bytes(length_bytes) as usize;
    if packet_len > 35000 || packet_len < 8 {
        return Err(Error::Proto);
    }
    let total = 4 + packet_len;
    if buf.len() < total {
        return Ok((Vec::new(), 0));
    }
    let payload = unpad(&buf[4..total])?;
    Ok((payload, total))
}

fn unpad(plain: &[u8]) -> Result<Vec<u8>, Error> {
    let pad = *plain.first().ok_or(Error::Proto)? as usize;
    if pad < 4 || pad >= plain.len() {
        return Err(Error::Proto);
    }
    Ok(plain[1..plain.len() - pad].to_vec())
}

fn chacha(key: &[u8; 32], seq: u32) -> ChaCha20Legacy {
    let mut nonce = [0u8; 8];
    nonce[..4].copy_from_slice(&seq.to_le_bytes());
    ChaCha20Legacy::new(key.into(), &nonce.into())
}

fn poly(key: &[u8; 32], data: &[u8]) -> [u8; 16] {
    let mac = Poly1305::new(key.into());
    let tag = mac.compute_unpadded(data);
    let mut out = [0u8; 16];
    out.copy_from_slice(tag.as_slice());
    out
}

fn fill(buf: &mut [u8]) -> Result<(), Error> {
    getrandom::fill(buf).map_err(|_| Error::Io)
}
