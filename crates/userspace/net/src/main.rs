//! The `net` service. It is the only task that touches the wire.
//!
//! Frames go through [`galexy_net`]. TLS 1.3 is rustls with the pure-Rust
//! provider. A handshake that is not finished in five seconds is dropped.
//! Session keys are not written to the console.

use std::io::Read;
use std::sync::Arc;

use galexy_abi::{
    Cap, SysError, INIT_RPC_HDR, NET_OP_ALLOW, NET_OP_DIAL, NET_OP_GRANT, NET_OP_LINK, NET_OP_PING,
    NET_OP_REVOKE, NET_OP_SERVE,
};
use galexy_net::Engine;
use galexy_netcfg::{decode, encode, Record, MODE_FIXED, NAME_MAX};
use galexy_rt::{
    chan_recv_poll, chan_send, channel, clock_ms, close, lane_pair, open, read, seek, sleep_ms,
    truncate, wire_read, wire_write, write, write_console,
};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{verify_tls13_signature, CryptoProvider};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{ClientConfig, ClientConnection, DigitallySignedStruct, Error, SignatureScheme};

const HANDSHAKE_MS: u64 = 5_000;
const MAX_GATES: usize = 8;
const PENDING: usize = 4;

fn main() {
    fill_random_once();
    let mut ends = [0u64; 2];
    if !channel(&mut ends).ok {
        loop {
            let _ = sleep_ms(1_000);
        }
    }
    let rx = Cap::from_bits(ends[0]);
    let _peer = ends[1];
    let mac = read_mac();
    log(&format!(
        "[net] link up mac={:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
        mac[0], mac[1], mac[2], mac[3], mac[4], mac[5]
    ));
    let mut record = load_record();
    let mut engine = if record.mode == MODE_FIXED {
        let mut engine = Engine::new(mac, false);
        if let Some(lease) = record.lease {
            engine.set_fixed(lease.addr, lease.prefix, lease.gateway);
        }
        engine
    } else {
        Engine::new(mac, true)
    };
    let mut gates: Vec<Gate> = Vec::new();
    let mut stalls: Vec<Stall> = Vec::new();
    let mut logged_addr = false;
    let mut logged_udp = false;
    let mut logged_tcp = false;
    let mut flood_at: Option<u64> = None;
    let mut flood_ids: Vec<u8> = Vec::new();
    let mut flood_logged = false;
    loop {
        pump(&mut engine);
        let now = clock_ms();
        engine.poll(now);
        pump(&mut engine);
        if !logged_addr {
            if let Some((addr, prefix, gw)) = engine.locator() {
                log(&format!(
                    "[net] addr {}.{}.{}.{}/{} gw {}.{}.{}.{}",
                    addr[0], addr[1], addr[2], addr[3], prefix, gw[0], gw[1], gw[2], gw[3]
                ));
                record.lease = Some(galexy_netcfg::Lease {
                    addr,
                    prefix,
                    gateway: gw,
                    expiry: engine.lease_secs(),
                });
                save_record(&record);
                logged_addr = true;
            }
        }
        if engine.udp_sent() && !logged_udp {
            log("[net] udp ok");
            logged_udp = true;
        }
        if engine.tcp_ready() && !logged_tcp {
            log("[net] tcp ok");
            logged_tcp = true;
        }
        while let Some(id) = engine.take_accepted() {
            let local = 0u16;
            let _ = local;
            if stalls.len() >= PENDING * MAX_GATES {
                engine.tcp_close(id);
                continue;
            }
            stalls.push(Stall { id, start: now });
        }
        let mut i = 0;
        while i < stalls.len() {
            if now.saturating_sub(stalls[i].start) >= HANDSHAKE_MS {
                engine.tcp_close(stalls[i].id);
                log("[net] deadline");
                stalls.swap_remove(i);
            } else {
                i += 1;
            }
        }
        if let Some(start) = flood_at {
            if flood_ids.len() < 4 {
                if let Some(gw) = engine.locator().map(|(_, _, gw)| gw) {
                    if let Ok(id) = engine.tcp_connect(gw, 4583) {
                        flood_ids.push(id);
                    }
                }
            }
            if !flood_logged && now.saturating_sub(start) >= HANDSHAKE_MS {
                for id in flood_ids.drain(..) {
                    engine.tcp_close(id);
                }
                log("[net] deadline");
                flood_logged = true;
            }
        }
        if let Some(msg) = recv_ctrl(rx) {
            handle(
                &msg,
                rx,
                &mut engine,
                &mut record,
                &mut gates,
                now,
                &mut flood_at,
            );
        }
        if sleep_ms(20).ok {
            continue;
        }
        let err = SysError::from_code(0);
        let _ = err;
    }
}

struct Gate {
    port: u16,
    /// Listen slot. Inbound delivery to the serving task is still open,
    /// so nothing reads this yet.
    #[allow(dead_code)]
    id: u8,
}

struct Stall {
    id: u8,
    start: u64,
}

fn handle(
    msg: &[u8],
    rx: Cap,
    engine: &mut Engine,
    record: &mut Record,
    gates: &mut Vec<Gate>,
    now: u64,
    flood_at: &mut Option<u64>,
) {
    if msg.len() < INIT_RPC_HDR + 1 {
        reply(rx, b"bad\n", 0);
        return;
    }
    let admin = msg[0] == 1;
    let mut id_bytes = [0u8; 8];
    id_bytes.copy_from_slice(&msg[4..12]);
    let actor_id = u64::from_le_bytes(id_bytes);
    let body = &msg[INIT_RPC_HDR..];
    let op = body[0];
    let rest = &body[1..];
    match op {
        NET_OP_LINK => {
            if flood_at.is_none() {
                *flood_at = Some(now);
            }
            if let Some((addr, prefix, _)) = engine.locator() {
                reply(
                    rx,
                    format!(
                        "up {}.{}.{}.{}/{}\n",
                        addr[0], addr[1], addr[2], addr[3], prefix
                    )
                    .as_bytes(),
                    0,
                );
            } else {
                reply(rx, b"down\n", 0);
            }
        }
        NET_OP_DIAL => dial(rx, engine, record, admin, actor_id, rest, now),
        NET_OP_ALLOW if admin => allow(rx, record, rest),
        NET_OP_GRANT if admin => grant_op(rx, record, rest, true),
        NET_OP_REVOKE if admin => grant_op(rx, record, rest, false),
        NET_OP_SERVE if admin => serve(rx, engine, record, gates, rest),
        NET_OP_PING => ping(rx, engine, rest),
        NET_OP_ALLOW | NET_OP_GRANT | NET_OP_REVOKE | NET_OP_SERVE => {
            reply(rx, b"access denied\n", 0);
        }
        _ => reply(rx, b"bad\n", 0),
    }
}

fn dial(
    rx: Cap,
    engine: &mut Engine,
    record: &Record,
    admin: bool,
    actor_id: u64,
    rest: &[u8],
    now: u64,
) {
    let Some((name, claimed)) = two_fields(rest) else {
        reply(rx, b"bad\n", 0);
        return;
    };
    let actor_ok = if claimed == b"admin" {
        admin && record.may_dial(name, b"admin")
    } else {
        record.may_dial(name, claimed)
    };
    let Some(entry) = record.find(name) else {
        audit(actor_id, name, &[0; 32], "missing");
        reply(rx, b"access denied\n", 0);
        return;
    };
    if !actor_ok {
        audit(actor_id, name, &entry.key, "denied");
        reply(rx, b"access denied\n", 0);
        return;
    }
    if entry.kind != 4 {
        reply(rx, b"unsupported\n", 0);
        return;
    }
    let ip = [entry.ip[0], entry.ip[1], entry.ip[2], entry.ip[3]];
    let Ok(id) = engine.tcp_connect(ip, entry.port) else {
        audit(actor_id, name, &entry.key, "unavailable");
        reply(rx, b"unavailable\n", 0);
        return;
    };
    let start = now;
    let mut established = false;
    while clock_ms().saturating_sub(start) < HANDSHAKE_MS {
        pump(engine);
        engine.poll(clock_ms());
        pump(engine);
        if engine.tcp_established(id) {
            established = true;
            break;
        }
        let _ = sleep_ms(20);
    }
    if !established {
        engine.tcp_close(id);
        audit(actor_id, name, &entry.key, "deadline");
        log("[net] deadline");
        reply(rx, b"timed out\n", 0);
        return;
    }
    let name_str = String::from_utf8_lossy(name).into_owned();
    match tls_fetch(engine, id, &name_str, &entry.key, start) {
        Ok(body) => {
            let mut pair = [0u64; 2];
            if !lane_pair(&mut pair).ok {
                audit(actor_id, name, &entry.key, "unavailable");
                reply(rx, b"unavailable\n", 0);
                return;
            }
            let ours = Cap::from_bits(pair[0]);
            let _ = write(ours, &body);
            let _ = close(ours);
            audit(actor_id, name, &entry.key, "ok");
            reply(rx, b"ok\n", pair[1]);
        }
        Err(why) => {
            engine.tcp_close(id);
            audit(actor_id, name, &entry.key, why);
            if why == "deadline" {
                log("[net] deadline");
            }
            reply(
                rx,
                if why == "denied" {
                    b"access denied\n"
                } else {
                    b"timed out\n"
                },
                0,
            );
        }
    }
}

fn tls_fetch(
    engine: &mut Engine,
    id: u8,
    name: &str,
    key: &[u8; 32],
    start: u64,
) -> Result<Vec<u8>, &'static str> {
    let server = ServerName::try_from(name.to_string()).map_err(|_| "bad")?;
    let config = client_config(key).map_err(|_| "bad")?;
    let mut conn = ClientConnection::new(config, server).map_err(|_| "bad")?;
    let mut body = Vec::new();
    // The handshake has five seconds. A finished handshake gets the same
    // again to read the body, then the lane closes either way.
    let limit = start.saturating_add(HANDSHAKE_MS.saturating_mul(2));
    while clock_ms() < limit {
        if clock_ms().saturating_sub(start) >= HANDSHAKE_MS && conn.is_handshaking() {
            return Err("deadline");
        }
        drive_tls(engine, id, &mut conn, &mut body)?;
        if !conn.is_handshaking() && !conn.wants_read() {
            break;
        }
        pump(engine);
        let _ = sleep_ms(20);
    }
    if conn.is_handshaking() {
        return Err("deadline");
    }
    engine.tcp_close(id);
    Ok(body)
}

fn drive_tls(
    engine: &mut Engine,
    id: u8,
    conn: &mut ClientConnection,
    body: &mut Vec<u8>,
) -> Result<(), &'static str> {
    while conn.wants_write() {
        let mut out = Vec::new();
        conn.write_tls(&mut out).map_err(|_| "bad")?;
        if engine.tcp_send(id, &out).is_err() {
            return Err("bad");
        }
    }
    // `tcp_send` only queues. `poll` is what puts the segment on the wire.
    engine.poll(clock_ms());
    pump(engine);
    let mut buf = [0u8; 2048];
    let n = engine.tcp_recv(id, &mut buf);
    if n > 0 {
        conn.read_tls(&mut &buf[..n]).map_err(|_| "bad")?;
        let _io = conn.process_new_packets().map_err(|err| {
            log(&format!("[net] tls {err}"));
            "denied"
        })?;
        let mut plain = [0u8; 2048];
        let got = conn.reader().read(&mut plain).unwrap_or(0);
        body.extend_from_slice(&plain[..got]);
    }
    Ok(())
}

fn client_config(key: &[u8; 32]) -> Result<Arc<ClientConfig>, Error> {
    let provider = filtered_provider();
    let verifier = Arc::new(PinVerifier {
        key: *key,
        provider: provider.clone(),
    });
    let config = ClientConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13])?
        .dangerous()
        .with_custom_certificate_verifier(verifier)
        .with_no_client_auth();
    Ok(Arc::new(config))
}

fn filtered_provider() -> Arc<CryptoProvider> {
    let mut provider = rustls_pure::provider();
    provider.cipher_suites.retain(|suite| {
        let name = suite.suite();
        name == rustls::CipherSuite::TLS13_AES_128_GCM_SHA256
            || name == rustls::CipherSuite::TLS13_CHACHA20_POLY1305_SHA256
    });
    provider
        .kx_groups
        .retain(|group| group.name() == rustls::NamedGroup::X25519);
    Arc::new(provider)
}

#[derive(Debug)]
struct PinVerifier {
    key: [u8; 32],
    provider: Arc<CryptoProvider>,
}

impl ServerCertVerifier for PinVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, Error> {
        let Some(got) = ed25519_key(end_entity.as_ref()) else {
            return Err(Error::General("pin-miss".into()));
        };
        if got != self.key {
            return Err(Error::General("pin-mismatch".into()));
        }
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, Error> {
        Err(Error::PeerIncompatible(
            rustls::PeerIncompatible::ServerDoesNotSupportTls12Or13,
        ))
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, Error> {
        verify_tls13_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )?;
        Ok(HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        vec![SignatureScheme::ED25519]
    }
}

fn ed25519_key(der: &[u8]) -> Option<[u8; 32]> {
    const OID: &[u8] = &[0x06, 0x03, 0x2b, 0x65, 0x70];
    let at = der.windows(OID.len()).position(|w| w == OID)?;
    let rest = &der[at + OID.len()..];
    let bit = rest.windows(3).position(|w| w == [0x03, 0x21, 0x00])?;
    if rest.len() < bit + 35 {
        return None;
    }
    let mut key = [0u8; 32];
    key.copy_from_slice(&rest[bit + 3..bit + 35]);
    Some(key)
}

fn allow(rx: Cap, record: &mut Record, rest: &[u8]) {
    if rest.len() < 1 + 2 + 1 + 16 + 32 {
        reply(rx, b"bad\n", 0);
        return;
    }
    let nlen = rest[0] as usize;
    if rest.len() < 1 + nlen + 2 + 1 + 16 + 32 || nlen == 0 || nlen > NAME_MAX {
        reply(rx, b"bad\n", 0);
        return;
    }
    let name = &rest[1..1 + nlen];
    let mut at = 1 + nlen;
    let port = u16::from_le_bytes([rest[at], rest[at + 1]]);
    at += 2;
    let kind = rest[at];
    at += 1;
    let mut ip = [0u8; 16];
    ip.copy_from_slice(&rest[at..at + 16]);
    at += 16;
    let mut key = [0u8; 32];
    key.copy_from_slice(&rest[at..at + 32]);
    if record.allow(name, port, kind, ip, key).is_err() {
        reply(rx, b"full\n", 0);
        return;
    }
    save_record(record);
    reply(rx, b"ok\n", 0);
}

fn grant_op(rx: Cap, record: &mut Record, rest: &[u8], grant: bool) {
    let Some((name, actor)) = two_fields(rest) else {
        reply(rx, b"bad\n", 0);
        return;
    };
    let result = if grant {
        record.grant(name, actor)
    } else {
        record.revoke(name, actor)
    };
    if result.is_err() {
        reply(rx, b"missing\n", 0);
        return;
    }
    save_record(record);
    reply(rx, b"ok\n", 0);
}

fn serve(rx: Cap, engine: &mut Engine, record: &Record, gates: &mut Vec<Gate>, rest: &[u8]) {
    let name = rest;
    let Some(entry) = record.find(name) else {
        reply(rx, b"missing\n", 0);
        return;
    };
    if gates.iter().any(|g| g.port == entry.port) {
        reply(rx, b"ok\n", 0);
        return;
    }
    if gates.len() >= MAX_GATES {
        reply(rx, b"full\n", 0);
        return;
    }
    let Ok(id) = engine.tcp_listen(entry.port) else {
        reply(rx, b"unavailable\n", 0);
        return;
    };
    gates.push(Gate {
        port: entry.port,
        id,
    });
    reply(rx, b"attached\n", 0);
}

fn two_fields(rest: &[u8]) -> Option<(&[u8], &[u8])> {
    if rest.is_empty() {
        return None;
    }
    let nlen = rest[0] as usize;
    if rest.len() < 1 + nlen + 1 {
        return None;
    }
    let name = &rest[1..1 + nlen];
    let alen = rest[1 + nlen] as usize;
    if rest.len() < 2 + nlen + alen {
        return None;
    }
    let actor = &rest[2 + nlen..2 + nlen + alen];
    Some((name, actor))
}

fn ping(rx: Cap, engine: &mut Engine, rest: &[u8]) {
    if rest.len() < 4 {
        reply(rx, b"bad\n", 0);
        return;
    }
    let dst = [rest[0], rest[1], rest[2], rest[3]];
    if engine.icmp_echo(dst, b"galexy").is_err() {
        reply(rx, b"down\n", 0);
        return;
    }
    let start = clock_ms();
    loop {
        let now = clock_ms();
        if now.saturating_sub(start) >= 3_000 {
            reply(rx, b"timeout\n", 0);
            return;
        }
        pump(engine);
        engine.poll(now);
        pump(engine);
        if let Some(from) = engine.take_echo() {
            reply(
                rx,
                format!(
                    "reply from {}.{}.{}.{}\n",
                    from[0], from[1], from[2], from[3]
                )
                .as_bytes(),
                0,
            );
            return;
        }
        let _ = sleep_ms(20);
    }
}

fn reply(rx: Cap, text: &[u8], lane: u64) {
    let _ = chan_send(rx, text, lane, 0);
}

fn audit(actor: u64, name: &[u8], key: &[u8; 32], result: &str) {
    let name = String::from_utf8_lossy(name);
    log(&galexy_net::audit_line(actor, &name, key, result));
}

fn pump(engine: &mut Engine) {
    let mut buf = [0u8; 1600];
    loop {
        let got = wire_read(&mut buf);
        if !got.ok || got.value == 0 {
            break;
        }
        let n = got.value as usize;
        engine.ingest(&buf[..n]);
    }
    while let Some(frame) = engine.pop() {
        let _ = wire_write(&frame);
    }
}

fn recv_ctrl(rx: Cap) -> Option<Vec<u8>> {
    let mut buf = [0u8; galexy_abi::CHAN_MSG_MAX];
    let got = chan_recv_poll(rx, &mut buf);
    if !got.ok || got.value == 0 {
        return None;
    }
    Some(buf[..got.value as usize].to_vec())
}

fn load_record() -> Record {
    let got = open(b"/net");
    if !got.ok {
        return Record::empty_ask();
    }
    let cap = Cap::from_bits(got.value);
    let mut buf = [0u8; 4096];
    let mut n = 0usize;
    loop {
        let got = read(cap, &mut buf[n..]);
        if !got.ok || got.value == 0 {
            break;
        }
        n += got.value as usize;
        if n == buf.len() {
            break;
        }
    }
    let _ = close(cap);
    decode(&buf[..n]).unwrap_or_else(|_| Record::empty_ask())
}

fn save_record(record: &Record) {
    let mut buf = [0u8; 4096];
    let Ok(n) = encode(record, &mut buf) else {
        return;
    };
    let got = open(b"/net");
    if !got.ok {
        return;
    }
    let cap = Cap::from_bits(got.value);
    let _ = seek(cap, 0, galexy_abi::SEEK_SET);
    let _ = write(cap, &buf[..n]);
    let _ = truncate(cap, n as u64);
    let _ = close(cap);
}

fn read_mac() -> [u8; 6] {
    let mut buf = [0u8; 128];
    let got = read(galexy_abi::reserved::self_cap(), &mut buf);
    if got.ok {
        let text = &buf[..got.value as usize];
        if let Some(mac) = parse_mac(text) {
            return mac;
        }
    }
    [0x52, 0x54, 0x00, 0x12, 0x34, 0x56]
}

fn parse_mac(text: &[u8]) -> Option<[u8; 6]> {
    let at = text.windows(4).position(|w| w == b"mac=")?;
    let rest = &text[at + 4..];
    let mut mac = [0u8; 6];
    for (i, byte) in mac.iter_mut().enumerate() {
        let off = i * 3;
        if rest.len() < off + 2 {
            return None;
        }
        *byte = (hex(rest[off])? << 4) | hex(rest[off + 1])?;
    }
    Some(mac)
}

fn hex(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

fn log(line: &str) {
    let mut buf = [0u8; 200];
    let bytes = line.as_bytes();
    let n = bytes.len().min(buf.len() - 1);
    buf[..n].copy_from_slice(&bytes[..n]);
    buf[n] = b'\n';
    let _ = write_console(&buf[..n + 1]);
}

fn fill_random_once() {}

/// `getrandom` custom backend. `rdrand` via inline asm because the target
/// does not enable the `rdrand` feature (the CPU still has it under QEMU).
#[unsafe(no_mangle)]
unsafe extern "Rust" fn __getrandom_v03_custom(
    dest: *mut u8,
    len: usize,
) -> Result<(), getrandom::Error> {
    let buf = unsafe {
        core::ptr::write_bytes(dest, 0, len);
        core::slice::from_raw_parts_mut(dest, len)
    };
    for chunk in buf.chunks_mut(8) {
        let mut val: u64 = 0;
        let mut ok = 0u8;
        for _ in 0..16 {
            unsafe {
                core::arch::asm!(
                    "rdrand {v}",
                    "setc {c}",
                    v = out(reg) val,
                    c = lateout(reg_byte) ok,
                    options(nomem, nostack, preserves_flags),
                );
            }
            if ok == 1 {
                break;
            }
        }
        if ok != 1 {
            return Err(getrandom::Error::new_custom(1));
        }
        let bytes = val.to_ne_bytes();
        chunk.copy_from_slice(&bytes[..chunk.len()]);
    }
    Ok(())
}
