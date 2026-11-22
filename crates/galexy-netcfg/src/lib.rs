//! Version 2 bytes for the `/net` record.
//!
//! Magic, version, mode, an optional learned lease, the directory, then
//! the pinned resolver. A version this crate does not know is an error.
//! Callers do not guess. Version 1 is refused.

#![cfg_attr(not(test), no_std)]

/// File magic, ASCII `GNET`.
pub const MAGIC: &[u8; 4] = b"GNET";
/// Record version. Unknown versions, including version 1, are refused.
pub const VERSION: u16 = 2;
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
/// DNS-over-TLS port. The record does not store another one.
pub const DOT_PORT: u16 = 853;
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

/// Address learned from DHCP, or stored for fixed mode.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Lease {
    /// IPv4 address.
    pub addr: [u8; 4],
    /// Prefix length.
    pub prefix: u8,
    /// Gateway.
    pub gateway: [u8; 4],
    /// Seconds since the Unix epoch. `0` means none (fixed mode).
    pub expiry: u32,
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
    /// [`KIND_DNS`], [`KIND_IPV4`], [`KIND_IPV6`], [`KIND_ADDR`], or [`KIND_PORT`].
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

/// Pinned DNS-over-TLS resolver. The port is always [`DOT_PORT`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Resolver {
    /// Resolver address. Not the QEMU gateway and not `10.0.2.3`.
    pub addr: [u8; 4],
    /// Pinned key. Callers do not write these bytes to the console.
    pub key: [u8; 32],
}

/// The whole file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Record {
    /// [`MODE_ASK`] or [`MODE_FIXED`].
    pub mode: u8,
    /// Learned or configured locator.
    pub lease: Option<Lease>,
    /// Directory.
    pub names: [Name; MAX_NAMES],
    /// Live entries in [`Self::names`].
    pub name_count: u8,
    /// Pinned resolver. Absent means lookup fails closed.
    pub resolver: Option<Resolver>,
}

impl Record {
    /// Mode `ask`, no lease, empty directory. Init writes this once.
    pub const fn empty_ask() -> Self {
        Self {
            mode: MODE_ASK,
            lease: None,
            names: [Name::empty(); MAX_NAMES],
            name_count: 0,
            resolver: None,
        }
    }

    /// Stores the resolver. `10.0.2.2` and `10.0.2.3` are refused.
    pub fn pin(&mut self, addr: [u8; 4], key: [u8; 32]) -> Result<(), CfgError> {
        if addr == [10, 0, 2, 2] || addr == [10, 0, 2, 3] {
            return Err(CfgError::BadRecord);
        }
        self.resolver = Some(Resolver { addr, key });
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
    match record.lease {
        None => w.u8(0)?,
        Some(lease) => {
            w.u8(1)?;
            w.bytes(&lease.addr)?;
            w.u8(lease.prefix)?;
            w.bytes(&lease.gateway)?;
            w.u32(lease.expiry)?;
        }
    }
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
        Some(resolver) => {
            w.u8(1)?;
            w.bytes(&resolver.addr)?;
            w.bytes(&resolver.key)?;
        }
    }
    Ok(w.at)
}

fn kind_ok(kind: u8) -> bool {
    matches!(
        kind,
        KIND_DNS | KIND_IPV4 | KIND_IPV6 | KIND_ADDR | KIND_PORT
    )
}

/// Decodes a version-2 record. Any other version is [`CfgError::BadRecord`].
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
    let lease = if r.u8()? == 0 {
        None
    } else {
        let mut addr = [0u8; 4];
        let mut gateway = [0u8; 4];
        r.bytes(&mut addr)?;
        let prefix = r.u8()?;
        r.bytes(&mut gateway)?;
        let expiry = r.u32()?;
        Some(Lease {
            addr,
            prefix,
            gateway,
            expiry,
        })
    };
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
            let mut addr = [0u8; 4];
            let mut key = [0u8; 32];
            r.bytes(&mut addr)?;
            r.bytes(&mut key)?;
            if addr == [10, 0, 2, 2] || addr == [10, 0, 2, 3] {
                return Err(CfgError::BadRecord);
            }
            Some(Resolver { addr, key })
        }
        _ => return Err(CfgError::BadRecord),
    };
    if r.at != bytes.len() {
        return Err(CfgError::BadRecord);
    }
    Ok(Record {
        mode,
        lease,
        names,
        name_count,
        resolver,
    })
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
        rec.lease = Some(Lease {
            addr: [10, 0, 2, 15],
            prefix: 24,
            gateway: [10, 0, 2, 2],
            expiry: 42,
        });
        let mut ip = [0u8; 16];
        ip[..4].copy_from_slice(&[10, 0, 2, 2]);
        rec.allow(b"echo", 4582, KIND_IPV4, ip, [0xAB; 32]).unwrap();
        rec.allow(b"docs", 4594, KIND_DNS, [0; 16], [0xCD; 32])
            .unwrap();
        rec.pin([172, 30, 0, 2], [0x11; 32]).unwrap();
        assert!(rec.pin([10, 0, 2, 2], [0x11; 32]).is_err());
        assert!(rec.pin([10, 0, 2, 3], [0x11; 32]).is_err());
        rec.grant(b"echo", b"admin").unwrap();
        assert!(rec.may_dial(b"echo", b"admin"));
        assert!(!rec.may_dial(b"echo", b"other"));
        let mut buf = [0u8; 1024];
        let n = encode(&rec, &mut buf).unwrap();
        let back = decode(&buf[..n]).unwrap();
        assert_eq!(back, rec);
        rec.revoke(b"echo", b"admin").unwrap();
        assert!(!rec.may_dial(b"echo", b"admin"));
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
