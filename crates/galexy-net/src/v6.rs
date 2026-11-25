//! IPv6 frames for the one engine: neighbor discovery, router solicitation,
//! and DHCPv6. DNS options are not read.

use alloc::vec;
use alloc::vec::Vec;

use super::Frame;

pub(crate) const HOP_ND: u8 = 255;
const HOP_UDP: u8 = 64;

/// All-routers multicast.
pub(crate) fn all_routers() -> [u8; 16] {
    let mut ip = [0u8; 16];
    ip[0] = 0xff;
    ip[1] = 0x02;
    ip[15] = 0x02;
    ip
}

/// All DHCP servers and relays on the link.
pub(crate) fn all_dhcp() -> [u8; 16] {
    let mut ip = [0u8; 16];
    ip[0] = 0xff;
    ip[1] = 0x02;
    ip[13] = 0x01;
    ip[15] = 0x02;
    ip
}

pub(crate) fn solicited_node(target: &[u8; 16]) -> [u8; 16] {
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

pub(crate) fn multicast_mac(ip: &[u8; 16]) -> [u8; 6] {
    [0x33, 0x33, ip[12], ip[13], ip[14], ip[15]]
}

/// One duplicate-address solicitation. The source address is unspecified.
pub(crate) fn dad(src_mac: [u8; 6], target: [u8; 16]) -> Frame {
    let mut icmp = vec![135, 0, 0, 0, 0, 0, 0, 0];
    icmp.extend_from_slice(&target);
    stamp_icmp(&mut icmp, &[0u8; 16], &solicited_node(&target));
    ipv6(
        multicast_mac(&solicited_node(&target)),
        src_mac,
        [0u8; 16],
        solicited_node(&target),
        58,
        HOP_ND,
        &icmp,
    )
}

/// One router advertisement. The engine does not send these. Tests do.
pub(crate) fn router_advert(src_mac: [u8; 6], src: [u8; 16]) -> Frame {
    let mut icmp = vec![134, 0, 0, 0, 64, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];
    let mut dst = [0u8; 16];
    dst[0] = 0xff;
    dst[1] = 0x02;
    dst[15] = 1;
    stamp_icmp(&mut icmp, &src, &dst);
    ipv6(multicast_mac(&dst), src_mac, src, dst, 58, HOP_ND, &icmp)
}

/// One router solicitation from our link-local.
pub(crate) fn router_solicit(src_mac: [u8; 6], src: [u8; 16]) -> Frame {
    let mut icmp = vec![133, 0, 0, 0, 0, 0, 0, 0];
    icmp.extend_from_slice(&[1, 1]);
    icmp.extend_from_slice(&src_mac);
    let dst = all_routers();
    stamp_icmp(&mut icmp, &src, &dst);
    ipv6(multicast_mac(&dst), src_mac, src, dst, 58, HOP_ND, &icmp)
}

/// Neighbor solicitation for `target`, from `src`.
pub(crate) fn neighbor_solicit(src_mac: [u8; 6], src: [u8; 16], target: [u8; 16]) -> Frame {
    let mut icmp = vec![135, 0, 0, 0, 0, 0, 0, 0];
    icmp.extend_from_slice(&target);
    icmp.extend_from_slice(&[1, 1]);
    icmp.extend_from_slice(&src_mac);
    let dst = solicited_node(&target);
    stamp_icmp(&mut icmp, &src, &dst);
    ipv6(multicast_mac(&dst), src_mac, src, dst, 58, HOP_ND, &icmp)
}

/// Neighbor advertisement. `solicited` answers a solicitation.
pub(crate) fn neighbor_advert(
    src_mac: [u8; 6],
    src: [u8; 16],
    dst_mac: [u8; 6],
    dst: [u8; 16],
    target: [u8; 16],
    solicited: bool,
) -> Frame {
    let flags = if solicited { 0x60 } else { 0x20 };
    let mut icmp = vec![136, 0, 0, 0, flags, 0, 0, 0];
    icmp.extend_from_slice(&target);
    icmp.extend_from_slice(&[2, 1]);
    icmp.extend_from_slice(&src_mac);
    stamp_icmp(&mut icmp, &src, &dst);
    ipv6(dst_mac, src_mac, src, dst, 58, HOP_ND, &icmp)
}

pub(crate) fn udp(
    src_mac: [u8; 6],
    dst_mac: [u8; 6],
    src: [u8; 16],
    dst: [u8; 16],
    sport: u16,
    dport: u16,
    payload: &[u8],
) -> Frame {
    let mut body = Vec::with_capacity(8 + payload.len());
    body.extend_from_slice(&sport.to_be_bytes());
    body.extend_from_slice(&dport.to_be_bytes());
    body.extend_from_slice(&((8 + payload.len()) as u16).to_be_bytes());
    body.extend_from_slice(&[0, 0]);
    body.extend_from_slice(payload);
    let sum = transport_sum(&src, &dst, 17, &body);
    let sum = if sum == 0 { 0xffff } else { sum };
    body[6] = (sum >> 8) as u8;
    body[7] = sum as u8;
    ipv6(dst_mac, src_mac, src, dst, 17, HOP_UDP, &body)
}

pub(crate) fn tcp(
    src_mac: [u8; 6],
    dst_mac: [u8; 6],
    src: [u8; 16],
    dst: [u8; 16],
    segment: &[u8],
) -> Frame {
    ipv6(dst_mac, src_mac, src, dst, 6, HOP_UDP, segment)
}

pub(crate) fn echo(
    src_mac: [u8; 6],
    dst_mac: [u8; 6],
    src: [u8; 16],
    dst: [u8; 16],
    kind: u8,
    id: u16,
    seq: u16,
    payload: &[u8],
) -> Frame {
    let mut icmp = Vec::with_capacity(8 + payload.len());
    icmp.extend_from_slice(&[kind, 0, 0, 0]);
    icmp.extend_from_slice(&id.to_be_bytes());
    icmp.extend_from_slice(&seq.to_be_bytes());
    icmp.extend_from_slice(payload);
    stamp_icmp(&mut icmp, &src, &dst);
    ipv6(dst_mac, src_mac, src, dst, 58, HOP_UDP, &icmp)
}

/// A parsed IPv6 packet. `body` is the next-header payload, checksum and all.
pub(crate) struct Packet<'a> {
    pub src: [u8; 16],
    pub dst: [u8; 16],
    pub hop: u8,
    pub next: u8,
    pub body: &'a [u8],
    pub smac: [u8; 6],
}

pub(crate) fn parse(frame: &[u8]) -> Option<Packet<'_>> {
    if frame.len() < 14 + 40 || frame[12] != 0x86 || frame[13] != 0xdd {
        return None;
    }
    let ip = &frame[14..];
    if ip[0] >> 4 != 6 {
        return None;
    }
    let plen = u16::from_be_bytes([ip[4], ip[5]]) as usize;
    if ip.len() < 40 + plen {
        return None;
    }
    let mut src = [0u8; 16];
    let mut dst = [0u8; 16];
    let mut smac = [0u8; 6];
    src.copy_from_slice(&ip[8..24]);
    dst.copy_from_slice(&ip[24..40]);
    smac.copy_from_slice(&frame[6..12]);
    Some(Packet {
        src,
        dst,
        hop: ip[7],
        next: ip[6],
        body: &ip[40..40 + plen],
        smac,
    })
}

/// Router advertisement source, when the hop limit is 255 and the source
/// is link-local. The prefix is not turned into an address.
pub(crate) fn router_advert_from(pkt: &Packet<'_>) -> Option<[u8; 16]> {
    if pkt.next != 58 || pkt.hop != HOP_ND || pkt.body.len() < 16 || pkt.body[0] != 134 {
        return None;
    }
    if !is_link_local(&pkt.src) || icmp_sum(&pkt.src, &pkt.dst, pkt.body) != 0 {
        return None;
    }
    Some(pkt.src)
}

pub(crate) struct Dhcp6 {
    pub kind: u8,
    pub addr: [u8; 16],
    pub valid: u32,
    pub server: Vec<u8>,
}

/// DHCPv6 advertise (2) or reply (7) carrying one IA address. DNS options
/// are skipped.
pub(crate) fn dhcp6_reply(pkt: &Packet<'_>, xid: [u8; 3]) -> Option<Dhcp6> {
    if pkt.next != 17 || pkt.body.len() < 12 {
        return None;
    }
    let sport = u16::from_be_bytes([pkt.body[0], pkt.body[1]]);
    let dport = u16::from_be_bytes([pkt.body[2], pkt.body[3]]);
    if sport != 547 || dport != 546 {
        return None;
    }
    if transport_sum(&pkt.src, &pkt.dst, 17, pkt.body) != 0 {
        return None;
    }
    let msg = &pkt.body[8..];
    if msg.len() < 4 || msg[1..4] != xid {
        return None;
    }
    let kind = msg[0];
    if kind != 2 && kind != 7 {
        return None;
    }
    let mut server = None;
    let mut found = None;
    let mut i = 4usize;
    while i + 4 <= msg.len() {
        let code = u16::from_be_bytes([msg[i], msg[i + 1]]);
        let len = u16::from_be_bytes([msg[i + 2], msg[i + 3]]) as usize;
        i += 4;
        if i + len > msg.len() {
            return None;
        }
        let val = &msg[i..i + len];
        if code == 2 {
            server = Some(val.to_vec());
        } else if code == 3 {
            found = ia_addr(val);
        }
        i += len;
    }
    let (addr, valid) = found?;
    Some(Dhcp6 {
        kind,
        addr,
        valid,
        server: server.unwrap_or_default(),
    })
}

pub(crate) fn solicit(src_mac: [u8; 6], src: [u8; 16], xid: [u8; 3]) -> Frame {
    let mut msg = Vec::new();
    msg.push(1);
    msg.extend_from_slice(&xid);
    push_opt(&mut msg, 1, &duid(src_mac));
    push_opt(&mut msg, 8, &[0, 0]);
    let mut ia = [0u8; 12];
    ia[..4].copy_from_slice(&1u32.to_be_bytes());
    push_opt(&mut msg, 3, &ia);
    udp(
        src_mac,
        multicast_mac(&all_dhcp()),
        src,
        all_dhcp(),
        546,
        547,
        &msg,
    )
}

pub(crate) fn request(
    src_mac: [u8; 6],
    src: [u8; 16],
    xid: [u8; 3],
    server: &[u8],
    addr: [u8; 16],
) -> Frame {
    let mut msg = Vec::new();
    msg.push(3);
    msg.extend_from_slice(&xid);
    push_opt(&mut msg, 1, &duid(src_mac));
    push_opt(&mut msg, 2, server);
    push_opt(&mut msg, 8, &[0, 0]);
    let mut ia = vec![0u8; 12];
    ia[..4].copy_from_slice(&1u32.to_be_bytes());
    let mut iaaddr = [0u8; 24];
    iaaddr[..16].copy_from_slice(&addr);
    iaaddr[16..20].copy_from_slice(&3600u32.to_be_bytes());
    iaaddr[20..24].copy_from_slice(&7200u32.to_be_bytes());
    push_opt(&mut ia, 5, &iaaddr);
    push_opt(&mut msg, 3, &ia);
    udp(
        src_mac,
        multicast_mac(&all_dhcp()),
        src,
        all_dhcp(),
        546,
        547,
        &msg,
    )
}

pub(crate) fn icmp_ok(pkt: &Packet<'_>) -> bool {
    pkt.next == 58 && icmp_sum(&pkt.src, &pkt.dst, pkt.body) == 0
}

fn ia_addr(ia: &[u8]) -> Option<([u8; 16], u32)> {
    if ia.len() < 12 {
        return None;
    }
    let mut i = 12usize;
    while i + 4 <= ia.len() {
        let code = u16::from_be_bytes([ia[i], ia[i + 1]]);
        let len = u16::from_be_bytes([ia[i + 2], ia[i + 3]]) as usize;
        i += 4;
        if i + len > ia.len() {
            return None;
        }
        if code == 5 && len >= 24 {
            let mut addr = [0u8; 16];
            addr.copy_from_slice(&ia[i..i + 16]);
            let valid = u32::from_be_bytes([ia[i + 20], ia[i + 21], ia[i + 22], ia[i + 23]]);
            return Some((addr, valid));
        }
        i += len;
    }
    None
}

fn duid(mac: [u8; 6]) -> [u8; 10] {
    let mut d = [0u8; 10];
    d[1] = 3;
    d[3] = 1;
    d[4..10].copy_from_slice(&mac);
    d
}

fn push_opt(out: &mut Vec<u8>, code: u16, val: &[u8]) {
    out.extend_from_slice(&code.to_be_bytes());
    out.extend_from_slice(&(val.len() as u16).to_be_bytes());
    out.extend_from_slice(val);
}

fn is_link_local(ip: &[u8; 16]) -> bool {
    ip[0] == 0xfe && (ip[1] & 0xc0) == 0x80
}

fn ipv6(
    dmac: [u8; 6],
    smac: [u8; 6],
    src: [u8; 16],
    dst: [u8; 16],
    next: u8,
    hop: u8,
    body: &[u8],
) -> Frame {
    let mut f = Vec::with_capacity(14 + 40 + body.len());
    f.extend_from_slice(&dmac);
    f.extend_from_slice(&smac);
    f.extend_from_slice(&[0x86, 0xdd]);
    f.extend_from_slice(&[0x60, 0, 0, 0]);
    f.extend_from_slice(&(body.len() as u16).to_be_bytes());
    f.push(next);
    f.push(hop);
    f.extend_from_slice(&src);
    f.extend_from_slice(&dst);
    f.extend_from_slice(body);
    f
}

fn stamp_icmp(icmp: &mut [u8], src: &[u8; 16], dst: &[u8; 16]) {
    icmp[2] = 0;
    icmp[3] = 0;
    let sum = icmp_sum(src, dst, icmp);
    icmp[2] = (sum >> 8) as u8;
    icmp[3] = sum as u8;
}

fn icmp_sum(src: &[u8; 16], dst: &[u8; 16], icmp: &[u8]) -> u16 {
    transport_sum(src, dst, 58, icmp)
}

pub(crate) fn transport_sum(src: &[u8; 16], dst: &[u8; 16], next: u8, body: &[u8]) -> u16 {
    let mut p = Vec::with_capacity(40 + body.len());
    p.extend_from_slice(src);
    p.extend_from_slice(dst);
    p.extend_from_slice(&(body.len() as u32).to_be_bytes());
    p.extend_from_slice(&[0, 0, 0, next]);
    p.extend_from_slice(body);
    checksum(&p)
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
