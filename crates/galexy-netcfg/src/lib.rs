//! Version 3 bytes for the `/net` record.
//!
//! Magic, version, mode, the address list, one optional gateway per
//! family, the directory, then the pinned resolver. Tag 0 is absent.
//! Tag 1 is the address and a 32-byte key. Tag 2 is the address and the
//! domain taken from the certificate. The resolver trusts the bundle in
//! the image for that domain. A tag 2 record with no domain still loads.
//! An unknown tag is refused. A version this crate does not know is an
//! error. Callers do not guess. Version 1 and version 2 are refused.

#![cfg_attr(not(test), no_std)]

use galexy_core::Ip;

/// File magic, ASCII `GNET`.
pub const MAGIC: &[u8; 4] = b"GNET";
/// Record version. Unknown versions, including version 1 and 2, are refused.
pub const VERSION: u16 = 3;
/// Addresses stored on the link. Four per family.
pub const MAX_ADDRS: usize = 8;
/// A name resolved over DNS-over-TLS. The address is not stored.
pub const KIND_DNS: u8 = 1;
/// IPv4 locator.
pub const KIND_IPV4: u8 = 4;
/// IPv6 locator. It stays inside `net`.
pub const KIND_IPV6: u8 = 6;
/// Address card. The name is the dotted address.
pub const KIND_ADDR: u8 = 8;
/// Port card. The name is the decimal port.
pub const KIND_PORT: u8 = 9;
/// Datagram card. The name is a label. The address and port are the
/// one peer a read may send to. Port 53 is not a peer.
pub const KIND_DGRAM: u8 = 10;
/// DNS-over-TLS port. The record does not store another one.
pub const DOT_PORT: u16 = 853;
/// Domain taken from the resolver certificate. It fits in a `net dns` reply.
pub const RESOLVER_NAME_MAX: usize = 128;
/// Mode byte: ask DHCP.
pub const MODE_ASK: u8 = 0;
/// Mode byte: use the stored locator and skip DHCP.
pub const MODE_FIXED: u8 = 1;
/// Directory entries in one record.
pub const MAX_NAMES: usize = 16;
/// Actors who may dial one name.
pub const MAX_ACTORS: usize = 4;
/// Name and actor byte cap.
pub const NAME_MAX: usize = 64;

/// Why a record was refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CfgError {
    /// Truncated, bad magic, or a version other than [`VERSION`].
    BadRecord,
    /// The directory or an actor list is full, or the name is empty.
    Full,
    /// No entry has that name.
    Missing,
}

/// One address on the link. The gateway is stored once per family.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CfgAddr {
    /// Address.
    pub ip: Ip,
    /// Prefix length.
    pub prefix: u8,
    /// Lease lifetime in seconds. `0` means none.
    pub expiry: u32,
}

impl CfgAddr {
    /// Empty slot. Not part of the live list.
    pub const fn blank() -> Self {
        Self {
            ip: Ip::V4([0, 0, 0, 0]),
            prefix: 0,
            expiry: 0,
        }
    }
}

/// One directory entry. The key is the raw 32-byte pin.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Name {
    /// Locator name, not a DNS search.
    pub name: [u8; NAME_MAX],
    /// Length of [`Self::name`].
    pub name_len: u8,
    /// Remote port. `0` on an address card means the card does not dial.
    pub port: u16,
    /// [`KIND_DNS`], [`KIND_IPV4`], [`KIND_IPV6`], [`KIND_ADDR`],
    /// [`KIND_PORT`], or [`KIND_DGRAM`].
    pub kind: u8,
    /// Address. IPv4 uses the first four bytes.
    pub ip: [u8; 16],
    /// Pinned key.
    pub key: [u8; 32],
    /// Actors who may dial. Unused slots are zero.
    pub actors: [[u8; NAME_MAX]; MAX_ACTORS],
    /// Length of each actor.
    pub actor_len: [u8; MAX_ACTORS],
    /// How many actors are set.
    pub actor_count: u8,
}

impl Name {
    /// Empty entry.
    pub const fn empty() -> Self {
        Self {
            name: [0; NAME_MAX],
            name_len: 0,
            port: 0,
            kind: 4,
            ip: [0; 16],
            key: [0; 32],
            actors: [[0; NAME_MAX]; MAX_ACTORS],
            actor_len: [0; MAX_ACTORS],
            actor_count: 0,
        }
    }

    /// The name as bytes.
    pub fn name_bytes(&self) -> &[u8] {
        &self.name[..self.name_len as usize]
    }
}

/// DNS-over-TLS resolver. The port is always [`DOT_PORT`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Resolver {
    /// Resolver address. `10.0.2.2` and `10.0.2.3` are refused.
    pub addr: Ip,
    /// Raw key. `None` trusts the bundle in the image for [`Self::name`].
    /// `Some` is that key, and the bundle is not consulted. Callers do
    /// not write these bytes to the console.
    pub key: Option<[u8; 32]>,
    /// Domain from the certificate. Empty when a key is the pin, and
    /// empty on a tag 2 record written before the domain was stored.
    pub name: [u8; RESOLVER_NAME_MAX],
    /// Length of [`Self::name`]. Zero means no domain.
    pub name_len: u8,
}

/// The whole file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Record {
    /// [`MODE_ASK`] or [`MODE_FIXED`].
    pub mode: u8,
    /// Learned or configured addresses. Live entries are the prefix of
    /// length [`Self::addr_count`].
    pub addrs: [CfgAddr; MAX_ADDRS],
    /// Live entries in [`Self::addrs`].
    pub addr_count: u8,
    /// The one IPv4 gateway, when the link has one.
    pub v4_gw: Option<Ip>,
    /// The one IPv6 gateway, when the link has one.
    pub v6_gw: Option<Ip>,
    /// Directory.
    pub names: [Name; MAX_NAMES],
    /// Live entries in [`Self::names`].
    pub name_count: u8,
    /// DNS resolver. Absent means lookup fails closed.
    pub resolver: Option<Resolver>,
}

impl Record {
    /// Mode `ask`, no lease, empty directory. Init writes this once.
    pub const fn empty_ask() -> Self {
        Self {
            mode: MODE_ASK,
            addrs: [CfgAddr::blank(); MAX_ADDRS],
            addr_count: 0,
            v4_gw: None,
            v6_gw: None,
            names: [Name::empty(); MAX_NAMES],
            name_count: 0,
            resolver: None,
        }
    }

    /// Stores the resolver. `10.0.2.2` and `10.0.2.3` are refused.
    ///
    /// `key` absent trusts the bundle in the image. The domain is stored
    /// with [`Self::dns_domain`]. `key` present is that raw key, and the
    /// bundle is not consulted.
    pub fn dns(&mut self, addr: Ip, key: Option<[u8; 32]>) -> Result<(), CfgError> {
        if let Ip::V4(octets) = addr {
            if octets == [10, 0, 2, 2] || octets == [10, 0, 2, 3] {
                return Err(CfgError::BadRecord);
            }
        }
        self.resolver = Some(Resolver {
            addr,
            key,
            name: [0; RESOLVER_NAME_MAX],
            name_len: 0,
        });
        Ok(())
    }

    /// Stores the domain taken from the certificate. A key file has no
    /// domain. A wildcard is refused.
    pub fn dns_domain(&mut self, name: &[u8]) -> Result<(), CfgError> {
        let Some(resolver) = self.resolver.as_mut() else {
            return Err(CfgError::BadRecord);
        };
        if resolver.key.is_some() || !domain_ok(name) {
            return Err(CfgError::BadRecord);
        }
        resolver.name = [0; RESOLVER_NAME_MAX];
        resolver.name[..name.len()].copy_from_slice(name);
        resolver.name_len = name.len() as u8;
        Ok(())
    }

    /// Installs or replaces a name. Actors on an existing name are kept
    /// when the locator and key are replaced, unless `reset_actors`.
    pub fn allow(
        &mut self,
        name: &[u8],
        port: u16,
        kind: u8,
        ip: [u8; 16],
        key: [u8; 32],
    ) -> Result<(), CfgError> {
        if name.is_empty() || name.len() > NAME_MAX || !kind_ok(kind) {
            return Err(CfgError::BadRecord);
        }
        if let Some(slot) = self.find_mut(name) {
            slot.port = port;
            slot.kind = kind;
            slot.ip = ip;
            slot.key = key;
            return Ok(());
        }
        if self.name_count as usize >= MAX_NAMES {
            return Err(CfgError::Full);
        }
        let slot = &mut self.names[self.name_count as usize];
        *slot = Name::empty();
        slot.name[..name.len()].copy_from_slice(name);
        slot.name_len = name.len() as u8;
        slot.port = port;
        slot.kind = kind;
        slot.ip = ip;
        slot.key = key;
        self.name_count += 1;
        Ok(())
    }

    /// Lets `actor` dial `name`.
    pub fn grant(&mut self, name: &[u8], actor: &[u8]) -> Result<(), CfgError> {
        let slot = self.find_mut(name).ok_or(CfgError::Missing)?;
        if actor.is_empty() || actor.len() > NAME_MAX {
            return Err(CfgError::BadRecord);
        }
        for i in 0..slot.actor_count as usize {
            if &slot.actors[i][..slot.actor_len[i] as usize] == actor {
                return Ok(());
            }
        }
        if slot.actor_count as usize >= MAX_ACTORS {
            return Err(CfgError::Full);
        }
        let i = slot.actor_count as usize;
        slot.actors[i][..actor.len()].copy_from_slice(actor);
        slot.actor_len[i] = actor.len() as u8;
        slot.actor_count += 1;
        Ok(())
    }

    /// Drops `actor` from `name`.
    pub fn revoke(&mut self, name: &[u8], actor: &[u8]) -> Result<(), CfgError> {
        let slot = self.find_mut(name).ok_or(CfgError::Missing)?;
        let Some(at) = (0..slot.actor_count as usize)
            .find(|i| &slot.actors[*i][..slot.actor_len[*i] as usize] == actor)
        else {
            return Ok(());
        };
        for i in at + 1..slot.actor_count as usize {
            slot.actors[i - 1] = slot.actors[i];
            slot.actor_len[i - 1] = slot.actor_len[i];
        }
        slot.actor_count -= 1;
        let last = slot.actor_count as usize;
        slot.actors[last] = [0; NAME_MAX];
        slot.actor_len[last] = 0;
        Ok(())
    }

    /// True when `actor` may dial `name`.
    pub fn may_dial(&self, name: &[u8], actor: &[u8]) -> bool {
        let Some(slot) = self.find(name) else {
            return false;
        };
        (0..slot.actor_count as usize)
            .any(|i| &slot.actors[i][..slot.actor_len[i] as usize] == actor)
    }

    /// Stores `key` when `name` has no pin yet.
    ///
    /// An empty key is refused. A pin that is already set stays as it
    /// is, so a changed key cannot replace a stored one through this path.
    pub fn remember_key(&mut self, name: &[u8], key: [u8; 32]) -> Result<(), CfgError> {
        if key == [0; 32] {
            return Err(CfgError::BadRecord);
        }
        let slot = self.find_mut(name).ok_or(CfgError::Missing)?;
        if slot.key != [0; 32] {
            return Err(CfgError::BadRecord);
        }
        slot.key = key;
        Ok(())
    }

    /// The entry, if the name is installed.
    pub fn find(&self, name: &[u8]) -> Option<&Name> {
        self.names[..self.name_count as usize]
            .iter()
            .find(|n| n.name_bytes() == name)
    }

    fn find_mut(&mut self, name: &[u8]) -> Option<&mut Name> {
        let n = self.name_count as usize;
        self.names[..n].iter_mut().find(|n| n.name_bytes() == name)
    }
}

/// Encodes `record` into `out`. Returns the byte count.
pub fn encode(record: &Record, out: &mut [u8]) -> Result<usize, CfgError> {
    let mut w = Writer { buf: out, at: 0 };
    w.bytes(MAGIC)?;
    w.u16(VERSION)?;
    w.u8(record.mode)?;
    w.u8(record.addr_count)?;
    for addr in &record.addrs[..record.addr_count as usize] {
        w.u8(addr.ip.kind())?;
        w.bytes(&addr.ip.octets16())?;
        w.u8(addr.prefix)?;
        w.u32(addr.expiry)?;
    }
    write_gw(&mut w, record.v4_gw)?;
    write_gw(&mut w, record.v6_gw)?;
    w.u8(record.name_count)?;
    for name in &record.names[..record.name_count as usize] {
        w.u8(name.name_len)?;
        w.bytes(name.name_bytes())?;
        w.u16(name.port)?;
        w.u8(name.kind)?;
        w.bytes(&name.ip)?;
        w.bytes(&name.key)?;
        w.u8(name.actor_count)?;
        for i in 0..name.actor_count as usize {
            w.u8(name.actor_len[i])?;
            w.bytes(&name.actors[i][..name.actor_len[i] as usize])?;
        }
    }
    match &record.resolver {
        None => w.u8(0)?,
        Some(resolver) => match resolver.key {
            Some(key) => {
                w.u8(1)?;
                w.u8(resolver.addr.kind())?;
                w.bytes(&resolver.addr.octets16())?;
                w.bytes(&key)?;
            }
            None => {
                w.u8(2)?;
                w.u8(resolver.addr.kind())?;
                w.bytes(&resolver.addr.octets16())?;
                w.u8(resolver.name_len)?;
                w.bytes(&resolver.name[..resolver.name_len as usize])?;
            }
        },
    }
    Ok(w.at)
}

fn domain_ok(name: &[u8]) -> bool {
    if name.is_empty() || name.len() > RESOLVER_NAME_MAX || name.starts_with(b"*.") {
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

fn kind_ok(kind: u8) -> bool {
    matches!(
        kind,
        KIND_DNS | KIND_IPV4 | KIND_IPV6 | KIND_ADDR | KIND_PORT | KIND_DGRAM
    )
}

fn write_gw(w: &mut Writer<'_>, gw: Option<Ip>) -> Result<(), CfgError> {
    match gw {
        None => w.u8(0),
        Some(ip) => {
            w.u8(1)?;
            w.u8(ip.kind())?;
            w.bytes(&ip.octets16())
        }
    }
}

fn read_gw(r: &mut Reader<'_>) -> Result<Option<Ip>, CfgError> {
    if r.u8()? == 0 {
        Ok(None)
    } else {
        let kind = r.u8()?;
        let mut octets = [0u8; 16];
        r.bytes(&mut octets)?;
        Ip::from_kind(kind, octets)
            .filter(|ip| match ip {
                Ip::V4(a) => *a != [0, 0, 0, 0],
                Ip::V6(a) => *a != [0u8; 16],
            })
            .map(Some)
            .ok_or(CfgError::BadRecord)
    }
}

fn read_ip(r: &mut Reader<'_>) -> Result<Ip, CfgError> {
    let kind = r.u8()?;
    let mut octets = [0u8; 16];
    r.bytes(&mut octets)?;
    Ip::from_kind(kind, octets).ok_or(CfgError::BadRecord)
}

/// Decodes a version-3 record. Any other version is [`CfgError::BadRecord`].
pub fn decode(bytes: &[u8]) -> Result<Record, CfgError> {
    let mut r = Reader { buf: bytes, at: 0 };
    let mut magic = [0u8; 4];
    r.bytes(&mut magic)?;
    if &magic != MAGIC {
        return Err(CfgError::BadRecord);
    }
    if r.u16()? != VERSION {
        return Err(CfgError::BadRecord);
    }
    let mode = r.u8()?;
    if mode != MODE_ASK && mode != MODE_FIXED {
        return Err(CfgError::BadRecord);
    }
    let addr_count = r.u8()?;
    if addr_count as usize > MAX_ADDRS {
        return Err(CfgError::BadRecord);
    }
    let mut addrs = [CfgAddr::blank(); MAX_ADDRS];
    for addr in addrs.iter_mut().take(addr_count as usize) {
        addr.ip = read_ip(&mut r)?;
        addr.prefix = r.u8()?;
        if !galexy_core::prefix_ok(addr.ip, addr.prefix) {
            return Err(CfgError::BadRecord);
        }
        addr.expiry = r.u32()?;
    }
    let v4_gw = read_gw(&mut r)?;
    let v6_gw = read_gw(&mut r)?;
    if v4_gw.is_some_and(|ip| !ip.is_v4()) || v6_gw.is_some_and(|ip| ip.is_v4()) {
        return Err(CfgError::BadRecord);
    }
    let name_count = r.u8()?;
    if name_count as usize > MAX_NAMES {
        return Err(CfgError::BadRecord);
    }
    let mut names = [Name::empty(); MAX_NAMES];
    for name in names.iter_mut().take(name_count as usize) {
        let nlen = r.u8()? as usize;
        if nlen == 0 || nlen > NAME_MAX {
            return Err(CfgError::BadRecord);
        }
        r.bytes(&mut name.name[..nlen])?;
        name.name_len = nlen as u8;
        name.port = r.u16()?;
        name.kind = r.u8()?;
        if !kind_ok(name.kind) {
            return Err(CfgError::BadRecord);
        }
        r.bytes(&mut name.ip)?;
        r.bytes(&mut name.key)?;
        name.actor_count = r.u8()?;
        if name.actor_count as usize > MAX_ACTORS {
            return Err(CfgError::BadRecord);
        }
        for i in 0..name.actor_count as usize {
            let alen = r.u8()? as usize;
            if alen == 0 || alen > NAME_MAX {
                return Err(CfgError::BadRecord);
            }
            r.bytes(&mut name.actors[i][..alen])?;
            name.actor_len[i] = alen as u8;
        }
    }
    let resolver = match r.u8()? {
        0 => None,
        1 => {
            let addr = read_ip(&mut r)?;
            let mut key = [0u8; 32];
            r.bytes(&mut key)?;
            if refused_resolver(addr) {
                return Err(CfgError::BadRecord);
            }
            Some(Resolver {
                addr,
                key: Some(key),
                name: [0; RESOLVER_NAME_MAX],
                name_len: 0,
            })
        }
        2 => {
            let addr = read_ip(&mut r)?;
            if refused_resolver(addr) {
                return Err(CfgError::BadRecord);
            }
            let mut name = [0u8; RESOLVER_NAME_MAX];
            let mut name_len = 0u8;
            if r.at < bytes.len() {
                name_len = r.u8()?;
                let n = name_len as usize;
                if n > RESOLVER_NAME_MAX {
                    return Err(CfgError::BadRecord);
                }
                r.bytes(&mut name[..n])?;
                if n > 0 && !domain_ok(&name[..n]) {
                    return Err(CfgError::BadRecord);
                }
            }
            Some(Resolver {
                addr,
                key: None,
                name,
                name_len,
            })
        }
        _ => return Err(CfgError::BadRecord),
    };
    if r.at != bytes.len() {
        return Err(CfgError::BadRecord);
    }
    Ok(Record {
        mode,
        addrs,
        addr_count,
        v4_gw,
        v6_gw,
        names,
        name_count,
        resolver,
    })
}

fn refused_resolver(addr: Ip) -> bool {
    matches!(addr, Ip::V4([10, 0, 2, 2] | [10, 0, 2, 3]))
}

struct Writer<'a> {
    buf: &'a mut [u8],
    at: usize,
}

impl Writer<'_> {
    fn bytes(&mut self, src: &[u8]) -> Result<(), CfgError> {
        if self.at + src.len() > self.buf.len() {
            return Err(CfgError::Full);
        }
        self.buf[self.at..self.at + src.len()].copy_from_slice(src);
        self.at += src.len();
        Ok(())
    }
    fn u8(&mut self, v: u8) -> Result<(), CfgError> {
        self.bytes(&[v])
    }
    fn u16(&mut self, v: u16) -> Result<(), CfgError> {
        self.bytes(&v.to_le_bytes())
    }
    fn u32(&mut self, v: u32) -> Result<(), CfgError> {
        self.bytes(&v.to_le_bytes())
    }
}

struct Reader<'a> {
    buf: &'a [u8],
    at: usize,
}

impl Reader<'_> {
    fn bytes(&mut self, dst: &mut [u8]) -> Result<(), CfgError> {
        if self.at + dst.len() > self.buf.len() {
            return Err(CfgError::BadRecord);
        }
        dst.copy_from_slice(&self.buf[self.at..self.at + dst.len()]);
        self.at += dst.len();
        Ok(())
    }
    fn u8(&mut self) -> Result<u8, CfgError> {
        let mut b = [0u8; 1];
        self.bytes(&mut b)?;
        Ok(b[0])
    }
    fn u16(&mut self) -> Result<u16, CfgError> {
        let mut b = [0u8; 2];
        self.bytes(&mut b)?;
        Ok(u16::from_le_bytes(b))
    }
    fn u32(&mut self) -> Result<u32, CfgError> {
        let mut b = [0u8; 4];
        self.bytes(&mut b)?;
        Ok(u32::from_le_bytes(b))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_and_grants() {
        let mut rec = Record::empty_ask();
        rec.addrs[0] = CfgAddr {
            ip: Ip::V4([10, 0, 2, 15]),
            prefix: 24,
            expiry: 42,
        };
        rec.addr_count = 1;
        rec.v4_gw = Some(Ip::V4([10, 0, 2, 2]));
        let mut ip = [0u8; 16];
        ip[..4].copy_from_slice(&[10, 0, 2, 2]);
        rec.allow(b"echo", 4582, KIND_IPV4, ip, [0xAB; 32]).unwrap();
        rec.allow(b"docs", 4594, KIND_DNS, [0; 16], [0xCD; 32])
            .unwrap();
        rec.allow(b"peer", 4597, KIND_DGRAM, ip, [0; 32]).unwrap();
        rec.dns(Ip::V4([172, 30, 0, 2]), Some([0x11; 32])).unwrap();
        assert!(rec.dns(Ip::V4([10, 0, 2, 2]), Some([0x11; 32])).is_err());
        assert!(rec.dns(Ip::V4([10, 0, 2, 3]), None).is_err());
        assert_eq!(rec.resolver.unwrap().key, Some([0x11; 32]));
        let mut keyed = [0u8; 1024];
        let kn = encode(&rec, &mut keyed).unwrap();
        assert_eq!(decode(&keyed[..kn]).unwrap(), rec);
        rec.dns(Ip::V4([172, 30, 0, 9]), None).unwrap();
        assert!(rec.resolver.unwrap().key.is_none());
        assert_eq!(rec.resolver.unwrap().name_len, 0);
        rec.dns_domain(b"one.one.one.one").unwrap();
        assert_eq!(
            &rec.resolver.unwrap().name[..rec.resolver.unwrap().name_len as usize],
            b"one.one.one.one"
        );
        assert!(rec.dns_domain(b"*.evil.test").is_err());
        assert!(rec.dns_domain(b"localhost").is_err());
        rec.grant(b"echo", b"admin").unwrap();
        assert!(rec.may_dial(b"echo", b"admin"));
        assert!(!rec.may_dial(b"echo", b"other"));
        let mut buf = [0u8; 1024];
        let n = encode(&rec, &mut buf).unwrap();
        let back = decode(&buf[..n]).unwrap();
        assert_eq!(back, rec);
        rec.revoke(b"echo", b"admin").unwrap();
        assert!(!rec.may_dial(b"echo", b"admin"));
        let legacy = &buf[..n - 1 - "one.one.one.one".len()];
        let old = decode(legacy).unwrap();
        assert_eq!(old.resolver.unwrap().name_len, 0);
        assert_eq!(old.resolver.unwrap().addr, Ip::V4([172, 30, 0, 9]));
        buf[4] = 2;
        buf[5] = 0;
        assert_eq!(decode(&buf[..n]), Err(CfgError::BadRecord));
    }

    #[test]
    fn an_empty_pin_can_be_remembered_once() {
        let mut rec = Record::empty_ask();
        let mut ip = [0u8; 16];
        ip[..4].copy_from_slice(&[192, 168, 0, 2]);
        rec.allow(b"ssh", 22, KIND_IPV4, ip, [0; 32]).unwrap();
        let key = [0x44; 32];
        rec.remember_key(b"ssh", key).unwrap();
        assert_eq!(rec.find(b"ssh").unwrap().key, key);
        assert!(rec.remember_key(b"ssh", [0x55; 32]).is_err());
        assert_eq!(rec.find(b"ssh").unwrap().key, key);
        assert!(rec.remember_key(b"ssh", [0; 32]).is_err());
        assert!(rec.remember_key(b"missing", key).is_err());
    }

    #[test]
    fn unknown_version_is_refused() {
        let mut buf = [0u8; 64];
        let n = encode(&Record::empty_ask(), &mut buf).unwrap();
        buf[4] = 1;
        buf[5] = 0;
        assert_eq!(decode(&buf[..n]), Err(CfgError::BadRecord));
        buf[4] = 99;
        assert_eq!(decode(&buf[..n]), Err(CfgError::BadRecord));
    }
}
