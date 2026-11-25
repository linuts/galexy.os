//! Ethernet, ARP, IPv4, ICMP echo, UDP, DHCP, and a small TCP client/listener.
//!
//! The kernel moves frames. This crate is what the `net` service runs,
//! and what the host tests drive without QEMU. Checksums are computed
//! here. There is no DNS client: a lookup with no pinned resolver fails
//! closed.

#![cfg_attr(not(test), no_std)]
extern crate alloc;

mod v6;

use alloc::vec;
use alloc::vec::Vec;
use galexy_core::{link_local_from_mac, prefix_contains, prefix_ok, same_family, Ip};
use galexy_crypto::sha256;

/// Addresses stored for one family.
const PER_FAMILY: usize = 4;

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
    ip: Ip,
    mac: [u8; 6],
    used: bool,
}

struct Slot {
    ip: Ip,
    prefix: u8,
    expiry: u32,
    /// Generated link-local. The status line has no gateway.
    auto: bool,
}

impl Slot {
    const fn blank() -> Self {
        Self {
            ip: Ip::V4([0, 0, 0, 0]),
            prefix: 0,
            expiry: 0,
            auto: false,
        }
    }
}

/// Why a static address was refused. Nothing was sent.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StaticErr {
    /// The address and the gateway are different families, or the prefix
    /// does not fit.
    Bad,
    /// This family already has four addresses.
    Full,
    /// This family already has a different gateway.
    OneGateway,
}

struct Conn {
    state: TcpState,
    local: u16,
    remote: u16,
    dst: Ip,
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
            dst: Ip::V4([0, 0, 0, 0]),
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
    v4: [Slot; PER_FAMILY],
    v4n: u8,
    v4_gw: Option<Ip>,
    v6: [Slot; PER_FAMILY],
    v6n: u8,
    v6_gw: Option<Ip>,
    learn: bool,
    dad_sent: bool,
    rs_sent: bool,
    v6_dhcp: Dhcp,
    v6_ms: u64,
    v6_xid: [u8; 3],
    v6_req: [u8; 3],
    v6_server: Vec<u8>,
    v6_offer: Option<[u8; 16]>,
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
    echo_from: Option<Ip>,
}

struct PendingUdp {
    dst: Ip,
    dport: u16,
    sport: u16,
    payload: Vec<u8>,
}

/// One datagram the card is willing to take. Anything else is dropped.
struct UdpInbox {
    peer: Ip,
    peer_port: u16,
    local_port: u16,
    payload: Option<Vec<u8>>,
}

struct PendingIcmp {
    dst: Ip,
    id: u16,
    seq: u16,
    payload: Vec<u8>,
}

struct AwaitEcho {
    dst: Ip,
    id: u16,
    seq: u16,
}

impl Engine {
    /// `ask` sends DHCP discover. Fixed mode waits for [`Self::set_fixed`].
    pub fn new(mac: [u8; 6], ask: bool) -> Self {
        Self {
            mac,
            ask,
            v4: [const { Slot::blank() }; PER_FAMILY],
            v4n: 0,
            v4_gw: None,
            v6: [const { Slot::blank() }; PER_FAMILY],
            v6n: 0,
            v6_gw: None,
            learn: false,
            dad_sent: false,
            rs_sent: false,
            v6_dhcp: Dhcp::Bound,
            v6_ms: 0,
            v6_xid: [0x67, 0x61, 0x6c],
            v6_req: [0, 0, 0],
            v6_server: Vec::new(),
            v6_offer: None,
            arp: [const {
                ArpEnt {
                    ip: Ip::V4([0, 0, 0, 0]),
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

    /// Ask for an address on an empty list. No packet until the next
    /// [`Self::poll`]. Installs the link-local address, then DHCPv4,
    /// one router solicitation, and DHCPv6. A list that already holds
    /// an address is left alone.
    pub fn start_dhcp(&mut self) {
        if !self.is_empty() {
            return;
        }
        self.install_link_local();
        self.ask = true;
        self.learn = true;
        self.dad_sent = false;
        self.rs_sent = false;
        self.dhcp = Dhcp::Idle;
        self.dhcp_ms = 0;
        self.v6_dhcp = Dhcp::Idle;
        self.v6_ms = 0;
    }

    /// True when neither family has an address.
    pub fn is_empty(&self) -> bool {
        self.v4n == 0 && self.v6n == 0
    }

    /// Fixed IPv4 locator. Does not send DHCP and does not add IPv6.
    pub fn set_fixed(&mut self, addr: [u8; 4], prefix: u8, gateway: [u8; 4]) {
        self.ask = false;
        self.learn = false;
        self.v4n = 1;
        self.v4[0] = Slot {
            ip: Ip::V4(addr),
            prefix,
            expiry: 0,
            auto: false,
        };
        self.v4_gw = Some(Ip::V4(gateway));
        self.dhcp = Dhcp::Bound;
    }

    /// Installs one static address and that family's gateway.
    ///
    /// A global IPv6 address also installs the MAC link-local. A
    /// link-local address does not. The same gateway adds the address.
    /// A different gateway is [`StaticErr::OneGateway`]. Nothing is sent.
    pub fn add_static(&mut self, addr: Ip, prefix: u8, gateway: Ip) -> Result<(), StaticErr> {
        if !same_family(addr, gateway) || !prefix_ok(addr, prefix) {
            return Err(StaticErr::Bad);
        }
        if let Ip::V6(octets) = addr {
            if !Ip::V6(octets).is_link_local() {
                self.install_link_local();
            }
        }
        self.insert(addr, prefix, gateway, 0, false)
    }

    /// The first IPv4 address, its prefix, and the IPv4 gateway.
    pub fn locator(&self) -> Option<([u8; 4], u8, [u8; 4])> {
        let slot = self.v4_slots().first()?;
        let Ip::V4(addr) = slot.ip else {
            return None;
        };
        let Ip::V4(gw) = self.v4_gw? else {
            return None;
        };
        Some((addr, slot.prefix, gw))
    }

    /// Every address, its prefix, and the gateway to print. Generated
    /// link-local has no gateway in the line.
    pub fn each_addr(&self, mut f: impl FnMut(Ip, u8, Option<Ip>, u32)) {
        for slot in self.v4_slots() {
            f(slot.ip, slot.prefix, self.v4_gw, slot.expiry);
        }
        for slot in self.v6_slots() {
            let via = if slot.auto { None } else { self.v6_gw };
            f(slot.ip, slot.prefix, via, slot.expiry);
        }
    }

    /// True when DHCPv4 has a lease, DHCPv6 has a routed address, and
    /// the v6 gateway is known.
    pub fn learn_ready(&self) -> bool {
        self.locator().is_some() && self.v6_gw.is_some() && self.v6_slots().iter().any(|s| !s.auto)
    }

    fn v4_slots(&self) -> &[Slot] {
        &self.v4[..self.v4n as usize]
    }

    fn v6_slots(&self) -> &[Slot] {
        &self.v6[..self.v6n as usize]
    }

    fn install_link_local(&mut self) {
        let ip = link_local_from_mac(self.mac);
        if self.v6_slots().iter().any(|s| s.ip == ip) {
            return;
        }
        if self.v6n as usize >= PER_FAMILY {
            return;
        }
        self.v6[self.v6n as usize] = Slot {
            ip,
            prefix: 64,
            expiry: 0,
            auto: true,
        };
        self.v6n += 1;
    }

    fn insert(
        &mut self,
        addr: Ip,
        prefix: u8,
        gateway: Ip,
        expiry: u32,
        auto: bool,
    ) -> Result<(), StaticErr> {
        let (n, slots, gw) = match addr {
            Ip::V4(_) => (&mut self.v4n, &mut self.v4, &mut self.v4_gw),
            Ip::V6(_) => (&mut self.v6n, &mut self.v6, &mut self.v6_gw),
        };
        if let Some(have) = *gw {
            if have != gateway {
                return Err(StaticErr::OneGateway);
            }
        } else {
            *gw = Some(gateway);
        }
        if slots[..*n as usize].iter().any(|s| s.ip == addr) {
            return Ok(());
        }
        if *n as usize >= PER_FAMILY {
            return Err(StaticErr::Full);
        }
        slots[*n as usize] = Slot {
            ip: addr,
            prefix,
            expiry,
            auto,
        };
        *n += 1;
        Ok(())
    }

    fn source_for(&self, dst: Ip) -> Option<Ip> {
        match dst {
            Ip::V4(_) => self
                .v4_slots()
                .iter()
                .find(|s| prefix_contains(s.ip, s.prefix, dst))
                .map(|s| s.ip)
                .or_else(|| self.v4_slots().first().map(|s| s.ip)),
            Ip::V6(d) => {
                let dest = Ip::V6(d);
                if dest.is_link_local() || dest.is_multicast() {
                    return self
                        .v6_slots()
                        .iter()
                        .find(|s| s.ip.is_link_local())
                        .map(|s| s.ip);
                }
                self.v6_slots()
                    .iter()
                    .find(|s| !s.auto && prefix_contains(s.ip, s.prefix, dst))
                    .map(|s| s.ip)
                    .or_else(|| self.v6_slots().iter().find(|s| !s.auto).map(|s| s.ip))
                    .or_else(|| {
                        self.v6_slots()
                            .iter()
                            .find(|s| s.ip.is_link_local())
                            .map(|s| s.ip)
                    })
            }
        }
    }

    fn on_link(&self, dst: Ip) -> bool {
        match dst {
            Ip::V4(_) => self
                .v4_slots()
                .iter()
                .any(|s| prefix_contains(s.ip, s.prefix, dst)),
            Ip::V6(d) => {
                let dest = Ip::V6(d);
                dest.is_link_local()
                    || self
                        .v6_slots()
                        .iter()
                        .any(|s| prefix_contains(s.ip, s.prefix, dst))
            }
        }
    }

    pub fn holds(&self, dst: Ip) -> bool {
        match dst {
            Ip::V4(_) => self.v4_slots().iter().any(|s| s.ip == dst),
            Ip::V6(_) => self.v6_slots().iter().any(|s| s.ip == dst),
        }
    }

    fn link_local(&self) -> Option<[u8; 16]> {
        self.v6_slots().iter().find_map(|s| match s.ip {
            Ip::V6(a) if s.ip.is_link_local() => Some(a),
            _ => None,
        })
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
        self.poll_v6(now_ms);
        self.flush_pending();
        if self.locator().is_some() && !self.udp_sent {
            if let Some(gw) = self.v4_gw {
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
            if let Some(gw) = self.v4_gw {
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
        } else if eth == 0x86dd {
            self.ingest_ip6(frame);
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
    pub fn udp_send(&mut self, dst: Ip, dport: u16, sport: u16, payload: &[u8]) -> Result<(), ()> {
        let Some(src) = self.source_for(dst) else {
            return Err(());
        };
        let broadcast = dst == Ip::V4([255, 255, 255, 255]);
        let hop = if broadcast { dst } else { self.next_hop(dst) };
        if !broadcast && self.lookup(hop).is_none() && !hop.is_multicast() {
            self.pending_udp = Some(PendingUdp {
                dst,
                dport,
                sport,
                payload: payload.to_vec(),
            });
            self.solicit(hop);
            return Ok(());
        }
        let mac = self.mac_for(hop);
        self.emit_udp(src, dst, mac, sport, dport, payload);
        Ok(())
    }

    /// True when a datagram is not waiting on neighbor discovery.
    pub fn udp_idle(&self) -> bool {
        self.pending_udp.is_none()
    }

    /// Arm a receive for one stored peer. Port 53 is refused. A later
    /// datagram from any other address or port is not queued.
    pub fn udp_arm(&mut self, peer: Ip, peer_port: u16, local_port: u16) -> bool {
        if peer == Ip::V4([0, 0, 0, 0])
            || peer == Ip::V4([255, 255, 255, 255])
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
    pub fn icmp_echo(&mut self, dst: Ip, payload: &[u8]) -> Result<(), ()> {
        if self.source_for(dst).is_none() || payload.len() > 64 {
            return Err(());
        }
        self.echo_seq = self.echo_seq.wrapping_add(1).max(1);
        let id = 0x6761;
        let seq = self.echo_seq;
        self.await_echo = Some(AwaitEcho { dst, id, seq });
        self.echo_from = None;
        let hop = self.next_hop(dst);
        if self.lookup(hop).is_none() && !hop.is_multicast() {
            self.pending_icmp = Some(PendingIcmp {
                dst,
                id,
                seq,
                payload: payload.to_vec(),
            });
            self.solicit(hop);
            return Ok(());
        }
        self.pending_icmp = None;
        self.emit_echo(dst, id, seq, payload);
        Ok(())
    }

    /// The source of an echo reply that matched the outstanding request.
    pub fn take_echo(&mut self) -> Option<Ip> {
        self.echo_from.take()
    }

    /// Active open. Returns a connection id.
    #[allow(clippy::result_unit_err)]
    pub fn tcp_connect(&mut self, dst: Ip, dport: u16) -> Result<u8, ()> {
        if self.source_for(dst).is_none() {
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
            let hop = if p.dst == Ip::V4([255, 255, 255, 255]) {
                p.dst
            } else {
                self.next_hop(p.dst)
            };
            self.lookup(hop).is_some() || hop.is_multicast()
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
        let hop = self.next_hop(p.dst);
        if self.lookup(hop).is_none() && !hop.is_multicast() {
            return;
        }
        let p = self.pending_icmp.take().unwrap();
        self.emit_echo(p.dst, p.id, p.seq, &p.payload);
    }

    fn emit_echo(&mut self, dst: Ip, id: u16, seq: u16, payload: &[u8]) {
        let Some(src) = self.source_for(dst) else {
            return;
        };
        let hop = self.next_hop(dst);
        let mac = self.mac_for(hop);
        match (src, dst) {
            (Ip::V4(s), Ip::V4(d)) => {
                let icmp = echo_message(8, id, seq, payload);
                self.egress.push(ipv4_frame(self.mac, mac, s, d, 1, &icmp));
            }
            (Ip::V6(s), Ip::V6(d)) => {
                self.egress
                    .push(v6::echo(self.mac, mac, s, d, 128, id, seq, payload));
            }
            _ => {}
        }
    }

    /// On-link addresses resolve themselves. Anything else uses that
    /// family's one gateway.
    fn next_hop(&self, ip: Ip) -> Ip {
        if self.on_link(ip) {
            return ip;
        }
        match ip {
            Ip::V4(_) => self.v4_gw.unwrap_or(ip),
            Ip::V6(_) => self.v6_gw.unwrap_or(ip),
        }
    }

    fn lookup(&self, ip: Ip) -> Option<[u8; 6]> {
        self.arp
            .iter()
            .find(|e| e.used && e.ip == ip)
            .map(|e| e.mac)
    }

    fn learn(&mut self, ip: Ip, mac: [u8; 6]) {
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

    fn solicit(&mut self, hop: Ip) {
        match hop {
            Ip::V4(ip) => self.send_arp(ip),
            Ip::V6(ip) => {
                let Some(src) = self.link_local().or_else(|| match self.source_for(hop) {
                    Some(Ip::V6(a)) => Some(a),
                    _ => None,
                }) else {
                    return;
                };
                self.egress.push(v6::neighbor_solicit(self.mac, src, ip));
            }
        }
    }

    fn mac_for(&self, hop: Ip) -> [u8; 6] {
        if let Some(mac) = self.lookup(hop) {
            return mac;
        }
        match hop {
            Ip::V6(ip) if hop.is_multicast() => v6::multicast_mac(&ip),
            Ip::V4([255, 255, 255, 255]) => [0xff; 6],
            _ => [0xff; 6],
        }
    }

    fn emit_udp(&mut self, src: Ip, dst: Ip, mac: [u8; 6], sport: u16, dport: u16, payload: &[u8]) {
        match (src, dst) {
            (Ip::V4(s), Ip::V4(d)) => {
                self.egress
                    .push(udp_frame(self.mac, mac, s, d, sport, dport, payload));
            }
            (Ip::V6(s), Ip::V6(d)) => {
                self.egress
                    .push(v6::udp(self.mac, mac, s, d, sport, dport, payload));
            }
            _ => {}
        }
    }

    fn send_arp(&mut self, target: [u8; 4]) {
        let Some(src) = self.source_for(Ip::V4(target)).and_then(|ip| match ip {
            Ip::V4(a) => Some(a),
            Ip::V6(_) => None,
        }) else {
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
        self.learn(Ip::V4(sip), smac);
        if op == 1 && self.holds(Ip::V4(tip)) {
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
            self.v4_slots()
                .first()
                .and_then(|s| match s.ip {
                    Ip::V4(a) => Some(a),
                    Ip::V6(_) => None,
                })
                .unwrap_or([0; 4]),
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
                self.learn(Ip::V4(src), smac);
            }
        }
        if self.v4n > 0 && !self.holds(Ip::V4(dst)) && dst != [255, 255, 255, 255] {
            return;
        }
        let proto = ip[9];
        let body = &ip[ihl..];
        let src_ip = Ip::V4(src);
        if proto == 1 {
            self.ingest_icmp(src_ip, body);
        } else if proto == 17 {
            self.ingest_udp(src_ip, body);
        } else if proto == 6 {
            self.ingest_tcp(src_ip, body);
        }
    }

    fn ingest_icmp(&mut self, src: Ip, body: &[u8]) {
        if body.len() < 8 || body.len() > 512 || body[1] != 0 {
            return;
        }
        let request = match src {
            Ip::V4(_) => body[0] == 8 && checksum(body) == 0,
            Ip::V6(_) => body[0] == 128,
        };
        let reply = match src {
            Ip::V4(_) => body[0] == 0 && checksum(body) == 0,
            Ip::V6(_) => body[0] == 129,
        };
        if request {
            self.reply_echo(src, body);
        } else if reply {
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

    fn reply_echo(&mut self, dst: Ip, request: &[u8]) {
        let Some(src) = self.source_for(dst) else {
            return;
        };
        let Some(mac) = self.lookup(dst) else {
            return;
        };
        match (src, dst) {
            (Ip::V4(s), Ip::V4(d)) => {
                let mut icmp = request.to_vec();
                icmp[0] = 0;
                icmp[2] = 0;
                icmp[3] = 0;
                let sum = checksum(&icmp);
                icmp[2] = (sum >> 8) as u8;
                icmp[3] = sum as u8;
                self.egress.push(ipv4_frame(self.mac, mac, s, d, 1, &icmp));
            }
            (Ip::V6(s), Ip::V6(d)) => {
                let payload = if request.len() > 8 {
                    &request[8..]
                } else {
                    &[]
                };
                let id = u16::from_be_bytes([request[4], request[5]]);
                let seq = u16::from_be_bytes([request[6], request[7]]);
                self.egress
                    .push(v6::echo(self.mac, mac, s, d, 129, id, seq, payload));
            }
            _ => {}
        }
    }

    fn ingest_udp(&mut self, src: Ip, body: &[u8]) {
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

    fn ingest_dhcp(&mut self, _src: Ip, p: &[u8]) {
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
            let prefix = mask.map(prefix_of).unwrap_or(24);
            let gateway = router.unwrap_or(server);
            self.lease_secs = lease_secs;
            self.dhcp = Dhcp::Bound;
            self.ask = false;
            let _ = self.insert(Ip::V4(yi), prefix, Ip::V4(gateway), lease_secs, false);
        }
    }

    fn ingest_tcp(&mut self, src: Ip, seg: &[u8]) {
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

    fn find_conn(&self, local: u16, remote: u16, dst: Ip) -> Option<u8> {
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

    fn accept_syn(&mut self, listen: u8, src: Ip, sport: u16, seq: u32) {
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
        let (seq, ack, src_port, dst_port, dst) = {
            let c = &self.conns[id as usize];
            (c.snd_nxt, c.rcv_nxt, c.local, c.remote, c.dst)
        };
        let Some(src_ip) = self.source_for(dst) else {
            return;
        };
        let hop = self.next_hop(dst);
        let dst_mac = self.mac_for(hop);
        // MSS 1460 on IPv4, 1440 on IPv6. A larger segment does not fit
        // one virtio receive buffer, so the flight never arrives.
        let mss = match dst {
            Ip::V4(_) => [2u8, 4, 0x05, 0xb4],
            Ip::V6(_) => [2u8, 4, 0x05, 0xa0],
        };
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
        match (src_ip, dst) {
            (Ip::V4(s), Ip::V4(d)) => {
                stamp_tcp(&mut tcp, s, d);
                let frame = ipv4_frame(self.mac, dst_mac, s, d, 6, &tcp);
                self.egress.push(frame);
            }
            (Ip::V6(s), Ip::V6(d)) => {
                let sum = v6::transport_sum(&s, &d, 6, &tcp);
                tcp[16] = (sum >> 8) as u8;
                tcp[17] = sum as u8;
                self.egress.push(v6::tcp(self.mac, dst_mac, s, d, &tcp));
            }
            _ => return,
        }
        self.conns[id as usize].last_tx_ms = 0;
    }

    fn poll_v6(&mut self, now_ms: u64) {
        if !self.learn {
            return;
        }
        if !self.dad_sent {
            if let Some(ll) = self.link_local() {
                self.egress.push(v6::dad(self.mac, ll));
            }
            self.dad_sent = true;
        }
        if !self.rs_sent {
            if let Some(ll) = self.link_local() {
                self.egress.push(v6::router_solicit(self.mac, ll));
            }
            self.rs_sent = true;
        }
        if self.v6_dhcp == Dhcp::Idle && now_ms.saturating_sub(self.v6_ms) >= 200 {
            if let Some(ll) = self.link_local() {
                self.v6_xid = xid_bytes(now_ms);
                self.egress.push(v6::solicit(self.mac, ll, self.v6_xid));
                self.v6_dhcp = Dhcp::WaitOffer;
                self.v6_ms = now_ms;
            }
        } else if matches!(self.v6_dhcp, Dhcp::WaitOffer | Dhcp::WaitAck)
            && now_ms.saturating_sub(self.v6_ms) >= 1000
        {
            self.v6_dhcp = Dhcp::Idle;
            self.v6_ms = now_ms;
        }
    }

    fn ingest_ip6(&mut self, frame: &[u8]) {
        let Some(pkt) = v6::parse(frame) else {
            return;
        };
        if pkt.src != [0u8; 16] && !Ip::V6(pkt.src).is_multicast() {
            let smac = pkt.smac;
            if smac != [0; 6] && smac != [0xff; 6] {
                self.learn(Ip::V6(pkt.src), smac);
            }
        }
        if pkt.next == 58 {
            self.ingest_icmp6(&pkt);
            return;
        }
        if pkt.next != 17 && pkt.next != 6 {
            return;
        }
        let dst = Ip::V6(pkt.dst);
        let mine = self.holds(dst) || self.solicited(pkt.dst) || pkt.dst == v6::all_dhcp();
        if !mine {
            return;
        }
        let src = Ip::V6(pkt.src);
        if pkt.next == 17 {
            if let Some(msg) = v6::dhcp6_reply(&pkt, self.dhcp6_xid()) {
                self.take_dhcp6(msg);
                return;
            }
            self.ingest_udp(src, pkt.body);
        } else if pkt.next == 6 {
            self.ingest_tcp(src, pkt.body);
        }
    }

    fn dhcp6_xid(&self) -> [u8; 3] {
        if self.v6_dhcp == Dhcp::WaitAck {
            self.v6_req
        } else {
            self.v6_xid
        }
    }

    fn solicited(&self, dst: [u8; 16]) -> bool {
        self.v6_slots().iter().any(|s| match s.ip {
            Ip::V6(a) => v6::solicited_node(&a) == dst,
            Ip::V4(_) => false,
        })
    }

    fn ingest_icmp6(&mut self, pkt: &v6::Packet<'_>) {
        if !v6::icmp_ok(pkt) || pkt.body.is_empty() {
            return;
        }
        if let Some(gw) = v6::router_advert_from(pkt) {
            if self.v6_gw.is_none() && self.v6n > 0 {
                self.v6_gw = Some(Ip::V6(gw));
            }
            return;
        }
        let kind = pkt.body[0];
        if kind == 136 {
            self.learn_na(pkt);
            self.flush_pending();
            return;
        }
        if kind == 135 && pkt.body.len() >= 24 {
            let mut target = [0u8; 16];
            target.copy_from_slice(&pkt.body[8..24]);
            if self.holds(Ip::V6(target)) {
                self.reply_ns(pkt, target);
            }
            return;
        }
        if kind == 128 || kind == 129 {
            self.ingest_icmp(Ip::V6(pkt.src), pkt.body);
        }
    }

    fn learn_na(&mut self, pkt: &v6::Packet<'_>) {
        if pkt.body.len() < 24 {
            return;
        }
        let mut target = [0u8; 16];
        target.copy_from_slice(&pkt.body[8..24]);
        let mut mac = pkt.smac;
        let mut i = 24usize;
        while i + 2 <= pkt.body.len() {
            let t = pkt.body[i];
            let len = pkt.body[i + 1] as usize * 8;
            if len < 2 || i + len > pkt.body.len() {
                break;
            }
            if t == 2 && len >= 8 {
                mac.copy_from_slice(&pkt.body[i + 2..i + 8]);
            }
            i += len;
        }
        self.learn(Ip::V6(target), mac);
    }

    fn reply_ns(&mut self, pkt: &v6::Packet<'_>, target: [u8; 16]) {
        let dst = if pkt.src == [0u8; 16] {
            let mut all = [0u8; 16];
            all[0] = 0xff;
            all[1] = 0x02;
            all[15] = 0x01;
            all
        } else {
            pkt.src
        };
        let dmac = if pkt.src == [0u8; 16] {
            v6::multicast_mac(&dst)
        } else {
            pkt.smac
        };
        self.egress.push(v6::neighbor_advert(
            self.mac,
            target,
            dmac,
            dst,
            target,
            pkt.src != [0u8; 16],
        ));
    }

    fn take_dhcp6(&mut self, msg: v6::Dhcp6) {
        if msg.kind == 2 && self.v6_dhcp == Dhcp::WaitOffer {
            self.v6_server = msg.server;
            self.v6_offer = Some(msg.addr);
            self.v6_req = [
                self.v6_xid[0].wrapping_add(1),
                self.v6_xid[1],
                self.v6_xid[2],
            ];
            if let Some(ll) = self.link_local() {
                self.egress.push(v6::request(
                    self.mac,
                    ll,
                    self.v6_req,
                    &self.v6_server,
                    msg.addr,
                ));
            }
            self.v6_dhcp = Dhcp::WaitAck;
            return;
        }
        if msg.kind == 7 && matches!(self.v6_dhcp, Dhcp::WaitOffer | Dhcp::WaitAck) {
            self.push_addr(Ip::V6(msg.addr), 64, msg.valid, false);
            self.v6_dhcp = Dhcp::Bound;
        }
    }

    fn push_addr(&mut self, addr: Ip, prefix: u8, expiry: u32, auto: bool) {
        let (n, slots) = match addr {
            Ip::V4(_) => (&mut self.v4n, &mut self.v4),
            Ip::V6(_) => (&mut self.v6n, &mut self.v6),
        };
        if slots[..*n as usize].iter().any(|s| s.ip == addr) {
            return;
        }
        if *n as usize >= PER_FAMILY {
            return;
        }
        slots[*n as usize] = Slot {
            ip: addr,
            prefix,
            expiry,
            auto,
        };
        *n += 1;
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

fn xid_bytes(now: u64) -> [u8; 3] {
    let n = now as u32;
    [0x67, (n >> 8) as u8, n as u8]
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
        e.udp_send(Ip::V4([172, 30, 0, 2]), 4597, 4598, b"card")
            .unwrap();
        let arp = e.pop().unwrap();
        assert_eq!(&arp[12..14], &[0x08, 0x06]);
        assert_eq!(&arp[38..42], &[10, 0, 2, 2]);
        e.learn(Ip::V4([10, 0, 2, 2]), [9; 6]);
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
        assert!(!e.udp_arm(Ip::V4([172, 30, 0, 2]), 53, 4598));
        assert!(e.udp_arm(Ip::V4([172, 30, 0, 2]), 4602, 4598));
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
        assert!(e.udp_arm(Ip::V4([10, 0, 2, 2]), NTP_PORT, 4123));
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
        a.learn(Ip::V4([10, 0, 0, 2]), [2; 6]);
        let mut b = Engine::new([2; 6], false);
        b.set_fixed([10, 0, 0, 2], 24, [10, 0, 0, 1]);
        b.learn(Ip::V4([10, 0, 0, 1]), [1; 6]);
        b.tcp_listen(4590).unwrap();
        a.tcp_connect(Ip::V4([10, 0, 0, 2]), 4590).unwrap();
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
        e.icmp_echo(Ip::V4([172, 30, 0, 2]), b"galexy").unwrap();
        let arp = e.pop().unwrap();
        assert_eq!(&arp[12..14], &[0x08, 0x06]);
        assert_eq!(&arp[38..42], &[10, 0, 2, 2]);
        e.learn(Ip::V4([10, 0, 2, 2]), [9; 6]);
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
        a.learn(Ip::V4([10, 0, 2, 2]), [2; 6]);
        let mut b = Engine::new([2; 6], false);
        b.set_fixed([10, 0, 2, 2], 24, [10, 0, 2, 15]);
        a.icmp_echo(Ip::V4([10, 0, 2, 2]), b"galexy").unwrap();
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
        assert_eq!(a.take_echo(), Some(Ip::V4([10, 0, 2, 2])));
    }

    #[test]
    fn icmp_bad_reply_is_ignored() {
        let mut a = Engine::new([1; 6], false);
        a.set_fixed([10, 0, 2, 15], 24, [10, 0, 2, 2]);
        a.learn(Ip::V4([10, 0, 2, 2]), [2; 6]);
        a.icmp_echo(Ip::V4([10, 0, 2, 2]), b"galexy").unwrap();
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
        a.icmp_echo(Ip::V4([10, 0, 2, 2]), b"nope").unwrap();
        assert!(a.icmp_echo(Ip::V4([10, 0, 2, 2]), &[0; 65]).is_err());
        assert!(Engine::new([1; 6], true)
            .icmp_echo(Ip::V4([10, 0, 2, 2]), b"x")
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
        engine.learn(Ip::V4([10, 0, 2, 2]), [9; 6]);
        let id = engine.tcp_connect(Ip::V4([1, 1, 1, 1]), 853).unwrap();
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

    #[test]
    fn learn_keeps_link_local_and_dhcp6() {
        let mac = [0x52, 0x54, 0x00, 0x12, 0x34, 0x56];
        let mut engine = Engine::new(mac, false);
        assert!(engine.is_empty());
        engine.poll(1_000);
        assert!(engine.pop().is_none());
        engine.start_dhcp();
        engine.poll(1_000);
        let mut saw_v4 = false;
        let mut saw_v6 = false;
        while let Some(frame) = engine.pop() {
            if frame.len() > 14 && frame[12] == 0x08 && frame[13] == 0x00 {
                saw_v4 = true;
            }
            if frame.len() > 14 && frame[12] == 0x86 && frame[13] == 0xdd {
                saw_v6 = true;
            }
        }
        assert!(saw_v4 && saw_v6);
        engine.poll(1_100);
        assert!(engine.pop().is_none());

        let galexy_core::Ip::V6(ll) = galexy_core::link_local_from_mac(mac) else {
            panic!("link-local");
        };
        let router = {
            let mut a = [0u8; 16];
            a[0] = 0xfe;
            a[1] = 0x80;
            a[15] = 2;
            a
        };
        engine.ingest(&v6::router_advert(
            [0x52, 0x55, 0x00, 0x12, 0x34, 0x02],
            router,
        ));
        let leased = {
            let mut a = ll;
            a[0] = 0xfe;
            a[1] = 0xc0;
            a
        };
        let xid = [0x67, (1_000u32 >> 8) as u8, 1_000u32 as u8];
        let adv = dhcp6_msg(2, xid, mac, leased);
        engine.ingest(&v6::udp(
            [0x52, 0x55, 0x00, 0x12, 0x34, 0x02],
            mac,
            router,
            ll,
            547,
            546,
            &adv,
        ));
        let req_xid = [xid[0].wrapping_add(1), xid[1], xid[2]];
        let reply = dhcp6_msg(7, req_xid, mac, leased);
        engine.ingest(&v6::udp(
            [0x52, 0x55, 0x00, 0x12, 0x34, 0x02],
            mac,
            router,
            ll,
            547,
            546,
            &reply,
        ));
        let mut lines = Vec::new();
        engine.each_addr(|ip, prefix, via, _| {
            let mut buf = [0u8; 64];
            let n = galexy_core::format_ip(ip, &mut buf).unwrap();
            let text = core::str::from_utf8(&buf[..n]).unwrap().to_string();
            let gw = via.map(|gw| {
                let mut g = [0u8; 64];
                let n = galexy_core::format_ip(gw, &mut g).unwrap();
                core::str::from_utf8(&g[..n]).unwrap().to_string()
            });
            lines.push((text, prefix, gw));
        });
        assert!(lines.iter().any(|(ip, prefix, via)| {
            ip == "fe80::5054:ff:fe12:3456" && *prefix == 64 && via.is_none()
        }));
        assert!(lines.iter().any(|(ip, prefix, via)| {
            ip == "fec0::5054:ff:fe12:3456" && *prefix == 64 && via.as_deref() == Some("fe80::2")
        }));
    }

    #[test]
    fn static_address_sends_nothing_and_keeps_one_gateway() {
        let mut engine = Engine::new([1, 2, 3, 4, 5, 6], false);
        let local = galexy_core::parse_ip(b"fe80::1").unwrap();
        engine.add_static(local, 64, local).unwrap();
        engine.poll(1_000);
        assert!(engine.pop().is_none());
        let other = galexy_core::parse_ip(b"fe80::3").unwrap();
        let gw = galexy_core::parse_ip(b"fe80::2").unwrap();
        assert_eq!(engine.add_static(other, 64, gw), Err(StaticErr::OneGateway));
        let global = galexy_core::parse_ip(b"2001:db8::1").unwrap();
        assert_eq!(
            engine.add_static(global, 64, gw),
            Err(StaticErr::OneGateway)
        );
        let v4 = galexy_core::parse_ip(b"10.0.2.15").unwrap();
        assert_eq!(engine.add_static(v4, 24, local), Err(StaticErr::Bad));
        assert_eq!(engine.add_static(global, 64, local), Ok(()));
        let mut n = 0;
        engine.each_addr(|_, _, _, _| n += 1);
        assert_eq!(n, 3);
    }

    fn dhcp6_msg(kind: u8, xid: [u8; 3], mac: [u8; 6], addr: [u8; 16]) -> Vec<u8> {
        let mut msg = Vec::new();
        msg.push(kind);
        msg.extend_from_slice(&xid);
        let mut duid = vec![0, 3, 0, 1];
        duid.extend_from_slice(&mac);
        push_dhcp_opt(&mut msg, 2, &duid);
        let mut ia = vec![0u8; 12];
        ia[..4].copy_from_slice(&1u32.to_be_bytes());
        let mut iaaddr = [0u8; 24];
        iaaddr[..16].copy_from_slice(&addr);
        iaaddr[16..20].copy_from_slice(&3600u32.to_be_bytes());
        push_dhcp_opt(&mut ia, 5, &iaaddr);
        push_dhcp_opt(&mut msg, 3, &ia);
        msg
    }

    fn push_dhcp_opt(out: &mut Vec<u8>, code: u16, val: &[u8]) {
        out.extend_from_slice(&code.to_be_bytes());
        out.extend_from_slice(&(val.len() as u16).to_be_bytes());
        out.extend_from_slice(val);
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
