//! Ethernet, ARP, IPv4, ICMP echo, UDP, DHCP, and a small TCP client/listener.
//!
//! The kernel moves frames. This crate is what the `net` service runs,
//! and what the host tests drive without QEMU. Checksums are computed
//! here. There is no DNS client: a lookup with no pinned resolver fails
//! closed.

#![cfg_attr(not(test), no_std)]
extern crate alloc;

use alloc::vec;
use alloc::vec::Vec;
use galexy_crypto::sha256;

const UDP_PROOF_PORT: u16 = 4580;
const TCP_PROOF_PORT: u16 = 4581;

/// One direction of a TCP byte stream, and the frame queue.
const BUF: usize = 8 * 1024;
const CONNS: usize = 16;
const ARP_SLOTS: usize = 8;

/// A frame the service should put on the wire.
pub type Frame = Vec<u8>;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Dhcp {
    Idle,
    WaitOffer,
    WaitAck,
    Bound,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum TcpState {
    Free,
    Listen,
    SynSent,
    SynRecv,
    Established,
    Closed,
}

struct ArpEnt {
    ip: [u8; 4],
    mac: [u8; 6],
    used: bool,
}

struct Conn {
    state: TcpState,
    local: u16,
    remote: u16,
    dst: [u8; 4],
    snd_una: u32,
    snd_nxt: u32,
    rcv_nxt: u32,
    rx: Vec<u8>,
    rx_len: usize,
    tx: Vec<u8>,
    tx_len: usize,
    tx_off: usize,
    last_tx_ms: u64,
    accepted: bool,
    /// One segment that arrived ahead of `rcv_nxt`. Empty when none.
    ooo_seq: u32,
    ooo: Vec<u8>,
}

impl Conn {
    fn blank() -> Self {
        Self {
            state: TcpState::Free,
            local: 0,
            remote: 0,
            dst: [0; 4],
            snd_una: 0,
            snd_nxt: 0,
            rcv_nxt: 0,
            rx: vec![0; BUF],
            rx_len: 0,
            tx: vec![0; BUF],
            tx_len: 0,
            tx_off: 0,
            last_tx_ms: 0,
            accepted: false,
            ooo_seq: 0,
            ooo: Vec::new(),
        }
    }
}

/// The packet engine. Feed frames in, pull frames out.
pub struct Engine {
    mac: [u8; 6],
    ask: bool,
    addr: Option<[u8; 4]>,
    prefix: u8,
    gateway: Option<[u8; 4]>,
    arp: [ArpEnt; ARP_SLOTS],
    dhcp: Dhcp,
    xid: u32,
    dhcp_ms: u64,
    udp_sent: bool,
    tcp_proof: bool,
    tcp_proof_logged: bool,
    tcp_proof_closed: bool,
    lease_secs: u32,
    isn: u32,
    next_port: u16,
    conns: [Conn; CONNS],
    egress: Vec<Frame>,
    pending_udp: Option<PendingUdp>,
    udp_inbox: Option<UdpInbox>,
    echo_seq: u16,
    pending_icmp: Option<PendingIcmp>,
    await_echo: Option<AwaitEcho>,
    echo_from: Option<[u8; 4]>,
}

struct PendingUdp {
    dst: [u8; 4],
    dport: u16,
    sport: u16,
    payload: Vec<u8>,
}

/// One datagram the card is willing to take. Anything else is dropped.
struct UdpInbox {
    peer: [u8; 4],
    peer_port: u16,
    local_port: u16,
    payload: Option<Vec<u8>>,
}

struct PendingIcmp {
    dst: [u8; 4],
    id: u16,
    seq: u16,
    payload: Vec<u8>,
}

struct AwaitEcho {
    dst: [u8; 4],
    id: u16,
    seq: u16,
}

impl Engine {
    /// `ask` sends DHCP discover. Fixed mode waits for [`Self::set_fixed`].
    pub fn new(mac: [u8; 6], ask: bool) -> Self {
        Self {
            mac,
            ask,
            addr: None,
            prefix: 0,
            gateway: None,
            arp: [const {
                ArpEnt {
                    ip: [0; 4],
                    mac: [0; 6],
                    used: false,
                }
            }; ARP_SLOTS],
            dhcp: if ask { Dhcp::Idle } else { Dhcp::Bound },
            xid: 0x6761_6C65,
            dhcp_ms: 0,
            udp_sent: false,
            tcp_proof: false,
            tcp_proof_logged: false,
            tcp_proof_closed: false,
            lease_secs: 0,
            isn: 1000,
            next_port: 40000,
            conns: core::array::from_fn(|_| Conn::blank()),
            egress: Vec::new(),
            pending_udp: None,
            udp_inbox: None,
            echo_seq: 0,
            pending_icmp: None,
            await_echo: None,
            echo_from: None,
        }
    }

    /// Ask DHCP for an IPv4 address. No packet until the next [`Self::poll`].
    ///
    /// A locator that is already known is left alone. A discover that is
    /// already in flight is not restarted.
    pub fn start_dhcp(&mut self) {
        if self.locator().is_some() {
            return;
        }
        self.ask = true;
        if !matches!(self.dhcp, Dhcp::WaitOffer | Dhcp::WaitAck) {
            self.dhcp = Dhcp::Idle;
            self.dhcp_ms = 0;
        }
    }

    /// Fixed mode locator. Does not send DHCP.
    pub fn set_fixed(&mut self, addr: [u8; 4], prefix: u8, gateway: [u8; 4]) {
        self.ask = false;
        self.addr = Some(addr);
        self.prefix = prefix;
        self.gateway = Some(gateway);
        self.dhcp = Dhcp::Bound;
    }

    /// Our address, prefix, and gateway once known.
    pub fn locator(&self) -> Option<([u8; 4], u8, [u8; 4])> {
        Some((self.addr?, self.prefix, self.gateway?))
    }

    /// DHCP option 51, in seconds. `0` when the ack did not carry one.
    pub fn lease_secs(&self) -> u32 {
        self.lease_secs
    }

    /// True after the proof UDP datagram was queued.
    pub fn udp_sent(&self) -> bool {
        self.udp_sent
    }

    /// True after the proof TCP handshake reached established.
    pub fn tcp_ready(&self) -> bool {
        self.tcp_proof_logged
    }

    /// Drive timers and the proof packets. `now_ms` is monotonic.
    pub fn poll(&mut self, now_ms: u64) {
        if self.ask && self.dhcp == Dhcp::Idle && now_ms.saturating_sub(self.dhcp_ms) >= 200 {
            self.send_dhcp(1, None, None);
            self.dhcp = Dhcp::WaitOffer;
            self.dhcp_ms = now_ms;
        } else if self.ask
            && matches!(self.dhcp, Dhcp::WaitOffer | Dhcp::WaitAck)
            && now_ms.saturating_sub(self.dhcp_ms) >= 1000
        {
            self.dhcp = Dhcp::Idle;
            self.dhcp_ms = now_ms;
        }
        self.flush_pending();
        if self.locator().is_some() && !self.udp_sent {
            if let Some(gw) = self.gateway {
                if self
                    .udp_send(gw, UDP_PROOF_PORT, UDP_PROOF_PORT, b"galexy")
                    .is_ok()
                    && self.pending_udp.is_none()
                {
                    self.udp_sent = true;
                }
            }
        }
        if self.udp_sent && !self.tcp_proof {
            if let Some(gw) = self.gateway {
                if self.tcp_connect(gw, TCP_PROOF_PORT).is_ok() {
                    self.tcp_proof = true;
                }
            }
        }
        if !self.tcp_proof_logged
            && self
                .conns
                .iter()
                .any(|c| c.remote == TCP_PROOF_PORT && c.state == TcpState::Established)
        {
            self.tcp_proof_logged = true;
        }
        if self.tcp_proof_logged && !self.tcp_proof_closed {
            if let Some(id) = self
                .conns
                .iter()
                .position(|c| c.remote == TCP_PROOF_PORT && c.state == TcpState::Established)
            {
                self.tcp_close(id as u8);
                self.conns[id].state = TcpState::Free;
                self.tcp_proof_closed = true;
            }
        }
        self.retransmit(now_ms);
    }

    /// Parse one Ethernet frame.
    pub fn ingest(&mut self, frame: &[u8]) {
        if frame.len() < 14 {
            return;
        }
        let eth = u16::from_be_bytes([frame[12], frame[13]]);
        if eth == 0x0806 {
            self.ingest_arp(frame);
        } else if eth == 0x0800 {
            self.ingest_ip(frame);
        }
    }

    /// Next frame to transmit.
    pub fn pop(&mut self) -> Option<Frame> {
        if self.egress.is_empty() {
            None
        } else {
            Some(self.egress.remove(0))
        }
    }

    /// Queue a UDP datagram. `Err` when we have no address yet.
    ///
    /// An off-subnet peer is reached through the gateway. The IP
    /// destination stays `dst`. A broadcast is not a next hop.
    #[allow(clippy::result_unit_err)]
    pub fn udp_send(
        &mut self,
        dst: [u8; 4],
        dport: u16,
        sport: u16,
        payload: &[u8],
    ) -> Result<(), ()> {
        let Some(src) = self.addr else {
            return Err(());
        };
        let broadcast = dst == [255, 255, 255, 255];
        let hop = if broadcast { dst } else { self.next_hop(dst) };
        if !broadcast && self.lookup(hop).is_none() {
            self.pending_udp = Some(PendingUdp {
                dst,
                dport,
                sport,
                payload: payload.to_vec(),
            });
            self.send_arp(hop);
            return Ok(());
        }
        let mac = self.lookup(hop).unwrap_or([0xff; 6]);
        self.egress
            .push(udp_frame(self.mac, mac, src, dst, sport, dport, payload));
        Ok(())
    }

    /// True when a datagram is not waiting on neighbor discovery.
    pub fn udp_idle(&self) -> bool {
        self.pending_udp.is_none()
    }

    /// Arm a receive for one stored peer. Port 53 is refused. A later
    /// datagram from any other address or port is not queued.
    pub fn udp_arm(&mut self, peer: [u8; 4], peer_port: u16, local_port: u16) -> bool {
        if peer == [0, 0, 0, 0]
            || peer == [255, 255, 255, 255]
            || peer_port == 0
            || peer_port == 53
            || local_port == 0
            || local_port == 53
        {
            self.udp_inbox = None;
            return false;
        }
        self.udp_inbox = Some(UdpInbox {
            peer,
            peer_port,
            local_port,
            payload: None,
        });
        true
    }

    /// Take the one datagram the arm accepted, if it has arrived.
    pub fn udp_take(&mut self) -> Option<Vec<u8>> {
        let ready = self.udp_inbox.as_ref().is_some_and(|w| w.payload.is_some());
        if !ready {
            return None;
        }
        self.udp_inbox.take().and_then(|w| w.payload)
    }

    /// Drop a receive arm. A late datagram is not delivered later.
    pub fn udp_disarm(&mut self) {
        self.udp_inbox = None;
    }

    /// Queue an ICMP echo request. `Err` when we have no address, or the
    /// payload is longer than 64 bytes. An off-subnet peer is reached
    /// through the gateway. The IP destination stays `dst`. A missing
    /// neighbor of that next hop is ARP, and the request leaves on the
    /// next poll. One request is outstanding.
    #[allow(clippy::result_unit_err)]
    pub fn icmp_echo(&mut self, dst: [u8; 4], payload: &[u8]) -> Result<(), ()> {
        if self.addr.is_none() || payload.len() > 64 {
            return Err(());
        }
        self.echo_seq = self.echo_seq.wrapping_add(1).max(1);
        let id = 0x6761;
        let seq = self.echo_seq;
        self.await_echo = Some(AwaitEcho { dst, id, seq });
        self.echo_from = None;
        let hop = self.next_hop(dst);
        if self.lookup(hop).is_none() {
            self.pending_icmp = Some(PendingIcmp {
                dst,
                id,
                seq,
                payload: payload.to_vec(),
            });
            self.send_arp(hop);
            return Ok(());
        }
        self.pending_icmp = None;
        self.emit_echo(dst, id, seq, payload);
        Ok(())
    }

    /// The source of an echo reply that matched the outstanding request.
    pub fn take_echo(&mut self) -> Option<[u8; 4]> {
        self.echo_from.take()
    }

    /// Active open. Returns a connection id.
    #[allow(clippy::result_unit_err)]
    pub fn tcp_connect(&mut self, dst: [u8; 4], dport: u16) -> Result<u8, ()> {
        if self.addr.is_none() {
            return Err(());
        }
        let id = self.alloc_conn()?;
        let sport = self.next_port;
        self.next_port = self.next_port.wrapping_add(1).max(40000);
        let isn = self.isn;
        self.isn = self.isn.wrapping_add(64_000);
        let c = &mut self.conns[id as usize];
        c.state = TcpState::SynSent;
        c.local = sport;
        c.remote = dport;
        c.dst = dst;
        c.snd_una = isn;
        c.snd_nxt = isn;
        c.last_tx_ms = 0;
        self.send_syn(id, true);
        Ok(id)
    }

    /// Listen on `port`. One listener.
    #[allow(clippy::result_unit_err)]
    pub fn tcp_listen(&mut self, port: u16) -> Result<u8, ()> {
        let id = self.alloc_conn()?;
        let c = &mut self.conns[id as usize];
        c.state = TcpState::Listen;
        c.local = port;
        Ok(id)
    }

    /// A listener connection that has finished the handshake.
    pub fn take_accepted(&mut self) -> Option<u8> {
        let id = self
            .conns
            .iter()
            .position(|c| c.accepted && c.state == TcpState::Established)?;
        self.conns[id].accepted = false;
        Some(id as u8)
    }

    /// True when the handshake is done.
    pub fn tcp_established(&self, id: u8) -> bool {
        self.conns
            .get(id as usize)
            .is_some_and(|c| c.state == TcpState::Established)
    }

    /// Local port of a connection that is still in the table.
    pub fn tcp_local(&self, id: u8) -> Option<u16> {
        let c = self.conns.get(id as usize)?;
        if c.state == TcpState::Free || c.state == TcpState::Closed {
            None
        } else {
            Some(c.local)
        }
    }

    /// Queue bytes. They leave on the next poll once the handshake is done.
    #[allow(clippy::result_unit_err)]
    pub fn tcp_send(&mut self, id: u8, bytes: &[u8]) -> Result<usize, ()> {
        let c = self.conns.get_mut(id as usize).ok_or(())?;
        if c.state != TcpState::Established {
            return Err(());
        }
        let room = BUF - c.tx_len;
        let n = bytes.len().min(room);
        c.tx[c.tx_len..c.tx_len + n].copy_from_slice(&bytes[..n]);
        c.tx_len += n;
        Ok(n)
    }

    /// Copy received bytes out of the connection.
    pub fn tcp_recv(&mut self, id: u8, dst: &mut [u8]) -> usize {
        let n = {
            let Some(c) = self.conns.get_mut(id as usize) else {
                return 0;
            };
            let n = dst.len().min(c.rx_len);
            dst[..n].copy_from_slice(&c.rx[..n]);
            if n > 0 {
                c.rx.copy_within(n..c.rx_len, 0);
                c.rx_len -= n;
            }
            n
        };
        if n > 0 {
            self.drain_held(id);
        }
        n
    }

    /// Drop a connection that never finished, or FIN one that did.
    /// The slot can be reused.
    pub fn tcp_release(&mut self, id: u8) {
        if self.tcp_established(id) {
            self.tcp_close(id);
            return;
        }
        if let Some(c) = self.conns.get_mut(id as usize) {
            if c.state != TcpState::Free {
                c.state = TcpState::Closed;
            }
        }
    }

    /// Send FIN.
    pub fn tcp_close(&mut self, id: u8) {
        if self.tcp_established(id) {
            self.emit_tcp(id, 0x11, &[]);
            if let Some(c) = self.conns.get_mut(id as usize) {
                c.state = TcpState::Closed;
            }
        }
    }

    fn alloc_conn(&mut self) -> Result<u8, ()> {
        let id = self
            .conns
            .iter()
            .position(|c| c.state == TcpState::Free || c.state == TcpState::Closed)
            .ok_or(())?;
        self.conns[id] = Conn::blank();
        Ok(id as u8)
    }

    fn flush_pending(&mut self) {
        if self.pending_udp.as_ref().is_some_and(|p| {
            let hop = if p.dst == [255, 255, 255, 255] {
                p.dst
            } else {
                self.next_hop(p.dst)
            };
            self.lookup(hop).is_some()
        }) {
            let p = self.pending_udp.take().unwrap();
            let _ = self.udp_send(p.dst, p.dport, p.sport, &p.payload);
        }
        self.flush_icmp();
    }

    fn flush_icmp(&mut self) {
        let Some(p) = self.pending_icmp.as_ref() else {
            return;
        };
        if self.lookup(self.next_hop(p.dst)).is_none() {
            return;
        }
        let p = self.pending_icmp.take().unwrap();
        self.emit_echo(p.dst, p.id, p.seq, &p.payload);
    }

    fn emit_echo(&mut self, dst: [u8; 4], id: u16, seq: u16, payload: &[u8]) {
        let Some(src) = self.addr else {
            return;
        };
        let Some(mac) = self.lookup(self.next_hop(dst)) else {
            return;
        };
        let icmp = echo_message(8, id, seq, payload);
        self.egress
            .push(ipv4_frame(self.mac, mac, src, dst, 1, &icmp));
    }

    /// On-link addresses resolve themselves. Anything else uses the gateway
    /// as the next hop. The IP destination stays the address the caller passed.
    fn next_hop(&self, ip: [u8; 4]) -> [u8; 4] {
        let Some(src) = self.addr else {
            return ip;
        };
        let Some(gw) = self.gateway else {
            return ip;
        };
        let bits = u32::from(self.prefix.min(32));
        if bits == 0 {
            return gw;
        }
        let mask = if bits >= 32 {
            u32::MAX
        } else {
            u32::MAX << (32 - bits)
        };
        let local = u32::from_be_bytes(src) & mask;
        let dest = u32::from_be_bytes(ip) & mask;
        if local == dest {
            ip
        } else {
            gw
        }
    }

    fn lookup(&self, ip: [u8; 4]) -> Option<[u8; 6]> {
        self.arp
            .iter()
            .find(|e| e.used && e.ip == ip)
            .map(|e| e.mac)
    }

    fn learn(&mut self, ip: [u8; 4], mac: [u8; 6]) {
        if let Some(e) = self.arp.iter_mut().find(|e| e.used && e.ip == ip) {
            e.mac = mac;
            return;
        }
        if let Some(e) = self.arp.iter_mut().find(|e| !e.used) {
            *e = ArpEnt {
                ip,
                mac,
                used: true,
            };
            return;
        }
        self.arp[0] = ArpEnt {
            ip,
            mac,
            used: true,
        };
    }

    fn send_arp(&mut self, target: [u8; 4]) {
        let Some(src) = self.addr else {
            return;
        };
        let mut f = vec![0xff; 6];
        f.extend_from_slice(&self.mac);
        f.extend_from_slice(&[0x08, 0x06]);
        f.extend_from_slice(&[0x00, 0x01, 0x08, 0x00, 6, 4, 0x00, 0x01]);
        f.extend_from_slice(&self.mac);
        f.extend_from_slice(&src);
        f.extend_from_slice(&[0; 6]);
        f.extend_from_slice(&target);
        self.egress.push(f);
    }

    fn ingest_arp(&mut self, frame: &[u8]) {
        if frame.len() < 42 {
            return;
        }
        let op = u16::from_be_bytes([frame[20], frame[21]]);
        let mut smac = [0u8; 6];
        let mut sip = [0u8; 4];
        let mut tip = [0u8; 4];
        smac.copy_from_slice(&frame[22..28]);
        sip.copy_from_slice(&frame[28..32]);
        tip.copy_from_slice(&frame[38..42]);
        self.learn(sip, smac);
        if op == 1 && self.addr == Some(tip) {
            let mut f = Vec::with_capacity(42);
            f.extend_from_slice(&smac);
            f.extend_from_slice(&self.mac);
            f.extend_from_slice(&[0x08, 0x06, 0x00, 0x01, 0x08, 0x00, 6, 4, 0x00, 0x02]);
            f.extend_from_slice(&self.mac);
            f.extend_from_slice(&tip);
            f.extend_from_slice(&smac);
            f.extend_from_slice(&sip);
            self.egress.push(f);
        }
        if op == 2 {
            self.flush_pending();
        }
    }

    fn send_dhcp(&mut self, msg: u8, offer: Option<[u8; 4]>, server: Option<[u8; 4]>) {
        let mut p = vec![1, 1, 6, 0];
        p.extend_from_slice(&self.xid.to_be_bytes());
        // secs, broadcast flag, ciaddr. The flag asks for a broadcast offer
        // so the reply is received before this host has an address.
        p.extend_from_slice(&[0, 0, 0x80, 0, 0, 0, 0, 0]);
        if let Some(ip) = offer {
            p.extend_from_slice(&ip);
        } else {
            p.extend_from_slice(&[0; 4]);
        }
        p.extend_from_slice(&[0; 8]);
        p.extend_from_slice(&self.mac);
        p.extend_from_slice(&[0; 10 + 64 + 128]);
        p.extend_from_slice(&[0x63, 0x82, 0x53, 0x63]);
        p.extend_from_slice(&[53, 1, msg]);
        if let Some(ip) = offer {
            p.extend_from_slice(&[50, 4]);
            p.extend_from_slice(&ip);
        }
        if let Some(server) = server {
            p.extend_from_slice(&[54, 4]);
            p.extend_from_slice(&server);
        }
        p.extend_from_slice(&[55, 2, 1, 3, 255]);
        let frame = udp_frame(
            self.mac,
            [0xff; 6],
            self.addr.unwrap_or([0; 4]),
            [255, 255, 255, 255],
            68,
            67,
            &p,
        );
        self.egress.push(frame);
    }

    fn ingest_ip(&mut self, frame: &[u8]) {
        if frame.len() < 14 + 20 {
            return;
        }
        let ip = &frame[14..];
        if ip[0] >> 4 != 4 {
            return;
        }
        let ihl = (ip[0] & 0x0f) as usize * 4;
        if ihl < 20 || ip.len() < ihl {
            return;
        }
        let total = u16::from_be_bytes([ip[2], ip[3]]) as usize;
        if total < ihl {
            return;
        }
        // Padding and a second frame sit past the IP length. A short
        // capture stops early. Either one splices the TLS flight if it
        // is treated as payload.
        let have = ip.len().min(total);
        let ip = &ip[..have];
        let src = [ip[12], ip[13], ip[14], ip[15]];
        let dst = [ip[16], ip[17], ip[18], ip[19]];
        if src != [0, 0, 0, 0] && src != [255, 255, 255, 255] {
            let mut smac = [0u8; 6];
            smac.copy_from_slice(&frame[6..12]);
            if smac != [0, 0, 0, 0, 0, 0] && smac != [0xff, 0xff, 0xff, 0xff, 0xff, 0xff] {
                self.learn(src, smac);
            }
        }
        if let Some(ours) = self.addr {
            if dst != ours && dst != [255, 255, 255, 255] {
                return;
            }
        }
        let proto = ip[9];
        let body = &ip[ihl..];
        if proto == 1 {
            self.ingest_icmp(src, body);
        } else if proto == 17 {
            self.ingest_udp(src, body);
        } else if proto == 6 {
            self.ingest_tcp(src, body);
        }
    }

    fn ingest_icmp(&mut self, src: [u8; 4], body: &[u8]) {
        if body.len() < 8 || body.len() > 512 || checksum(body) != 0 || body[1] != 0 {
            return;
        }
        if body[0] == 8 {
            self.reply_echo(src, body);
        } else if body[0] == 0 {
            let id = u16::from_be_bytes([body[4], body[5]]);
            let seq = u16::from_be_bytes([body[6], body[7]]);
            let matched = self
                .await_echo
                .as_ref()
                .is_some_and(|w| w.dst == src && w.id == id && w.seq == seq);
            if matched {
                self.await_echo = None;
                self.echo_from = Some(src);
            }
        }
    }

    fn reply_echo(&mut self, dst: [u8; 4], request: &[u8]) {
        let Some(src) = self.addr else {
            return;
        };
        let Some(mac) = self.lookup(dst) else {
            return;
        };
        let mut icmp = request.to_vec();
        icmp[0] = 0;
        icmp[2] = 0;
        icmp[3] = 0;
        let sum = checksum(&icmp);
        icmp[2] = (sum >> 8) as u8;
        icmp[3] = sum as u8;
        self.egress
            .push(ipv4_frame(self.mac, mac, src, dst, 1, &icmp));
    }

    fn ingest_udp(&mut self, src: [u8; 4], body: &[u8]) {
        if body.len() < 8 {
            return;
        }
        let sport = u16::from_be_bytes([body[0], body[1]]);
        let dport = u16::from_be_bytes([body[2], body[3]]);
        if dport == 68 && sport == 67 {
            self.ingest_dhcp(src, &body[8..]);
            return;
        }
        // Port 53 is not a card datagram. Neither is any peer we did not arm.
        if sport == 53 || dport == 53 {
            return;
        }
        let Some(wait) = self.udp_inbox.as_mut() else {
            return;
        };
        if src != wait.peer || sport != wait.peer_port || dport != wait.local_port {
            return;
        }
        let payload = &body[8..];
        if payload.is_empty() || payload.len() > 64 || wait.payload.is_some() {
            return;
        }
        wait.payload = Some(payload.to_vec());
    }

    fn ingest_dhcp(&mut self, _src: [u8; 4], p: &[u8]) {
        if p.len() < 240 || p[236..240] != [0x63, 0x82, 0x53, 0x63] {
            return;
        }
        if p[4..8] != self.xid.to_be_bytes() {
            return;
        }
        let mut msg = 0u8;
        let mut server = [0u8; 4];
        let mut mask = None;
        let mut router = None;
        let mut lease_secs = 0u32;
        let mut i = 240;
        while i < p.len() && p[i] != 255 {
            let code = p[i];
            if i + 1 >= p.len() {
                return;
            }
            let len = p[i + 1] as usize;
            if i + 2 + len > p.len() {
                return;
            }
            let val = &p[i + 2..i + 2 + len];
            if code == 53 && len == 1 {
                msg = val[0];
            } else if code == 54 && len == 4 {
                server.copy_from_slice(val);
            } else if code == 1 && len == 4 {
                let mut m = [0u8; 4];
                m.copy_from_slice(val);
                mask = Some(m);
            } else if code == 3 && len >= 4 {
                let mut g = [0u8; 4];
                g.copy_from_slice(&val[..4]);
                router = Some(g);
            } else if code == 51 && len == 4 {
                lease_secs = u32::from_be_bytes([val[0], val[1], val[2], val[3]]);
            }
            i += 2 + len;
        }
        let mut yi = [0u8; 4];
        yi.copy_from_slice(&p[16..20]);
        if msg == 2 && self.dhcp == Dhcp::WaitOffer {
            self.send_dhcp(3, Some(yi), Some(server));
            self.dhcp = Dhcp::WaitAck;
        } else if msg == 5 && self.dhcp == Dhcp::WaitAck {
            self.addr = Some(yi);
            self.prefix = mask.map(prefix_of).unwrap_or(24);
            self.gateway = router.or(Some(server));
            self.lease_secs = lease_secs;
            self.dhcp = Dhcp::Bound;
            self.ask = false;
        }
    }

    fn ingest_tcp(&mut self, src: [u8; 4], seg: &[u8]) {
        if seg.len() < 20 {
            return;
        }
        let sport = u16::from_be_bytes([seg[0], seg[1]]);
        let dport = u16::from_be_bytes([seg[2], seg[3]]);
        let seq = u32::from_be_bytes([seg[4], seg[5], seg[6], seg[7]]);
        let ack = u32::from_be_bytes([seg[8], seg[9], seg[10], seg[11]]);
        let off = (seg[12] >> 4) as usize * 4;
        if seg.len() < off {
            return;
        }
        let flags = seg[13];
        let payload = &seg[off..];
        if let Some(id) = self.find_conn(dport, sport, src) {
            self.on_seg(id, seq, ack, flags, payload);
            return;
        }
        if flags & 0x02 != 0 {
            if let Some(id) = self.find_listener(dport) {
                self.accept_syn(id, src, sport, seq);
            }
        }
    }

    fn find_conn(&self, local: u16, remote: u16, dst: [u8; 4]) -> Option<u8> {
        self.conns
            .iter()
            .position(|c| {
                c.state != TcpState::Free
                    && c.state != TcpState::Listen
                    && c.local == local
                    && c.remote == remote
                    && c.dst == dst
            })
            .map(|i| i as u8)
    }

    fn find_listener(&self, port: u16) -> Option<u8> {
        self.conns
            .iter()
            .position(|c| c.state == TcpState::Listen && c.local == port)
            .map(|i| i as u8)
    }

    fn accept_syn(&mut self, listen: u8, src: [u8; 4], sport: u16, seq: u32) {
        let Ok(id) = self.alloc_conn() else {
            return;
        };
        let local = self.conns[listen as usize].local;
        let isn = self.isn;
        self.isn = self.isn.wrapping_add(64_000);
        let c = &mut self.conns[id as usize];
        c.state = TcpState::SynRecv;
        c.local = local;
        c.remote = sport;
        c.dst = src;
        c.snd_una = isn;
        c.snd_nxt = isn;
        c.rcv_nxt = seq.wrapping_add(1);
        self.send_syn(id, false);
    }

    fn on_seg(&mut self, id: u8, seq: u32, ack: u32, flags: u8, payload: &[u8]) {
        let state = self.conns[id as usize].state;
        if flags & 0x04 != 0 {
            self.conns[id as usize].state = TcpState::Closed;
            return;
        }
        if state == TcpState::SynSent
            && flags & 0x12 == 0x12
            && ack == self.conns[id as usize].snd_nxt
        {
            self.conns[id as usize].snd_una = ack;
            self.conns[id as usize].rcv_nxt = seq.wrapping_add(1);
            self.conns[id as usize].state = TcpState::Established;
            self.emit_tcp(id, 0x10, &[]);
            return;
        }
        if state == TcpState::SynRecv && flags & 0x10 != 0 && ack == self.conns[id as usize].snd_nxt
        {
            self.conns[id as usize].snd_una = ack;
            self.conns[id as usize].state = TcpState::Established;
            self.conns[id as usize].accepted = true;
        }
        if self.conns[id as usize].state != TcpState::Established {
            return;
        }
        if ack != self.conns[id as usize].snd_una && flags & 0x10 != 0 {
            let c = &mut self.conns[id as usize];
            let advanced = ack.wrapping_sub(c.snd_una) as usize;
            if advanced <= c.tx_off {
                c.tx.copy_within(advanced..c.tx_len, 0);
                c.tx_len -= advanced;
                c.tx_off -= advanced;
            }
            c.snd_una = ack;
        }
        if !payload.is_empty() {
            self.accept_payload(id, seq, payload);
        }
    }

    /// In-order bytes, the new tail of an overlap, or one segment held
    /// across a hole. `rcv_nxt` moves only by bytes kept.
    fn accept_payload(&mut self, id: u8, seq: u32, payload: &[u8]) {
        let rcv = self.conns[id as usize].rcv_nxt;
        let end = seq.wrapping_add(payload.len() as u32);
        if !seq_lt(rcv, end) {
            self.emit_tcp(id, 0x10, &[]);
            return;
        }
        let (seq, owned) = if seq_lt(seq, rcv) {
            let skip = rcv.wrapping_sub(seq) as usize;
            (rcv, payload[skip..].to_vec())
        } else {
            (seq, payload.to_vec())
        };
        if seq == self.conns[id as usize].rcv_nxt {
            self.take_in_order(id, &owned);
            self.drain_held(id);
        } else {
            self.hold_ooo(id, seq, &owned);
        }
        self.emit_tcp(id, 0x10, &[]);
    }

    fn take_in_order(&mut self, id: u8, payload: &[u8]) {
        let (n, rest_seq, rest) = {
            let c = &mut self.conns[id as usize];
            let room = BUF - c.rx_len;
            let n = payload.len().min(room);
            if n > 0 {
                c.rx[c.rx_len..c.rx_len + n].copy_from_slice(&payload[..n]);
                c.rx_len += n;
                c.rcv_nxt = c.rcv_nxt.wrapping_add(n as u32);
            }
            (n, c.rcv_nxt, payload[n..].to_vec())
        };
        if n < payload.len() {
            self.hold_ooo(id, rest_seq, &rest);
        }
    }

    /// Keep the segment closest to the hole. One slot is enough for the
    /// middle piece of a three-segment flight to wait for the first.
    fn hold_ooo(&mut self, id: u8, seq: u32, payload: &[u8]) {
        if payload.is_empty() {
            return;
        }
        let c = &mut self.conns[id as usize];
        if !c.ooo.is_empty() && !seq_lt(seq, c.ooo_seq) {
            return;
        }
        c.ooo_seq = seq;
        c.ooo.clear();
        let n = payload.len().min(BUF);
        c.ooo.extend_from_slice(&payload[..n]);
    }

    fn drain_held(&mut self, id: u8) {
        loop {
            let c = &self.conns[id as usize];
            if c.ooo.is_empty() {
                return;
            }
            let seq = c.ooo_seq;
            let rcv = c.rcv_nxt;
            let end = seq.wrapping_add(c.ooo.len() as u32);
            if !seq_lt(rcv, end) {
                self.conns[id as usize].ooo.clear();
                return;
            }
            if seq_lt(rcv, seq) {
                return;
            }
            let skip = rcv.wrapping_sub(seq) as usize;
            let rest = self.conns[id as usize].ooo[skip..].to_vec();
            self.conns[id as usize].ooo.clear();
            if BUF == self.conns[id as usize].rx_len {
                self.hold_ooo(id, rcv, &rest);
                return;
            }
            self.take_in_order(id, &rest);
        }
    }

    fn send_syn(&mut self, id: u8, active: bool) {
        let flags = if active { 0x02 } else { 0x12 };
        self.emit_tcp(id, flags, &[]);
        let c = &mut self.conns[id as usize];
        c.snd_nxt = c.snd_nxt.wrapping_add(1);
    }

    fn emit_tcp(&mut self, id: u8, flags: u8, payload: &[u8]) {
        let Some(src_ip) = self.addr else {
            return;
        };
        let (seq, ack, src_port, dst_port, dst) = {
            let c = &self.conns[id as usize];
            (c.snd_nxt, c.rcv_nxt, c.local, c.remote, c.dst)
        };
        let hop = self.next_hop(dst);
        let dst_mac = self.lookup(hop).unwrap_or([0xff; 6]);
        // MSS 1460. A larger segment does not fit one virtio receive
        // buffer, so the flight never arrives.
        let mss = [2u8, 4, 0x05, 0xb4];
        let opts: &[u8] = if flags & 0x02 != 0 { &mss } else { &[] };
        let hdr = 20 + opts.len();
        let mut tcp = Vec::with_capacity(hdr + payload.len());
        tcp.extend_from_slice(&src_port.to_be_bytes());
        tcp.extend_from_slice(&dst_port.to_be_bytes());
        tcp.extend_from_slice(&seq.to_be_bytes());
        tcp.extend_from_slice(&ack.to_be_bytes());
        tcp.push(((hdr / 4) as u8) << 4);
        tcp.push(flags);
        tcp.extend_from_slice(&8192u16.to_be_bytes());
        tcp.extend_from_slice(&[0, 0, 0, 0]);
        tcp.extend_from_slice(opts);
        tcp.extend_from_slice(payload);
        stamp_tcp(&mut tcp, src_ip, dst);
        let frame = ipv4_frame(self.mac, dst_mac, src_ip, dst, 6, &tcp);
        self.egress.push(frame);
        self.conns[id as usize].last_tx_ms = 0;
    }

    fn retransmit(&mut self, now_ms: u64) {
        for id in 0..CONNS {
            let c = &self.conns[id];
            if c.state != TcpState::Established || c.tx_off >= c.tx_len {
                continue;
            }
            if c.last_tx_ms != 0 && now_ms.saturating_sub(c.last_tx_ms) < 200 {
                continue;
            }
            let payload = {
                let c = &self.conns[id];
                let n = (c.tx_len - c.tx_off).min(1000);
                c.tx[c.tx_off..c.tx_off + n].to_vec()
            };
            let n = payload.len();
            self.emit_tcp(id as u8, 0x18, &payload);
            let c = &mut self.conns[id];
            c.snd_nxt = c.snd_nxt.wrapping_add(n as u32);
            c.tx_off += n;
            c.last_tx_ms = now_ms;
        }
    }
}

fn seq_lt(a: u32, b: u32) -> bool {
    (a.wrapping_sub(b) as i32) < 0
}

fn prefix_of(mask: [u8; 4]) -> u8 {
    let v = u32::from_be_bytes(mask);
    v.leading_ones() as u8
}

fn udp_frame(
    smac: [u8; 6],
    dmac: [u8; 6],
    src: [u8; 4],
    dst: [u8; 4],
    sport: u16,
    dport: u16,
    payload: &[u8],
) -> Frame {
    let mut udp = Vec::with_capacity(8 + payload.len());
    udp.extend_from_slice(&sport.to_be_bytes());
    udp.extend_from_slice(&dport.to_be_bytes());
    udp.extend_from_slice(&((8 + payload.len()) as u16).to_be_bytes());
    udp.extend_from_slice(&[0, 0]);
    udp.extend_from_slice(payload);
    let sum = transport_sum(src, dst, 17, &udp);
    udp[6] = (sum >> 8) as u8;
    udp[7] = sum as u8;
    ipv4_frame(smac, dmac, src, dst, 17, &udp)
}

fn ipv4_frame(
    smac: [u8; 6],
    dmac: [u8; 6],
    src: [u8; 4],
    dst: [u8; 4],
    proto: u8,
    body: &[u8],
) -> Frame {
    let mut f = Vec::with_capacity(14 + 20 + body.len());
    f.extend_from_slice(&dmac);
    f.extend_from_slice(&smac);
    f.extend_from_slice(&[0x08, 0x00]);
    let total = (20 + body.len()) as u16;
    f.extend_from_slice(&[0x45, 0, 0, 0, 0, 1, 0, 0, 64, proto, 0, 0]);
    f[16] = (total >> 8) as u8;
    f[17] = total as u8;
    f.extend_from_slice(&src);
    f.extend_from_slice(&dst);
    let sum = checksum(&f[14..34]);
    f[24] = (sum >> 8) as u8;
    f[25] = sum as u8;
    f.extend_from_slice(body);
    f
}

fn stamp_tcp(tcp: &mut [u8], src: [u8; 4], dst: [u8; 4]) {
    let sum = transport_sum(src, dst, 6, tcp);
    tcp[16] = (sum >> 8) as u8;
    tcp[17] = sum as u8;
}

fn transport_sum(src: [u8; 4], dst: [u8; 4], proto: u8, seg: &[u8]) -> u16 {
    let mut pseudo = Vec::with_capacity(12 + seg.len());
    pseudo.extend_from_slice(&src);
    pseudo.extend_from_slice(&dst);
    pseudo.push(0);
    pseudo.push(proto);
    pseudo.extend_from_slice(&(seg.len() as u16).to_be_bytes());
    pseudo.extend_from_slice(seg);
    checksum(&pseudo)
}

fn echo_message(kind: u8, id: u16, seq: u16, payload: &[u8]) -> Vec<u8> {
    let mut icmp = Vec::with_capacity(8 + payload.len());
    icmp.extend_from_slice(&[kind, 0, 0, 0]);
    icmp.extend_from_slice(&id.to_be_bytes());
    icmp.extend_from_slice(&seq.to_be_bytes());
    icmp.extend_from_slice(payload);
    let sum = checksum(&icmp);
    icmp[2] = (sum >> 8) as u8;
    icmp[3] = sum as u8;
    icmp
}

fn checksum(data: &[u8]) -> u16 {
    let mut sum = 0u32;
    let mut i = 0;
    while i + 1 < data.len() {
        sum += u16::from_be_bytes([data[i], data[i + 1]]) as u32;
        i += 2;
    }
    if i < data.len() {
        sum += (data[i] as u32) << 8;
    }
    while sum >> 16 != 0 {
        sum = (sum & 0xffff) + (sum >> 16);
    }
    !(sum as u16)
}

/// Audit line. The fingerprint is the first eight bytes of SHA-256(key),
/// hex. The line does not contain the key or a session secret.
pub fn audit_line(actor: u64, name: &str, key: &[u8; 32], result: &str) -> alloc::string::String {
    let dig = sha256(key);
    let mut fp = alloc::string::String::new();
    for b in &dig[..8] {
        fp.push(hex(b >> 4));
        fp.push(hex(b & 0xf));
    }
    alloc::format!("dial actor={actor} name={name} fp={fp} result={result}")
}

fn hex(n: u8) -> char {
    b"0123456789abcdef"[n as usize] as char
}

/// Builds an ICMPv6 neighbor solicitation. Host tests only.
pub fn neighbor_solicitation(src_mac: [u8; 6], src_ip: [u8; 16], target: [u8; 16]) -> Frame {
    let mut icmp = Vec::new();
    icmp.extend_from_slice(&[135, 0, 0, 0, 0, 0, 0, 0]);
    icmp.extend_from_slice(&target);
    icmp.extend_from_slice(&[1, 1]);
    icmp.extend_from_slice(&src_mac);
    let sum = icmpv6_sum(&src_ip, &solicited_node(&target), &icmp);
    icmp[2] = (sum >> 8) as u8;
    icmp[3] = sum as u8;
    let mut f = Vec::new();
    f.extend_from_slice(&multicast_mac(&target));
    f.extend_from_slice(&src_mac);
    f.extend_from_slice(&[0x86, 0xdd]);
    f.extend_from_slice(&[0x60, 0, 0, 0]);
    f.extend_from_slice(&(icmp.len() as u16).to_be_bytes());
    f.extend_from_slice(&[58, 255]);
    f.extend_from_slice(&src_ip);
    f.extend_from_slice(&solicited_node(&target));
    f.extend_from_slice(&icmp);
    f
}

/// The target address of a neighbor solicitation, if `frame` is one.
pub fn parse_neighbor_solicitation(frame: &[u8]) -> Option<[u8; 16]> {
    if frame.len() < 14 + 40 + 24 || frame[12] != 0x86 || frame[13] != 0xdd {
        return None;
    }
    if frame[20] != 58 || frame[54] != 135 {
        return None;
    }
    let mut target = [0u8; 16];
    target.copy_from_slice(&frame[62..78]);
    Some(target)
}

fn solicited_node(target: &[u8; 16]) -> [u8; 16] {
    let mut ip = [0u8; 16];
    ip[0] = 0xff;
    ip[1] = 0x02;
    ip[11] = 0x01;
    ip[12] = 0xff;
    ip[13] = target[13];
    ip[14] = target[14];
    ip[15] = target[15];
    ip
}

fn multicast_mac(target: &[u8; 16]) -> [u8; 6] {
    [0x33, 0x33, 0xff, target[13], target[14], target[15]]
}

fn icmpv6_sum(src: &[u8; 16], dst: &[u8; 16], icmp: &[u8]) -> u16 {
    let mut p = Vec::new();
    p.extend_from_slice(src);
    p.extend_from_slice(dst);
    p.extend_from_slice(&(icmp.len() as u32).to_be_bytes());
    p.extend_from_slice(&[0, 0, 0, 58]);
    p.extend_from_slice(icmp);
    checksum(&p)
}

/// DNS is fail-closed until a resolver is pinned. This phase has none.
#[allow(clippy::result_unit_err)]
pub fn lookup_name(_name: &str) -> Result<(), ()> {
    Err(())
}

/// UDP port of one Network Time Protocol query. Not 53.
pub const NTP_PORT: u16 = 123;

/// Seconds from 1900-01-01 to 1970-01-01. There is no leap smear.
const NTP_UNIX: u64 = 2_208_988_800;

/// One client query. `nonce` is the transmit timestamp and must come
/// back as the origin timestamp. It is not a key and not civil time.
pub fn ntp_request(nonce: [u8; 8]) -> [u8; 48] {
    let mut pkt = [0u8; 48];
    // LI = 0, version 4, mode 3 (client).
    pkt[0] = 0x23;
    pkt[40..48].copy_from_slice(&nonce);
    pkt
}

/// Unix seconds from one server reply, or `None` when it is not that reply.
///
/// The mode is server, the version is 3 or 4, the stratum is not a
/// kiss-of-death, the leap indicator is synchronized, and the origin
/// timestamp is `nonce`. A transmit field of zero is not a time. A field
/// from before 1970 is the next 32-bit era, which starts in 2036. The
/// caller still refuses a year outside 1970..=2099.
pub fn ntp_unix(packet: &[u8], nonce: &[u8; 8]) -> Option<u64> {
    if packet.len() < 48 || nonce == &[0; 8] {
        return None;
    }
    let li_vn_mode = packet[0];
    let mode = li_vn_mode & 0x07;
    let version = (li_vn_mode >> 3) & 0x07;
    let leap = li_vn_mode >> 6;
    if mode != 4 || (version != 3 && version != 4) || leap == 3 {
        return None;
    }
    if packet[1] == 0 || &packet[24..32] != nonce {
        return None;
    }
    let secs = u32::from_be_bytes([packet[40], packet[41], packet[42], packet[43]]);
    if secs == 0 {
        return None;
    }
    let ntp = u64::from(secs);
    if ntp >= NTP_UNIX {
        Some(ntp - NTP_UNIX)
    } else {
        Some(ntp + (1u64 << 32) - NTP_UNIX)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn offer(xid: u32, yi: [u8; 4]) -> Vec<u8> {
        let mut p = vec![0u8; 240];
        p[0] = 2;
        p[4..8].copy_from_slice(&xid.to_be_bytes());
        p[16..20].copy_from_slice(&yi);
        p[236..240].copy_from_slice(&[0x63, 0x82, 0x53, 0x63]);
        p.extend_from_slice(&[
            53, 1, 2, 1, 4, 255, 255, 255, 0, 3, 4, 10, 0, 2, 2, 54, 4, 10, 0, 2, 2, 255,
        ]);
        udp_frame(
            [0; 6],
            [0; 6],
            [10, 0, 2, 2],
            [255, 255, 255, 255],
            67,
            68,
            &p,
        )
    }

    #[test]
    fn quiet_until_start_dhcp() {
        let mac = [0x52, 0x54, 0x00, 0x12, 0x34, 0x56];
        let mut e = Engine::new(mac, false);
        e.poll(1_000);
        e.poll(5_000);
        assert!(e.pop().is_none());
        assert!(e.locator().is_none());
        e.start_dhcp();
        e.poll(5_000);
        let discover = e.pop().unwrap();
        assert_eq!(&discover[12..14], &[0x08, 0x00]);
        assert!(e.locator().is_none());
    }

    #[test]
    fn fixed_mode_skips_dhcp() {
        let mut e = Engine::new([2; 6], false);
        e.set_fixed([10, 0, 2, 15], 24, [10, 0, 2, 2]);
        e.poll(1000);
        assert!(e
            .pop()
            .is_some_and(|f| f.len() >= 14 && f[12] == 0x08 && f[13] == 0x06));
        assert!(e.pop().is_none());
    }

    #[test]
    fn dhcp_then_udp_payload() {
        let mac = [0x52, 0x54, 0x00, 0x12, 0x34, 0x56];
        let mut e = Engine::new(mac, true);
        e.poll(500);
        let discover = e.pop().unwrap();
        assert_eq!(&discover[12..14], &[0x08, 0x00]);
        e.ingest(&offer(0x6761_6C65, [10, 0, 2, 15]));
        let request = e.pop().unwrap();
        assert!(request.windows(3).any(|w| w == [53, 1, 3]));
        let mut ack = offer(0x6761_6C65, [10, 0, 2, 15]);
        // message type 5
        let at = ack.windows(3).position(|w| w == [53, 1, 2]).unwrap();
        ack[at + 2] = 5;
        e.ingest(&ack);
        assert_eq!(e.locator().unwrap().0, [10, 0, 2, 15]);
        e.poll(600);
        let arp = e.pop().unwrap();
        assert_eq!(&arp[12..14], &[0x08, 0x06]);
        let mut reply = arp.clone();
        reply[21] = 2;
        reply[22..28].copy_from_slice(&[0x52, 0x55, 0, 0, 0, 1]);
        reply[28..32].copy_from_slice(&[10, 0, 2, 2]);
        e.ingest(&reply);
        let udp = e.pop().unwrap();
        assert!(udp.ends_with(b"galexy"));
        assert_ne!(&udp[udp.len() - 8 - 6..udp.len() - 8], &[0, 0]);
    }

    #[test]
    fn udp_off_subnet_keeps_the_peer_address() {
        let mut e = Engine::new([1; 6], false);
        e.set_fixed([10, 0, 2, 15], 24, [10, 0, 2, 2]);
        e.udp_send([172, 30, 0, 2], 4597, 4598, b"card").unwrap();
        let arp = e.pop().unwrap();
        assert_eq!(&arp[12..14], &[0x08, 0x06]);
        assert_eq!(&arp[38..42], &[10, 0, 2, 2]);
        e.learn([10, 0, 2, 2], [9; 6]);
        e.poll(1);
        let udp = e.pop().unwrap();
        assert_eq!(&udp[..6], &[9; 6]);
        assert_eq!(&udp[30..34], &[172, 30, 0, 2]);
        assert!(udp.ends_with(b"card"));
        assert_ne!(&udp[30..34], &[10, 0, 2, 2]);
    }

    #[test]
    fn udp_arm_takes_only_the_stored_peer() {
        let mut e = Engine::new([1; 6], false);
        e.set_fixed([10, 0, 2, 15], 24, [10, 0, 2, 2]);
        assert!(!e.udp_arm([172, 30, 0, 2], 53, 4598));
        assert!(e.udp_arm([172, 30, 0, 2], 4602, 4598));
        let other = udp_frame(
            [9; 6],
            [1; 6],
            [9, 9, 9, 9],
            [10, 0, 2, 15],
            4602,
            4598,
            b"nope",
        );
        e.ingest(&other);
        assert!(e.udp_take().is_none());
        let dns = udp_frame(
            [9; 6],
            [1; 6],
            [172, 30, 0, 2],
            [10, 0, 2, 15],
            53,
            4598,
            b"dns",
        );
        e.ingest(&dns);
        assert!(e.udp_take().is_none());
        let frame = udp_frame(
            [9; 6],
            [1; 6],
            [172, 30, 0, 2],
            [10, 0, 2, 15],
            4602,
            4598,
            b"card-back",
        );
        e.ingest(&frame);
        assert_eq!(e.udp_take().as_deref(), Some(&b"card-back"[..]));
        assert!(e.udp_take().is_none());
    }

    #[test]
    fn ntp_reply_from_another_address_is_ignored() {
        let mut e = Engine::new([1; 6], false);
        e.set_fixed([10, 0, 2, 15], 24, [10, 0, 2, 2]);
        let nonce = [9, 8, 7, 6, 5, 4, 3, 2];
        let query = ntp_request(nonce);
        assert_eq!(query[0], 0x23);
        assert_eq!(&query[40..48], &nonce);
        assert!(e.udp_arm([10, 0, 2, 2], NTP_PORT, 4123));
        let mut reply = [0u8; 48];
        reply[0] = 0x24;
        reply[1] = 1;
        reply[24..32].copy_from_slice(&nonce);
        let unix = 1_781_524_800u64;
        let ntp = (unix + 2_208_988_800) as u32;
        reply[40..44].copy_from_slice(&ntp.to_be_bytes());
        let other = udp_frame(
            [9; 6],
            [1; 6],
            [9, 9, 9, 9],
            [10, 0, 2, 15],
            NTP_PORT,
            4123,
            &reply,
        );
        e.ingest(&other);
        assert!(e.udp_take().is_none());
        assert!(ntp_unix(&reply, &nonce).is_some());
        let dns = udp_frame(
            [9; 6],
            [1; 6],
            [10, 0, 2, 2],
            [10, 0, 2, 15],
            53,
            4123,
            &reply,
        );
        e.ingest(&dns);
        assert!(e.udp_take().is_none());
        let frame = udp_frame(
            [9; 6],
            [1; 6],
            [10, 0, 2, 2],
            [10, 0, 2, 15],
            NTP_PORT,
            4123,
            &reply,
        );
        e.ingest(&frame);
        let got = e.udp_take().expect("reply from the queried address");
        assert_eq!(ntp_unix(&got, &nonce), Some(unix));
        assert!(ntp_unix(&got, &[1; 8]).is_none());
        let mut kiss = reply;
        kiss[1] = 0;
        assert!(ntp_unix(&kiss, &nonce).is_none());
        // 2036-02-07 06:28:17 UTC is the second after the 32-bit wrap.
        let mut wrapped = reply;
        wrapped[40..44].copy_from_slice(&1u32.to_be_bytes());
        assert_eq!(ntp_unix(&wrapped, &nonce), Some(2_085_978_497));
        e.udp_disarm();
        e.ingest(&frame);
        assert!(e.udp_take().is_none());
    }

    #[test]
    fn tcp_handshake_between_two_engines() {
        let mut a = Engine::new([1; 6], false);
        a.set_fixed([10, 0, 0, 1], 24, [10, 0, 0, 2]);
        a.learn([10, 0, 0, 2], [2; 6]);
        let mut b = Engine::new([2; 6], false);
        b.set_fixed([10, 0, 0, 2], 24, [10, 0, 0, 1]);
        b.learn([10, 0, 0, 1], [1; 6]);
        b.tcp_listen(4590).unwrap();
        a.tcp_connect([10, 0, 0, 2], 4590).unwrap();
        for _ in 0..6 {
            while let Some(f) = a.pop() {
                b.ingest(&f);
            }
            while let Some(f) = b.pop() {
                a.ingest(&f);
            }
        }
        assert!(a.tcp_ready() || a.conns_established_for_test());
        let id = a.client_id_for_test();
        a.tcp_send(id, b"hi").unwrap();
        a.poll(1000);
        while let Some(f) = a.pop() {
            b.ingest(&f);
        }
        while let Some(f) = b.pop() {
            a.ingest(&f);
        }
        let acc = b.take_accepted().unwrap();
        assert_eq!(b.tcp_local(acc), Some(4590));
        let mut buf = [0u8; 8];
        let n = b.tcp_recv(acc, &mut buf);
        assert_eq!(&buf[..n], b"hi");
    }

    #[test]
    fn icmp_off_subnet_keeps_the_peer_address() {
        let mut e = Engine::new([1; 6], false);
        e.set_fixed([10, 0, 2, 15], 24, [10, 0, 2, 2]);
        e.icmp_echo([172, 30, 0, 2], b"galexy").unwrap();
        let arp = e.pop().unwrap();
        assert_eq!(&arp[12..14], &[0x08, 0x06]);
        assert_eq!(&arp[38..42], &[10, 0, 2, 2]);
        e.learn([10, 0, 2, 2], [9; 6]);
        e.poll(1);
        let echo = e.pop().unwrap();
        assert_eq!(&echo[..6], &[9; 6]);
        assert_eq!(&echo[30..34], &[172, 30, 0, 2]);
        assert_eq!(echo[23], 1);
        assert!(echo.windows(6).any(|w| w == b"galexy"));
    }

    #[test]
    fn icmp_echo_round_trip() {
        let mut a = Engine::new([1; 6], false);
        a.set_fixed([10, 0, 2, 15], 24, [10, 0, 2, 2]);
        a.learn([10, 0, 2, 2], [2; 6]);
        let mut b = Engine::new([2; 6], false);
        b.set_fixed([10, 0, 2, 2], 24, [10, 0, 2, 15]);
        a.icmp_echo([10, 0, 2, 2], b"galexy").unwrap();
        let req = a.pop().unwrap();
        assert_eq!(req[23], 1);
        assert_eq!(req[34], 8);
        assert!(req.ends_with(b"galexy"));
        assert_eq!(checksum(&req[34..]), 0);
        b.ingest(&req);
        let rep = b.pop().unwrap();
        assert_eq!(rep[34], 0);
        assert!(rep.ends_with(b"galexy"));
        a.ingest(&rep);
        assert_eq!(a.take_echo(), Some([10, 0, 2, 2]));
    }

    #[test]
    fn icmp_bad_reply_is_ignored() {
        let mut a = Engine::new([1; 6], false);
        a.set_fixed([10, 0, 2, 15], 24, [10, 0, 2, 2]);
        a.learn([10, 0, 2, 2], [2; 6]);
        a.icmp_echo([10, 0, 2, 2], b"galexy").unwrap();
        let req = a.pop().unwrap();
        let mut bad = req.clone();
        bad[34] = 0;
        bad[36] ^= 0xff;
        a.ingest(&bad);
        assert!(a.take_echo().is_none());
        let mut short = req[..34 + 4].to_vec();
        short[34] = 0;
        a.ingest(&short);
        assert!(a.take_echo().is_none());
        a.icmp_echo([10, 0, 2, 2], b"nope").unwrap();
        assert!(a.icmp_echo([10, 0, 2, 2], &[0; 65]).is_err());
        assert!(Engine::new([1; 6], true)
            .icmp_echo([10, 0, 2, 2], b"x")
            .is_err());
    }

    #[test]
    fn neighbor_solicitation_round_trip() {
        let src = [0xfe, 0x80, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1];
        let tgt = [0xfe, 0x80, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 2];
        let frame = neighbor_solicitation([0x52, 0x54, 0, 0, 0, 1], src, tgt);
        assert_eq!(parse_neighbor_solicitation(&frame), Some(tgt));
    }

    #[test]
    fn audit_line_hides_the_key() {
        let key = [0x11u8; 32];
        let line = audit_line(7, "echo", &key, "ok");
        assert!(line.contains("actor=7"));
        assert!(line.contains("name=echo"));
        assert!(line.contains("fp="));
        assert!(!line.contains("1111111111111111"));
        assert!(lookup_name("echo").is_err());
    }

    /// Cloudflare's DNS-over-TLS reply: 3263 bytes, record header
    /// `16 03 03 00 7a`. The rest is a pattern so a splice fails the compare.
    fn cloudflare_flight() -> Vec<u8> {
        let mut flight = vec![0u8; 3263];
        flight[..8].copy_from_slice(&[0x16, 0x03, 0x03, 0x00, 0x7a, 0x02, 0x00, 0x00]);
        for (i, b) in flight.iter_mut().enumerate().skip(8) {
            *b = (i.wrapping_mul(17)) as u8;
        }
        flight
    }

    struct Dot {
        engine: Engine,
        id: u8,
        rcv: u32,
        sport: u16,
        ack: u32,
    }

    fn dial_dot() -> Dot {
        let mut engine = Engine::new([1; 6], false);
        engine.set_fixed([10, 0, 2, 15], 24, [10, 0, 2, 2]);
        engine.learn([10, 0, 2, 2], [9; 6]);
        let id = engine.tcp_connect([1, 1, 1, 1], 853).unwrap();
        let syn = engine.pop().unwrap();
        assert_eq!(syn[46] >> 4, 6, "syn carries mss");
        assert_eq!(&syn[54..58], &[2, 4, 0x05, 0xb4]);
        let isn = u32::from_be_bytes([syn[38], syn[39], syn[40], syn[41]]);
        let sport = u16::from_be_bytes([syn[34], syn[35]]);
        let server_isn = 1_000_000u32;
        engine.ingest(&peer_tcp(
            sport,
            server_isn,
            isn.wrapping_add(1),
            0x12,
            &[],
            &[],
        ));
        assert!(engine.tcp_established(id));
        let _ = engine.pop();
        Dot {
            engine,
            id,
            rcv: server_isn.wrapping_add(1),
            sport,
            ack: isn.wrapping_add(1),
        }
    }

    fn peer_tcp(dport: u16, seq: u32, ack: u32, flags: u8, opts: &[u8], payload: &[u8]) -> Vec<u8> {
        let mut tcp = Vec::with_capacity(20 + opts.len() + payload.len());
        tcp.extend_from_slice(&853u16.to_be_bytes());
        tcp.extend_from_slice(&dport.to_be_bytes());
        tcp.extend_from_slice(&seq.to_be_bytes());
        tcp.extend_from_slice(&ack.to_be_bytes());
        let hdr = 20 + opts.len();
        assert_eq!(hdr % 4, 0);
        tcp.push(((hdr / 4) as u8) << 4);
        tcp.push(flags);
        tcp.extend_from_slice(&65535u16.to_be_bytes());
        tcp.extend_from_slice(&[0, 0, 0, 0]);
        tcp.extend_from_slice(opts);
        tcp.extend_from_slice(payload);
        stamp_tcp(&mut tcp, [1, 1, 1, 1], [10, 0, 2, 15]);
        ipv4_frame([9; 6], [1; 6], [1, 1, 1, 1], [10, 0, 2, 15], 6, &tcp)
    }

    fn feed(dot: &mut Dot, off: u32, bytes: &[u8]) {
        dot.engine.ingest(&peer_tcp(
            dot.sport,
            dot.rcv.wrapping_add(off),
            dot.ack,
            0x18,
            &[],
            bytes,
        ));
    }

    fn recv_all(engine: &mut Engine, id: u8) -> Vec<u8> {
        let mut out = Vec::new();
        let mut buf = [0u8; 2048];
        loop {
            let n = engine.tcp_recv(id, &mut buf);
            if n == 0 {
                break;
            }
            out.extend_from_slice(&buf[..n]);
        }
        out
    }

    #[test]
    fn dot_flight_three_segments_is_intact() {
        let mut dot = dial_dot();
        let flight = cloudflare_flight();
        feed(&mut dot, 0, &flight[..1460]);
        feed(&mut dot, 0, &flight[..1460]);
        feed(&mut dot, 1460, &flight[1460..2920]);
        feed(&mut dot, 2920, &flight[2920..]);
        let got = recv_all(&mut dot.engine, dot.id);
        assert_eq!(got.len(), 3263);
        assert_eq!(&got[..8], &[0x16, 0x03, 0x03, 0x00, 0x7a, 0x02, 0x00, 0x00]);
        assert_eq!(got, flight);
    }

    #[test]
    fn dot_flight_middle_segment_first_is_intact() {
        let mut dot = dial_dot();
        let flight = cloudflare_flight();
        feed(&mut dot, 1460, &flight[1460..2920]);
        feed(&mut dot, 0, &flight[..1460]);
        feed(&mut dot, 2920, &flight[2920..]);
        assert_eq!(recv_all(&mut dot.engine, dot.id), flight);
    }

    #[test]
    fn dot_flight_overlap_is_not_spliced() {
        let mut dot = dial_dot();
        let flight = cloudflare_flight();
        feed(&mut dot, 0, &flight[..1460]);
        feed(&mut dot, 1000, &flight[1000..2920]);
        feed(&mut dot, 2920, &flight[2920..]);
        assert_eq!(recv_all(&mut dot.engine, dot.id), flight);
    }

    #[test]
    fn bytes_past_ip_length_are_not_payload() {
        let mut dot = dial_dot();
        let flight = cloudflare_flight();
        let mut frame = peer_tcp(dot.sport, dot.rcv, dot.ack, 0x18, &[], &flight[..127]);
        frame.extend_from_slice(&[0x00, 0x03, 0xff, 0xff]);
        dot.engine.ingest(&frame);
        let got = recv_all(&mut dot.engine, dot.id);
        assert_eq!(got, flight[..127]);
        assert_eq!(&got[..4], &[0x16, 0x03, 0x03, 0x00]);
    }

    #[test]
    fn truncated_segment_then_overlap_fills_the_tail() {
        let mut dot = dial_dot();
        let flight = cloudflare_flight();
        let mut frame = peer_tcp(dot.sport, dot.rcv, dot.ack, 0x18, &[], &flight[..400]);
        frame.truncate(frame.len() - 80);
        dot.engine.ingest(&frame);
        assert_eq!(recv_all(&mut dot.engine, dot.id), flight[..320]);
        feed(&mut dot, 0, &flight[..400]);
        assert_eq!(recv_all(&mut dot.engine, dot.id), flight[320..400]);
    }

    #[test]
    fn tcp_timestamp_option_is_not_payload() {
        let mut dot = dial_dot();
        let flight = cloudflare_flight();
        let opts = [1u8, 1, 8, 10, 0, 0, 0, 1, 0, 0, 0, 2];
        let frame = peer_tcp(dot.sport, dot.rcv, dot.ack, 0x18, &opts, &flight[..180]);
        dot.engine.ingest(&frame);
        let got = recv_all(&mut dot.engine, dot.id);
        assert_eq!(&got[..4], &[0x16, 0x03, 0x03, 0x00]);
        assert_eq!(got, flight[..180]);
    }

    impl Engine {
        fn conns_established_for_test(&self) -> bool {
            self.conns.iter().any(|c| c.state == TcpState::Established)
        }
        fn client_id_for_test(&self) -> u8 {
            self.conns
                .iter()
                .position(|c| c.state == TcpState::Established)
                .unwrap() as u8
        }
    }
}
