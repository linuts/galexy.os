//! The `net` service. It is the only task that touches the wire.
//!
//! Frames go through [`galexy_net`]. TLS 1.3 is rustls with the pure-Rust
//! provider. A handshake that is not finished in five seconds is dropped.
//! Session keys are not written to the console. DNS-over-TLS checks the
//! resolver against the Mozilla roots in this image, unless a key file
//! was passed. That key is the pin, and the roots are not consulted.
//! No key file reads the domain off the certificate, tells the seat,
//! and later requires that domain. A lane does not use those roots.

mod trust;

use std::io::{Read, Write};
use std::sync::{Arc, Mutex};

use galexy_abi::{
    Cap, SysError, INIT_RPC_HDR, NET_HELD_OUT, NET_OP_ALLOW, NET_OP_BIND, NET_OP_DGRAM,
    NET_OP_DIAL, NET_OP_GRANT, NET_OP_LINK, NET_OP_LISTEN, NET_OP_PIN, NET_OP_PING, NET_OP_RECV,
    NET_OP_RESOLVE, NET_OP_REVOKE, NET_OP_SERVE, NET_OP_STREAM,
};
use galexy_net::Engine;
use galexy_netcfg::{
    decode, encode, Record, KIND_ADDR, KIND_DGRAM, KIND_DNS, KIND_IPV4, KIND_PORT, MODE_FIXED,
    NAME_MAX,
};
use galexy_rt::{
    chan_recv_poll, chan_send, channel, clock_ms, close, lane_pair, open, read, read_poll, seek,
    sleep_ms, truncate, wire_read, wire_write, write, write_console, write_poll,
};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{verify_tls13_signature, CryptoProvider};
use rustls::pki_types::{CertificateDer, IpAddr, Ipv4Addr, PrivateKeyDer, ServerName, UnixTime};
use rustls::{
    ClientConfig, ClientConnection, Connection, DigitallySignedStruct, Error, ServerConfig,
    ServerConnection, SignatureScheme,
};

/// Listener identity presented on an inbound gate. The private key stays
/// in this image and is never written to the console.
const LISTENER_CERT: &[u8] = include_bytes!("listener_cert.der");
const LISTENER_KEY: &[u8] = include_bytes!("listener_key.der");

const HANDSHAKE_MS: u64 = 5_000;
const MAX_GATES: usize = 8;
const PENDING: usize = 4;
/// Source port for a card datagram. Not 53, and not a program argument.
const DGRAM_SPORT: u16 = 4598;

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
    let mut logged_addr = false;
    let mut logged_udp = false;
    let mut logged_tcp = false;
    let mut flood_at: Option<u64> = None;
    let mut flood_ids: Vec<u8> = Vec::new();
    let mut flood_logged = false;
    let mut lives: Vec<Live> = Vec::new();
    let mut plains: Vec<Plain> = Vec::new();
    let mut seen: Option<Seen> = None;
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
        enqueue_accepted(&mut engine, &mut gates);
        pump_lives(&mut engine, &mut lives);
        pump_plains(&mut engine, &mut plains);
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
                &mut lives,
                &mut plains,
                &mut seen,
            );
        }
        if sleep_ms(20).ok {
            continue;
        }
        let err = SysError::from_code(0);
        let _ = err;
    }
}

/// A peer key learned on a dial that had no pin yet. It is not stored
/// until the seat says yes.
struct Seen {
    name: Vec<u8>,
    key: [u8; 32],
}

struct Gate {
    port: u16,
    /// Listen slot. Later accepts on this port belong to `listener`.
    id: u8,
    /// Task that attached the gate. A later `serve` from anyone else
    /// does not receive the lane.
    listener: u64,
    /// Accepted connections waiting for that listener. At most [`PENDING`].
    pending: Vec<u8>,
}

fn handle(
    msg: &[u8],
    rx: Cap,
    engine: &mut Engine,
    record: &mut Record,
    gates: &mut Vec<Gate>,
    now: u64,
    flood_at: &mut Option<u64>,
    lives: &mut Vec<Live>,
    plains: &mut Vec<Plain>,
    seen: &mut Option<Seen>,
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
        NET_OP_DIAL => dial(
            rx, engine, record, admin, actor_id, msg[2], rest, now, plains,
        ),
        NET_OP_RESOLVE => resolve(rx, engine, record, actor_id, msg[2], rest, now),
        NET_OP_PIN if admin => pin(rx, engine, record, rest, now),
        NET_OP_BIND => bind_local(rx, engine, record, msg[2], rest),
        NET_OP_LISTEN => listen(rx, engine, record, msg[2], actor_id, gates, rest),
        NET_OP_DGRAM => datagram(rx, engine, record, msg[2], rest),
        NET_OP_RECV => datagram_recv(rx, engine, record, msg[2], rest),
        NET_OP_ALLOW if admin => allow(rx, record, rest),
        NET_OP_GRANT if admin => grant_op(rx, record, rest, true),
        NET_OP_REVOKE if admin => grant_op(rx, record, rest, false),
        NET_OP_SERVE if admin => serve(rx, engine, record, msg[2], actor_id, gates, rest),
        NET_OP_PING => ping(rx, engine, record, msg[2], rest, now),
        NET_OP_STREAM => stream(
            rx, engine, record, msg[2], actor_id, gates, rest, lives, seen,
        ),
        NET_OP_ALLOW | NET_OP_GRANT | NET_OP_REVOKE | NET_OP_SERVE | NET_OP_PIN => {
            reply(rx, b"access denied\n", 0);
        }
        _ => reply(rx, b"bad\n", 0),
    }
}

fn dial(
    rx: Cap,
    engine: &mut Engine,
    record: &Record,
    _admin: bool,
    actor_id: u64,
    held: u8,
    rest: &[u8],
    now: u64,
    plains: &mut Vec<Plain>,
) {
    let Some((name, _claimed)) = two_fields(rest) else {
        reply(rx, b"bad\n", 0);
        return;
    };
    // Read on the card is dial. The check is the held bit from the
    // kernel. A passed address is not a directory name. The outbound
    // bit dials a name or an address without that name's card.
    if let Some((ip, passed_port)) = parse_passed(name) {
        dial_passed(
            rx,
            engine,
            record,
            actor_id,
            held,
            ip,
            passed_port,
            now,
            plains,
        );
        return;
    }
    let Some(entry) = record.find(name) else {
        audit(actor_id, name, &[0; 32], "missing");
        reply(rx, b"access denied\n", 0);
        return;
    };
    if held & 1 == 0 {
        audit(actor_id, name, &entry.key, "denied");
        reply(rx, b"access denied\n", 0);
        return;
    }
    let (ip, port, key) = match entry.kind {
        KIND_IPV4 => {
            let ip = [entry.ip[0], entry.ip[1], entry.ip[2], entry.ip[3]];
            (ip, entry.port, entry.key)
        }
        KIND_DNS => match lookup_dns(engine, record, name, now) {
            Ok(ip) => (ip, entry.port, entry.key),
            Err(why) => {
                reply_lookup(rx, why);
                return;
            }
        },
        _ => {
            reply(rx, b"unsupported\n", 0);
            return;
        }
    };
    if port == 0 || ip == [0, 0, 0, 0] {
        reply(rx, b"access denied\n", 0);
        return;
    }
    open_client(
        rx,
        engine,
        actor_id,
        name,
        ip,
        port,
        &key,
        now,
        held & NET_HELD_OUT != 0,
        plains,
    );
}

fn dial_passed(
    rx: Cap,
    engine: &mut Engine,
    record: &Record,
    actor_id: u64,
    held: u8,
    ip: [u8; 4],
    passed_port: Option<u16>,
    now: u64,
    plains: &mut Vec<Plain>,
) {
    let outbound = held & NET_HELD_OUT != 0;
    if held & 1 == 0 || ip == [0, 0, 0, 0] {
        reply(rx, b"access denied\n", 0);
        return;
    }
    let stored = find_kind(record, KIND_ADDR, ip, None);
    if outbound {
        // A stored address keeps its pin. The port in the dial does
        // not need a second card. An address that is not stored is
        // plaintext on the port the program passed.
        let port = if let Some(port) = passed_port {
            port
        } else if let Some(addr) = stored {
            addr.port
        } else {
            reply(rx, b"access denied\n", 0);
            return;
        };
        if port == 0 {
            reply(rx, b"access denied\n", 0);
            return;
        }
        let key = stored.map(|addr| addr.key).unwrap_or([0; 32]);
        let label = stored
            .map(|addr| addr.name_bytes().to_vec())
            .unwrap_or_default();
        open_client(
            rx, engine, actor_id, &label, ip, port, &key, now, true, plains,
        );
        return;
    }
    let Some(addr) = stored else {
        reply(rx, b"access denied\n", 0);
        return;
    };
    let port = if let Some(port) = passed_port {
        if find_kind(record, KIND_PORT, ip, Some(port)).is_none() {
            reply(rx, b"access denied\n", 0);
            return;
        }
        port
    } else {
        addr.port
    };
    if port == 0 {
        reply(rx, b"access denied\n", 0);
        return;
    }
    let key = addr.key;
    let label = addr.name_bytes().to_vec();
    open_client(
        rx, engine, actor_id, &label, ip, port, &key, now, false, plains,
    );
}

/// A stored pin stays the one-shot TLS fetch. So does every dial that
/// does not hold the outbound card, including an empty pin: the echo
/// tests speak TLS. An empty pin on the outbound card is a plaintext
/// lane. The program writes the request and reads the reply there.
fn open_client(
    rx: Cap,
    engine: &mut Engine,
    actor_id: u64,
    name: &[u8],
    ip: [u8; 4],
    port: u16,
    key: &[u8; 32],
    now: u64,
    outbound: bool,
    plains: &mut Vec<Plain>,
) {
    if outbound && *key == [0; 32] {
        plain_open(rx, engine, actor_id, name, ip, port, now, plains);
    } else {
        finish_dial(rx, engine, actor_id, name, ip, port, key, now);
    }
}

/// One plaintext TCP session. The program writes on the lane and reads
/// the reply there. No second card, and no TLS.
struct Plain {
    tcp: u8,
    lane: Cap,
    to_lane: Vec<u8>,
    to_tcp: Vec<u8>,
}

fn plain_open(
    rx: Cap,
    engine: &mut Engine,
    actor_id: u64,
    name: &[u8],
    ip: [u8; 4],
    port: u16,
    now: u64,
    plains: &mut Vec<Plain>,
) {
    log(&format!(
        "[net] dial {}.{}.{}.{}",
        ip[0], ip[1], ip[2], ip[3]
    ));
    let Ok(id) = engine.tcp_connect(ip, port) else {
        audit(actor_id, name, &[0; 32], "unavailable");
        reply(rx, b"unavailable\n", 0);
        return;
    };
    if !wait_established(engine, id, now) {
        engine.tcp_release(id);
        audit(actor_id, name, &[0; 32], "deadline");
        log("[net] deadline");
        reply(rx, b"timed out\n", 0);
        return;
    }
    let mut pair = [0u64; 2];
    if !lane_pair(&mut pair).ok {
        engine.tcp_close(id);
        audit(actor_id, name, &[0; 32], "unavailable");
        reply(rx, b"unavailable\n", 0);
        return;
    }
    plains.push(Plain {
        tcp: id,
        lane: Cap::from_bits(pair[0]),
        to_lane: Vec::new(),
        to_tcp: Vec::new(),
    });
    audit(actor_id, name, &[0; 32], "ok");
    reply(rx, b"ok\n", pair[1]);
}

fn pump_plains(engine: &mut Engine, plains: &mut Vec<Plain>) {
    let mut i = 0;
    while i < plains.len() {
        if step_plain(engine, &mut plains[i]) {
            i += 1;
        } else {
            let plain = plains.swap_remove(i);
            engine.tcp_close(plain.tcp);
            let _ = close(plain.lane);
        }
    }
}

fn step_plain(engine: &mut Engine, plain: &mut Plain) -> bool {
    let mut buf = [0u8; 1024];
    let n = engine.tcp_recv(plain.tcp, &mut buf);
    if n > 0 {
        plain.to_lane.extend_from_slice(&buf[..n]);
    }
    if !plain.to_lane.is_empty() {
        let got = write_poll(plain.lane, &plain.to_lane);
        if got.ok {
            let n = (got.value as usize).min(plain.to_lane.len());
            plain.to_lane.drain(..n);
        } else if SysError::from_code(got.value) != SysError::NoResource {
            return false;
        }
    }
    let got = read_poll(plain.lane, &mut buf);
    if got.ok {
        if got.value == 0 {
            return false;
        }
        plain.to_tcp.extend_from_slice(&buf[..got.value as usize]);
    } else if SysError::from_code(got.value) != SysError::NoResource {
        return false;
    }
    let mut at = 0;
    while at < plain.to_tcp.len() {
        match engine.tcp_send(plain.tcp, &plain.to_tcp[at..]) {
            Ok(0) => break,
            Err(()) => return false,
            Ok(n) => at += n,
        }
    }
    if at > 0 {
        plain.to_tcp.drain(..at);
        engine.poll(clock_ms());
    }
    engine.tcp_established(plain.tcp) || !plain.to_lane.is_empty()
}

fn finish_dial(
    rx: Cap,
    engine: &mut Engine,
    actor_id: u64,
    name: &[u8],
    ip: [u8; 4],
    port: u16,
    key: &[u8; 32],
    now: u64,
) {
    log(&format!(
        "[net] dial {}.{}.{}.{}",
        ip[0], ip[1], ip[2], ip[3]
    ));
    let Ok(id) = engine.tcp_connect(ip, port) else {
        audit(actor_id, name, key, "unavailable");
        reply(rx, b"unavailable\n", 0);
        return;
    };
    if !wait_established(engine, id, now) {
        engine.tcp_release(id);
        audit(actor_id, name, key, "deadline");
        log("[net] deadline");
        reply(rx, b"timed out\n", 0);
        return;
    }
    let name_str = String::from_utf8_lossy(name).into_owned();
    match tls_fetch(engine, id, &name_str, key, now) {
        Ok(body) => {
            let mut pair = [0u64; 2];
            if !lane_pair(&mut pair).ok {
                audit(actor_id, name, key, "unavailable");
                reply(rx, b"unavailable\n", 0);
                return;
            }
            let ours = Cap::from_bits(pair[0]);
            let _ = write(ours, &body);
            let _ = close(ours);
            audit(actor_id, name, key, "ok");
            reply(rx, b"ok\n", pair[1]);
        }
        Err(why) => {
            engine.tcp_release(id);
            audit(actor_id, name, key, why);
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
    let server_name = if ServerName::try_from(name.to_string()).is_ok() {
        name.to_string()
    } else {
        "echo".to_string()
    };
    let server = ServerName::try_from(server_name).map_err(|_| "bad")?;
    let config = client_config(key).map_err(|_| "bad")?;
    let mut conn = Connection::Client(ClientConnection::new(config, server).map_err(|_| "bad")?);
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
    conn: &mut Connection,
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
            let text = format!("{err}");
            log(&format!("[net] tls {text}"));
            if text.contains("pin-mismatch") {
                "changed"
            } else {
                "denied"
            }
        })?;
        let mut plain = [0u8; 2048];
        let got = conn.reader().read(&mut plain).unwrap_or(0);
        body.extend_from_slice(&plain[..got]);
    }
    Ok(())
}

fn client_config(key: &[u8; 32]) -> Result<Arc<ClientConfig>, Error> {
    trust::pinned_client(*key, trust::provider())
}

#[derive(Debug)]
struct LearnVerifier {
    key: Mutex<[u8; 32]>,
    saw: Mutex<bool>,
    provider: Arc<CryptoProvider>,
}

impl ServerCertVerifier for LearnVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, Error> {
        let Some(got) = trust::ed25519_key(end_entity.as_ref()) else {
            return Err(Error::General("pin-miss".into()));
        };
        if let Ok(mut slot) = self.key.lock() {
            *slot = got;
        }
        if let Ok(mut slot) = self.saw.lock() {
            *slot = true;
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
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}

fn resolve(
    rx: Cap,
    engine: &mut Engine,
    record: &Record,
    _actor_id: u64,
    held: u8,
    rest: &[u8],
    now: u64,
) {
    let Some(name) = one_name(rest) else {
        reply(rx, b"bad\n", 0);
        return;
    };
    if held & 1 == 0 || record.find(name).is_none_or(|entry| entry.kind != KIND_DNS) {
        reply(rx, b"access denied\n", 0);
        return;
    }
    match lookup_dns(engine, record, name, now) {
        Ok(ip) => {
            let text = format!("{}.{}.{}.{}\n", ip[0], ip[1], ip[2], ip[3]);
            reply(rx, text.as_bytes(), 0);
        }
        Err(why) => reply_lookup(rx, why),
    }
}

fn pin(rx: Cap, engine: &mut Engine, record: &mut Record, rest: &[u8], now: u64) {
    let key = if rest.len() == 4 {
        None
    } else if rest.len() >= 4 + 32 {
        let mut key = [0u8; 32];
        key.copy_from_slice(&rest[4..36]);
        Some(key)
    } else {
        reply(rx, b"bad\n", 0);
        return;
    };
    let addr = [rest[0], rest[1], rest[2], rest[3]];
    if addr == [10, 0, 2, 2] || addr == [10, 0, 2, 3] {
        reply(rx, b"access denied\n", 0);
        return;
    }
    if key.is_none() {
        let name = match probe_domain(engine, addr, now) {
            Ok(name) => name,
            Err(why) => {
                reply_lookup(rx, why);
                return;
            }
        };
        if record.pin(addr, None).is_err() || record.pin_domain(&name).is_err() {
            reply(rx, b"access denied\n", 0);
            return;
        }
        save_record(record);
        let mut text = Vec::with_capacity(4 + name.len());
        text.extend_from_slice(b"ok ");
        text.extend_from_slice(&name);
        text.push(b'\n');
        log(&format!("[net] pin {}", String::from_utf8_lossy(&name)));
        reply(rx, &text, 0);
        return;
    }
    if record.pin(addr, key).is_err() {
        reply(rx, b"access denied\n", 0);
        return;
    }
    save_record(record);
    reply(rx, b"ok\n", 0);
}

fn bind_local(rx: Cap, engine: &mut Engine, record: &Record, held: u8, rest: &[u8]) {
    let Some(name) = one_name(rest) else {
        reply(rx, b"bad\n", 0);
        return;
    };
    if held & 2 == 0 {
        reply(rx, b"access denied\n", 0);
        return;
    }
    let Some(ip) = parse_ip(name) else {
        reply(rx, b"access denied\n", 0);
        return;
    };
    if ip == [0, 0, 0, 0] || find_kind(record, KIND_ADDR, ip, None).is_none() {
        reply(rx, b"access denied\n", 0);
        return;
    }
    let Some((ours, _, _)) = engine.locator() else {
        reply(rx, b"unavailable\n", 0);
        return;
    };
    if ip != ours {
        reply(rx, b"access denied\n", 0);
        return;
    }
    log(&format!(
        "[net] bound {}.{}.{}.{}",
        ip[0], ip[1], ip[2], ip[3]
    ));
    reply(rx, b"ok\n", 0);
}

fn lookup_dns(
    engine: &mut Engine,
    record: &Record,
    name: &[u8],
    now: u64,
) -> Result<[u8; 4], &'static str> {
    let Some(resolver) = record.resolver else {
        return Err("denied");
    };
    if resolver.key.is_none() && resolver.name_len == 0 {
        return Err("denied");
    }
    let query = dns_query(name).ok_or("bad")?;
    let id = engine
        .tcp_connect(resolver.addr, galexy_netcfg::DOT_PORT)
        .map_err(|_| "unavailable")?;
    if !wait_established(engine, id, now) {
        engine.tcp_release(id);
        return Err("deadline");
    }
    let dns_name = (resolver.key.is_none() && resolver.name_len > 0)
        .then_some(&resolver.name[..resolver.name_len as usize]);
    let result = match tls_exchange(engine, id, resolver.key, dns_name, now, &query) {
        Ok(body) => dot_answer(&body).ok_or("denied"),
        Err(why) => Err(why),
    };
    engine.tcp_release(id);
    if let Ok(ip) = result {
        log(&format!(
            "[net] dot {}.{}.{}.{}",
            ip[0], ip[1], ip[2], ip[3]
        ));
    }
    result
}

fn reply_lookup(rx: Cap, why: &'static str) {
    if why == "deadline" {
        log("[net] deadline");
        reply(rx, b"timed out\n", 0);
    } else if why == "unavailable" {
        reply(rx, b"unavailable\n", 0);
    } else {
        reply(rx, b"access denied\n", 0);
    }
}

fn wait_established(engine: &mut Engine, id: u8, start: u64) -> bool {
    while clock_ms().saturating_sub(start) < HANDSHAKE_MS {
        pump(engine);
        engine.poll(clock_ms());
        pump(engine);
        if engine.tcp_established(id) {
            return true;
        }
        let _ = sleep_ms(20);
    }
    false
}

/// Handshake only. The domain on the certificate is the pin. No query
/// is written.
fn probe_domain(engine: &mut Engine, addr: [u8; 4], start: u64) -> Result<Vec<u8>, &'static str> {
    let id = engine
        .tcp_connect(addr, galexy_netcfg::DOT_PORT)
        .map_err(|_| "unavailable")?;
    if !wait_established(engine, id, start) {
        engine.tcp_release(id);
        return Err("deadline");
    }
    let result = probe_leaf(engine, id, addr, start);
    engine.tcp_release(id);
    let leaf = result?;
    let name = trust::dns_pin_name(&leaf, galexy_netcfg::RESOLVER_NAME_MAX).ok_or("denied")?;
    if !trust::name_on_cert(&leaf, &name) {
        return Err("denied");
    }
    Ok(name)
}

fn probe_leaf(
    engine: &mut Engine,
    id: u8,
    addr: [u8; 4],
    start: u64,
) -> Result<Vec<u8>, &'static str> {
    let config =
        trust::learn_client(trust::bundle_roots(), trust::provider()).map_err(|_| "bad")?;
    let server = ServerName::IpAddress(IpAddr::V4(Ipv4Addr::from(addr)));
    let mut conn = Connection::Client(ClientConnection::new(config, server).map_err(|_| "bad")?);
    let limit = start.saturating_add(HANDSHAKE_MS);
    while clock_ms() < limit {
        if !conn.is_handshaking() {
            break;
        }
        drive_tls(engine, id, &mut conn, &mut Vec::new())?;
        pump(engine);
        let _ = sleep_ms(20);
    }
    if conn.is_handshaking() {
        return Err("deadline");
    }
    conn.peer_certificates()
        .and_then(|chain| chain.first())
        .map(|cert| cert.as_ref().to_vec())
        .ok_or("denied")
}

fn tls_exchange(
    engine: &mut Engine,
    id: u8,
    key: Option<[u8; 32]>,
    dns_name: Option<&[u8]>,
    start: u64,
    send: &[u8],
) -> Result<Vec<u8>, &'static str> {
    let (config, server) =
        trust::resolver_client(key, dns_name, trust::provider()).map_err(|_| "bad")?;
    let mut conn = Connection::Client(ClientConnection::new(config, server).map_err(|_| "bad")?);
    let mut body = Vec::new();
    let mut sent = false;
    let limit = start.saturating_add(HANDSHAKE_MS.saturating_mul(2));
    while clock_ms() < limit {
        if clock_ms().saturating_sub(start) >= HANDSHAKE_MS && conn.is_handshaking() {
            return Err("deadline");
        }
        if !conn.is_handshaking() && !sent {
            conn.writer().write_all(send).map_err(|_| "bad")?;
            sent = true;
        }
        drive_tls(engine, id, &mut conn, &mut body)?;
        if sent && dot_complete(&body) {
            break;
        }
        pump(engine);
        let _ = sleep_ms(20);
    }
    if !sent || !dot_complete(&body) {
        return Err("deadline");
    }
    Ok(body)
}

fn find_kind(
    record: &Record,
    kind: u8,
    ip: [u8; 4],
    port: Option<u16>,
) -> Option<&galexy_netcfg::Name> {
    record.names[..record.name_count as usize]
        .iter()
        .find(|entry| {
            entry.kind == kind && entry.ip[..4] == ip && port.is_none_or(|port| entry.port == port)
        })
}

/// A dotted DNS name. Four payload bytes stay an IPv4 address, so a
/// three-byte name is not a name on this opcode.
fn dns_name(name: &[u8]) -> bool {
    if name.is_empty() || name.len() == 3 || name.len() > 234 {
        return false;
    }
    let mut label = 0usize;
    let mut dot = false;
    for b in name {
        match b {
            b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'-' => {
                label += 1;
                if label > 63 {
                    return false;
                }
            }
            b'.' => {
                if label == 0 {
                    return false;
                }
                dot = true;
                label = 0;
            }
            _ => return false,
        }
    }
    label != 0 && dot
}

fn dns_rest(rest: &[u8]) -> Option<&[u8]> {
    let name = one_name(rest)?;
    if rest.len() != 1 + name.len() || !dns_name(name) {
        return None;
    }
    Some(name)
}

fn one_name(rest: &[u8]) -> Option<&[u8]> {
    if rest.is_empty() {
        return None;
    }
    let nlen = rest[0] as usize;
    if nlen == 0 || rest.len() < 1 + nlen {
        return None;
    }
    Some(&rest[1..1 + nlen])
}

fn parse_passed(name: &[u8]) -> Option<([u8; 4], Option<u16>)> {
    if let Some(colon) = name.iter().position(|b| *b == b':') {
        let ip = parse_ip(&name[..colon])?;
        let port = parse_port(&name[colon + 1..])?;
        return Some((ip, Some(port)));
    }
    parse_ip(name).map(|ip| (ip, None))
}

fn parse_ip(text: &[u8]) -> Option<[u8; 4]> {
    let mut ip = [0u8; 4];
    let mut parts = 0usize;
    for part in text.split(|b| *b == b'.') {
        if parts == 4 {
            return None;
        }
        ip[parts] = parse_octet(part)?;
        parts += 1;
    }
    if parts == 4 {
        Some(ip)
    } else {
        None
    }
}

fn parse_octet(text: &[u8]) -> Option<u8> {
    if text.is_empty() || text.len() > 3 || (text.len() > 1 && text[0] == b'0') {
        return None;
    }
    let mut n = 0u16;
    for b in text {
        let d = b.wrapping_sub(b'0');
        if d > 9 {
            return None;
        }
        n = n.checked_mul(10)?.checked_add(u16::from(d))?;
    }
    u8::try_from(n).ok()
}

fn parse_port(text: &[u8]) -> Option<u16> {
    if text.is_empty() || (text.len() > 1 && text[0] == b'0') {
        return None;
    }
    let mut n = 0u16;
    for b in text {
        let d = b.wrapping_sub(b'0');
        if d > 9 {
            return None;
        }
        n = n.checked_mul(10)?.checked_add(u16::from(d))?;
    }
    if n == 0 {
        None
    } else {
        Some(n)
    }
}

fn dns_query(name: &[u8]) -> Option<Vec<u8>> {
    let mut msg = Vec::new();
    msg.extend_from_slice(&1u16.to_be_bytes());
    msg.extend_from_slice(&0x0100u16.to_be_bytes());
    msg.extend_from_slice(&1u16.to_be_bytes());
    msg.extend_from_slice(&[0, 0, 0, 0, 0, 0]);
    if name.is_empty() {
        return None;
    }
    for label in name.split(|b| *b == b'.') {
        if label.is_empty() || label.len() > 63 {
            return None;
        }
        msg.push(label.len() as u8);
        msg.extend_from_slice(label);
    }
    msg.push(0);
    msg.extend_from_slice(&1u16.to_be_bytes());
    msg.extend_from_slice(&1u16.to_be_bytes());
    let mut wire = Vec::new();
    let len = u16::try_from(msg.len()).ok()?;
    wire.extend_from_slice(&len.to_be_bytes());
    wire.extend_from_slice(&msg);
    Some(wire)
}

fn dot_complete(body: &[u8]) -> bool {
    if body.len() < 2 {
        return false;
    }
    let n = u16::from_be_bytes([body[0], body[1]]) as usize;
    body.len() >= 2 + n && n >= 12
}

fn dot_answer(body: &[u8]) -> Option<[u8; 4]> {
    if !dot_complete(body) {
        return None;
    }
    let n = u16::from_be_bytes([body[0], body[1]]) as usize;
    dns_a(&body[2..2 + n])
}

fn dns_a(buf: &[u8]) -> Option<[u8; 4]> {
    if buf.len() < 12 {
        return None;
    }
    let answers = u16::from_be_bytes([buf[6], buf[7]]) as usize;
    let mut i = skip_name(buf, 12)?;
    if i + 4 > buf.len() {
        return None;
    }
    i += 4;
    for _ in 0..answers {
        i = skip_name(buf, i)?;
        if i + 10 > buf.len() {
            return None;
        }
        let kind = u16::from_be_bytes([buf[i], buf[i + 1]]);
        let rdlen = u16::from_be_bytes([buf[i + 8], buf[i + 9]]) as usize;
        i += 10;
        if i + rdlen > buf.len() {
            return None;
        }
        if kind == 1 && rdlen == 4 {
            let ip = [buf[i], buf[i + 1], buf[i + 2], buf[i + 3]];
            if ip != [10, 0, 2, 2] && ip != [0, 0, 0, 0] {
                return Some(ip);
            }
        }
        i += rdlen;
    }
    None
}

fn skip_name(buf: &[u8], mut i: usize) -> Option<usize> {
    for _ in 0..16 {
        if i >= buf.len() {
            return None;
        }
        let len = buf[i];
        if len == 0 {
            return Some(i + 1);
        }
        if len & 0xc0 == 0xc0 {
            if i + 1 >= buf.len() {
                return None;
            }
            return Some(i + 2);
        }
        i += 1 + len as usize;
    }
    None
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

fn serve(
    rx: Cap,
    engine: &mut Engine,
    record: &Record,
    held: u8,
    actor_id: u64,
    gates: &mut Vec<Gate>,
    rest: &[u8],
) {
    let name = rest;
    let Some(entry) = record.find(name) else {
        reply(rx, b"missing\n", 0);
        return;
    };
    // Write on the name's object is accept. No token, no listener.
    if held & 2 == 0 {
        reply(rx, b"access denied\n", 0);
        return;
    }
    let port = entry.port;
    let key = entry.key;
    hand_gate(rx, engine, actor_id, gates, name, port, key, false);
}

fn listen(
    rx: Cap,
    engine: &mut Engine,
    record: &Record,
    held: u8,
    actor_id: u64,
    gates: &mut Vec<Gate>,
    rest: &[u8],
) {
    let Some(name) = one_name(rest) else {
        reply(rx, b"bad\n", 0);
        return;
    };
    if held & 2 == 0 {
        reply(rx, b"access denied\n", 0);
        return;
    }
    let Some(port) = parse_port(name) else {
        reply(rx, b"access denied\n", 0);
        return;
    };
    let Some(entry) = record.find(name) else {
        reply(rx, b"access denied\n", 0);
        return;
    };
    if entry.kind != KIND_PORT || entry.port != port {
        reply(rx, b"access denied\n", 0);
        return;
    }
    hand_gate(rx, engine, actor_id, gates, name, port, entry.key, true);
}

fn hand_gate(
    rx: Cap,
    engine: &mut Engine,
    actor_id: u64,
    gates: &mut Vec<Gate>,
    name: &[u8],
    port: u16,
    key: [u8; 32],
    announce: bool,
) {
    if let Some(gate) = gates.iter().find(|g| g.port == port) {
        if gate.listener != actor_id {
            reply(rx, b"held\n", 0);
            return;
        }
    } else if gates.len() >= MAX_GATES {
        reply(rx, b"full\n", 0);
        return;
    } else {
        let Ok(id) = engine.tcp_listen(port) else {
            reply(rx, b"unavailable\n", 0);
            return;
        };
        gates.push(Gate {
            port,
            id,
            listener: actor_id,
            pending: Vec::new(),
        });
        if announce {
            log(&format!("[net] listen {port}"));
        }
        reply(rx, b"attached\n", 0);
        return;
    }
    let start = clock_ms();
    let id = match next_inbound(engine, gates, port, start) {
        Ok(id) => id,
        Err(why) => {
            audit(actor_id, name, &key, why);
            reply(rx, b"timed out\n", 0);
            return;
        }
    };
    match tls_accept(engine, id, clock_ms()) {
        Ok(body) => {
            let mut pair = [0u64; 2];
            if !lane_pair(&mut pair).ok {
                engine.tcp_close(id);
                audit(actor_id, name, &key, "unavailable");
                reply(rx, b"unavailable\n", 0);
                return;
            }
            let ours = Cap::from_bits(pair[0]);
            let _ = write(ours, &body);
            let _ = close(ours);
            audit(actor_id, name, &key, "handed");
            reply(rx, b"ok\n", pair[1]);
        }
        Err(why) => {
            engine.tcp_close(id);
            audit(actor_id, name, &key, why);
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

/// The listener's next accepted connection. Other gates keep their own queues.
fn next_inbound(
    engine: &mut Engine,
    gates: &mut Vec<Gate>,
    port: u16,
    start: u64,
) -> Result<u8, &'static str> {
    loop {
        enqueue_accepted(engine, gates);
        if let Some(id) = take_pending(gates, port) {
            return Ok(id);
        }
        if clock_ms().saturating_sub(start) >= HANDSHAKE_MS.saturating_mul(3) {
            return Err("deadline");
        }
        pump(engine);
        engine.poll(clock_ms());
        pump(engine);
        let _ = sleep_ms(20);
    }
}

fn take_pending(gates: &mut [Gate], port: u16) -> Option<u8> {
    let gate = gates.iter_mut().find(|g| g.port == port)?;
    if gate.pending.is_empty() {
        None
    } else {
        Some(gate.pending.remove(0))
    }
}

/// Queue an accepted connection on the gate for its local port.
/// A port with no gate, or a gate that already holds [`PENDING`] lanes,
/// is closed here. Nobody else receives it.
fn enqueue_accepted(engine: &mut Engine, gates: &mut Vec<Gate>) {
    while let Some(id) = engine.take_accepted() {
        if gates.iter().any(|g| g.id == id) {
            engine.tcp_close(id);
            continue;
        }
        let Some(port) = engine.tcp_local(id) else {
            engine.tcp_close(id);
            continue;
        };
        let Some(gate) = gates.iter_mut().find(|g| g.port == port) else {
            engine.tcp_close(id);
            continue;
        };
        if gate.pending.len() >= PENDING {
            engine.tcp_close(id);
            continue;
        }
        gate.pending.push(id);
    }
}

fn tls_accept(engine: &mut Engine, id: u8, start: u64) -> Result<Vec<u8>, &'static str> {
    let config = server_config().map_err(|_| "bad")?;
    let mut conn = Connection::Server(ServerConnection::new(config).map_err(|_| "bad")?);
    let mut body = Vec::new();
    let limit = start.saturating_add(HANDSHAKE_MS.saturating_mul(2));
    while clock_ms() < limit {
        if clock_ms().saturating_sub(start) >= HANDSHAKE_MS && conn.is_handshaking() {
            return Err("deadline");
        }
        drive_tls(engine, id, &mut conn, &mut body)?;
        if !conn.is_handshaking() && !body.is_empty() && !conn.wants_read() {
            break;
        }
        pump(engine);
        let _ = sleep_ms(20);
    }
    if conn.is_handshaking() || body.is_empty() {
        return Err("deadline");
    }
    engine.tcp_close(id);
    Ok(body)
}

fn server_config() -> Result<Arc<ServerConfig>, Error> {
    let cert = CertificateDer::from(LISTENER_CERT.to_vec());
    let key = PrivateKeyDer::try_from(LISTENER_KEY.to_vec())
        .map_err(|_| Error::General("listener key".into()))?;
    let config = ServerConfig::builder_with_provider(trust::provider())
        .with_protocol_versions(&[&rustls::version::TLS13])?
        .with_no_client_auth()
        .with_single_cert(vec![cert], key)?;
    Ok(Arc::new(config))
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

fn datagram(rx: Cap, engine: &mut Engine, record: &Record, held: u8, rest: &[u8]) {
    let Some((name, payload)) = name_and_body(rest) else {
        reply(rx, b"bad\n", 0);
        return;
    };
    // Read sends. Write does not capture, and it does not send.
    let Some((ip, port)) = dgram_peer(record, held, name) else {
        reply(rx, b"access denied\n", 0);
        return;
    };
    if engine.udp_send(ip, port, DGRAM_SPORT, payload).is_err() {
        reply(rx, b"unavailable\n", 0);
        return;
    }
    let start = clock_ms();
    while !engine.udp_idle() && clock_ms().saturating_sub(start) < 1_000 {
        pump(engine);
        engine.poll(clock_ms());
        pump(engine);
        let _ = sleep_ms(20);
    }
    if !engine.udp_idle() {
        reply(rx, b"timed out\n", 0);
        return;
    }
    log(&format!(
        "[net] dgram {}.{}.{}.{} {}",
        ip[0], ip[1], ip[2], ip[3], port
    ));
    reply(rx, b"ok\n", 0);
}

fn datagram_recv(rx: Cap, engine: &mut Engine, record: &Record, held: u8, rest: &[u8]) {
    let Some(name) = one_name(rest) else {
        reply(rx, b"bad\n", 0);
        return;
    };
    // Read receives from the stored peer. Write does not capture.
    let Some((ip, port)) = dgram_peer(record, held, name) else {
        reply(rx, b"access denied\n", 0);
        return;
    };
    if !engine.udp_arm(ip, port, DGRAM_SPORT) {
        reply(rx, b"access denied\n", 0);
        return;
    }
    let start = clock_ms();
    loop {
        let now = clock_ms();
        if now.saturating_sub(start) >= 3_000 {
            reply(rx, b"timed out\n", 0);
            return;
        }
        pump(engine);
        engine.poll(now);
        pump(engine);
        if let Some(payload) = engine.udp_take() {
            log(&format!(
                "[net] dgram in {}.{}.{}.{} {}",
                ip[0], ip[1], ip[2], ip[3], port
            ));
            reply(rx, &payload, 0);
            return;
        }
        let _ = sleep_ms(20);
    }
}

/// Read on a datagram card names one peer. Port 53 is not that peer.
fn dgram_peer(record: &Record, held: u8, name: &[u8]) -> Option<([u8; 4], u16)> {
    if held & 1 == 0 {
        return None;
    }
    let entry = record.find(name)?;
    if entry.kind != KIND_DGRAM || entry.port == 0 || entry.port == 53 {
        return None;
    }
    let ip = [entry.ip[0], entry.ip[1], entry.ip[2], entry.ip[3]];
    if ip == [0, 0, 0, 0] || ip == [255, 255, 255, 255] {
        return None;
    }
    Some((ip, entry.port))
}

fn name_and_body(rest: &[u8]) -> Option<(&[u8], &[u8])> {
    let name = one_name(rest)?;
    let at = 1 + name.len();
    if rest.len() < at + 1 {
        return None;
    }
    let plen = rest[at] as usize;
    if plen == 0 || rest.len() < at + 1 + plen {
        return None;
    }
    Some((name, &rest[at + 1..at + 1 + plen]))
}

fn ping(rx: Cap, engine: &mut Engine, record: &Record, held: u8, rest: &[u8], now: u64) {
    if held & 1 == 0 {
        reply(rx, b"access denied\n", 0);
        return;
    }
    let dst = if rest.len() == 4 {
        [rest[0], rest[1], rest[2], rest[3]]
    } else {
        let Some(name) = dns_rest(rest) else {
            reply(rx, b"bad\n", 0);
            return;
        };
        match lookup_dns(engine, record, name, now) {
            Ok(ip) => ip,
            Err(why) => {
                reply_lookup(rx, why);
                return;
            }
        }
    };
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

/// One TLS session whose plaintext is a lane. `net` copies bytes. The
/// SSH service never sees TCP.
struct Live {
    tcp: u8,
    lane: Cap,
    conn: Connection,
    to_lane: Vec<u8>,
    to_tls: Vec<u8>,
    wire: Vec<u8>,
}

fn stream(
    rx: Cap,
    engine: &mut Engine,
    record: &mut Record,
    held: u8,
    actor_id: u64,
    gates: &mut Vec<Gate>,
    rest: &[u8],
    lives: &mut Vec<Live>,
    seen: &mut Option<Seen>,
) {
    if rest.is_empty() {
        reply(rx, b"bad\n", 0);
        return;
    }
    let dir = rest[0];
    let name = &rest[1..];
    if name.is_empty() || parse_passed(name).is_some() {
        reply(rx, b"bad\n", 0);
        return;
    }
    if dir == 3 || dir == 4 {
        if held & 1 == 0 {
            reply(rx, b"access denied\n", 0);
            return;
        }
        if dir == 4 {
            *seen = None;
            reply(rx, b"ok\n", 0);
            return;
        }
        let Some(got) = seen.take() else {
            reply(rx, b"bad\n", 0);
            return;
        };
        if got.name.as_slice() != name || record.remember_key(name, got.key).is_err() {
            reply(rx, b"bad\n", 0);
            return;
        }
        save_record(record);
        audit(actor_id, name, &got.key, "remembered");
        reply(rx, b"ok\n", 0);
        return;
    }
    let Some(entry) = record.find(name) else {
        reply(rx, b"missing\n", 0);
        return;
    };
    if dir == 2 {
        if held & 2 == 0 {
            reply(rx, b"access denied\n", 0);
            return;
        }
        live_accept(
            rx, engine, actor_id, gates, name, entry.port, entry.key, lives,
        );
    } else if dir == 1 {
        if held & 1 == 0 {
            reply(rx, b"access denied\n", 0);
            return;
        }
        if entry.kind != KIND_IPV4 {
            reply(rx, b"unsupported\n", 0);
            return;
        }
        let ip = [entry.ip[0], entry.ip[1], entry.ip[2], entry.ip[3]];
        live_dial(
            rx, engine, actor_id, name, ip, entry.port, entry.key, lives, seen,
        );
    } else {
        reply(rx, b"bad\n", 0);
    }
}

fn live_accept(
    rx: Cap,
    engine: &mut Engine,
    actor_id: u64,
    gates: &mut Vec<Gate>,
    name: &[u8],
    port: u16,
    key: [u8; 32],
    lives: &mut Vec<Live>,
) {
    if lives
        .iter()
        .any(|live| engine.tcp_local(live.tcp) == Some(port))
    {
        reply(rx, b"held\n", 0);
        return;
    }
    if let Some(gate) = gates.iter().find(|g| g.port == port) {
        if gate.listener != actor_id {
            reply(rx, b"held\n", 0);
            return;
        }
    } else if gates.len() >= MAX_GATES {
        reply(rx, b"full\n", 0);
        return;
    } else {
        let Ok(id) = engine.tcp_listen(port) else {
            reply(rx, b"unavailable\n", 0);
            return;
        };
        gates.push(Gate {
            port,
            id,
            listener: actor_id,
            pending: Vec::new(),
        });
        log(&format!("[net] listen {port}"));
        reply(rx, b"attached\n", 0);
        return;
    }
    let start = clock_ms();
    let id = match next_inbound(engine, gates, port, start) {
        Ok(id) => id,
        Err(why) => {
            audit(actor_id, name, &key, why);
            reply(rx, b"timed out\n", 0);
            return;
        }
    };
    match tls_ready(engine, id, true, &key, clock_ms()) {
        Ok((conn, early)) => {
            if !hand_live(rx, engine, actor_id, name, &key, id, conn, early, lives) {
                return;
            }
        }
        Err(why) => {
            engine.tcp_close(id);
            audit(actor_id, name, &key, why);
            reply(rx, b"timed out\n", 0);
        }
    }
}

fn live_dial(
    rx: Cap,
    engine: &mut Engine,
    actor_id: u64,
    name: &[u8],
    ip: [u8; 4],
    port: u16,
    key: [u8; 32],
    lives: &mut Vec<Live>,
    seen: &mut Option<Seen>,
) {
    if port == 0 || ip == [0, 0, 0, 0] {
        reply(rx, b"access denied\n", 0);
        return;
    }
    if key == [0; 32] {
        learn_pin(rx, engine, actor_id, name, ip, port, seen);
        return;
    }
    log(&format!(
        "[net] dial {}.{}.{}.{}",
        ip[0], ip[1], ip[2], ip[3]
    ));
    let Ok(id) = engine.tcp_connect(ip, port) else {
        reply(rx, b"unavailable\n", 0);
        return;
    };
    let now = clock_ms();
    if !wait_established(engine, id, now) {
        engine.tcp_release(id);
        reply(rx, b"timed out\n", 0);
        return;
    }
    match tls_ready(engine, id, false, &key, clock_ms()) {
        Ok((conn, early)) => {
            if !hand_live(rx, engine, actor_id, name, &key, id, conn, early, lives) {
                return;
            }
        }
        Err(why) => {
            engine.tcp_release(id);
            audit(actor_id, name, &key, why);
            if why == "changed" {
                reply(rx, b"changed\n", 0);
            } else {
                reply(rx, b"timed out\n", 0);
            }
        }
    }
}

/// Finishes TLS far enough to read the peer key, then closes. The lane
/// stays closed until the seat accepts the key and dials again.
fn learn_pin(
    rx: Cap,
    engine: &mut Engine,
    actor_id: u64,
    name: &[u8],
    ip: [u8; 4],
    port: u16,
    seen: &mut Option<Seen>,
) {
    log(&format!(
        "[net] dial {}.{}.{}.{}",
        ip[0], ip[1], ip[2], ip[3]
    ));
    let Ok(id) = engine.tcp_connect(ip, port) else {
        reply(rx, b"unavailable\n", 0);
        return;
    };
    let now = clock_ms();
    if !wait_established(engine, id, now) {
        engine.tcp_release(id);
        reply(rx, b"timed out\n", 0);
        return;
    }
    let learned = Arc::new(LearnVerifier {
        key: Mutex::new([0; 32]),
        saw: Mutex::new(false),
        provider: trust::provider(),
    });
    let result = learn_handshake(engine, id, Arc::clone(&learned), clock_ms());
    engine.tcp_close(id);
    let Ok(()) = result else {
        reply(rx, b"timed out\n", 0);
        return;
    };
    let saw = learned.saw.lock().map(|g| *g).unwrap_or(false);
    let key = learned.key.lock().map(|g| *g).unwrap_or([0; 32]);
    if !saw || key == [0; 32] {
        reply(rx, b"timed out\n", 0);
        return;
    }
    *seen = Some(Seen {
        name: name.to_vec(),
        key,
    });
    audit(actor_id, name, &key, "unseen");
    let mut text = [0u8; 7 + 64 + 1];
    text[..7].copy_from_slice(b"unseen ");
    const HEX: &[u8] = b"0123456789abcdef";
    for (i, byte) in key.iter().enumerate() {
        text[7 + i * 2] = HEX[(byte >> 4) as usize];
        text[8 + i * 2] = HEX[(byte & 0xf) as usize];
    }
    text[71] = b'\n';
    reply(rx, &text, 0);
}

fn learn_handshake(
    engine: &mut Engine,
    id: u8,
    learned: Arc<LearnVerifier>,
    start: u64,
) -> Result<(), &'static str> {
    let server = ServerName::try_from("echo".to_string()).map_err(|_| "bad")?;
    let config = learn_config(learned).map_err(|_| "bad")?;
    let mut conn = Connection::Client(ClientConnection::new(config, server).map_err(|_| "bad")?);
    let mut early = Vec::new();
    let limit = start.saturating_add(HANDSHAKE_MS);
    while clock_ms() < limit {
        drive_tls(engine, id, &mut conn, &mut early)?;
        if !conn.is_handshaking() {
            return Ok(());
        }
        pump(engine);
        let _ = sleep_ms(20);
    }
    Err("deadline")
}

fn learn_config(learned: Arc<LearnVerifier>) -> Result<Arc<ClientConfig>, Error> {
    let provider = Arc::clone(&learned.provider);
    let config = ClientConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13])?
        .dangerous()
        .with_custom_certificate_verifier(learned)
        .with_no_client_auth();
    Ok(Arc::new(config))
}

fn hand_live(
    rx: Cap,
    engine: &mut Engine,
    actor_id: u64,
    name: &[u8],
    key: &[u8; 32],
    tcp: u8,
    conn: Connection,
    early: Vec<u8>,
    lives: &mut Vec<Live>,
) -> bool {
    let mut pair = [0u64; 2];
    if !lane_pair(&mut pair).ok {
        engine.tcp_close(tcp);
        reply(rx, b"unavailable\n", 0);
        return false;
    }
    lives.push(Live {
        tcp,
        lane: Cap::from_bits(pair[0]),
        conn,
        to_lane: early,
        to_tls: Vec::new(),
        wire: Vec::new(),
    });
    audit(actor_id, name, key, "stream");
    reply(rx, b"ok\n", pair[1]);
    true
}

fn tls_ready(
    engine: &mut Engine,
    id: u8,
    server: bool,
    key: &[u8; 32],
    start: u64,
) -> Result<(Connection, Vec<u8>), &'static str> {
    let mut conn = if server {
        let config = server_config().map_err(|_| "bad")?;
        Connection::Server(ServerConnection::new(config).map_err(|_| "bad")?)
    } else {
        let server = ServerName::try_from("echo".to_string()).map_err(|_| "bad")?;
        let config = client_config(key).map_err(|_| "bad")?;
        Connection::Client(ClientConnection::new(config, server).map_err(|_| "bad")?)
    };
    let mut early = Vec::new();
    let limit = start.saturating_add(HANDSHAKE_MS);
    while clock_ms() < limit {
        drive_tls(engine, id, &mut conn, &mut early)?;
        if !conn.is_handshaking() {
            return Ok((conn, early));
        }
        pump(engine);
        let _ = sleep_ms(20);
    }
    Err("deadline")
}

fn pump_lives(engine: &mut Engine, lives: &mut Vec<Live>) {
    let mut i = 0;
    while i < lives.len() {
        if step_live(engine, &mut lives[i]) {
            i += 1;
        } else {
            let live = lives.swap_remove(i);
            engine.tcp_close(live.tcp);
            let _ = close(live.lane);
        }
    }
}

fn step_live(engine: &mut Engine, live: &mut Live) -> bool {
    if !engine.tcp_established(live.tcp) && live.to_lane.is_empty() {
        return false;
    }
    let mut pulled = Vec::new();
    if drive_tls(engine, live.tcp, &mut live.conn, &mut pulled).is_err() {
        return false;
    }
    live.to_lane.append(&mut pulled);
    if !live.to_lane.is_empty() {
        let got = write_poll(live.lane, &live.to_lane);
        if got.ok {
            let n = (got.value as usize).min(live.to_lane.len());
            live.to_lane.drain(..n);
        } else if SysError::from_code(got.value) != SysError::NoResource {
            return false;
        }
    }
    let mut buf = [0u8; 1024];
    let got = read_poll(live.lane, &mut buf);
    if got.ok {
        if got.value == 0 {
            return false;
        }
        live.to_tls.extend_from_slice(&buf[..got.value as usize]);
    } else if SysError::from_code(got.value) != SysError::NoResource {
        return false;
    }
    if !live.to_tls.is_empty() {
        let n = live.conn.writer().write(&live.to_tls).unwrap_or(0);
        live.to_tls.drain(..n);
    }
    while live.conn.wants_write() {
        let mut out = Vec::new();
        if live.conn.write_tls(&mut out).is_err() {
            return false;
        }
        live.wire.append(&mut out);
    }
    let mut at = 0;
    while at < live.wire.len() {
        match engine.tcp_send(live.tcp, &live.wire[at..]) {
            Ok(0) => break,
            Err(()) => return false,
            Ok(n) => at += n,
        }
    }
    if at > 0 {
        live.wire.drain(..at);
        engine.poll(clock_ms());
    }
    true
}

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
