//! One address. Four bytes or sixteen, distinguished by the kind.
//!
//! The shell and `net` both call [`parse_ip`]. A form that can be read
//! two ways is refused. Two spellings of one address are the same bytes.

/// An IPv4 or IPv6 address.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ip {
    /// Four octets.
    V4([u8; 4]),
    /// Sixteen octets.
    V6([u8; 16]),
}

impl Ip {
    /// True when this is [`Ip::V4`].
    pub fn is_v4(self) -> bool {
        matches!(self, Ip::V4(_))
    }

    /// True when this is a link-local IPv6 address (`fe80::/10`).
    pub fn is_link_local(self) -> bool {
        match self {
            Ip::V6(a) => a[0] == 0xfe && (a[1] & 0xc0) == 0x80,
            Ip::V4(_) => false,
        }
    }

    /// True for `ff00::/8`.
    pub fn is_multicast(self) -> bool {
        match self {
            Ip::V6(a) => a[0] == 0xff,
            Ip::V4(a) => a[0] >= 224 && a[0] <= 239,
        }
    }

    /// The sixteen stored bytes. IPv4 occupies the first four.
    pub fn octets16(self) -> [u8; 16] {
        match self {
            Ip::V4(a) => {
                let mut o = [0u8; 16];
                o[..4].copy_from_slice(&a);
                o
            }
            Ip::V6(a) => a,
        }
    }

    /// Kind byte: 4 or 6.
    pub fn kind(self) -> u8 {
        match self {
            Ip::V4(_) => 4,
            Ip::V6(_) => 6,
        }
    }

    /// Rebuilds an address from a record kind and sixteen bytes.
    pub fn from_kind(kind: u8, octets: [u8; 16]) -> Option<Self> {
        match kind {
            4 => Some(Ip::V4([octets[0], octets[1], octets[2], octets[3]])),
            6 => Some(Ip::V6(octets)),
            _ => None,
        }
    }
}

/// Link-local address formed from a MAC, modified EUI-64, prefix 64.
pub fn link_local_from_mac(mac: [u8; 6]) -> Ip {
    let mut ip = [0u8; 16];
    ip[0] = 0xfe;
    ip[1] = 0x80;
    ip[8] = mac[0] ^ 0x02;
    ip[9] = mac[1];
    ip[10] = mac[2];
    ip[11] = 0xff;
    ip[12] = 0xfe;
    ip[13] = mac[3];
    ip[14] = mac[4];
    ip[15] = mac[5];
    Ip::V6(ip)
}

/// Parses one address.
///
/// Refuses a v4 octet with a leading zero, a zone id, a second `::`,
/// an IPv4-mapped form, a dotted tail inside v6 text, `0.0.0.0`, and
/// `::`.
pub fn parse_ip(text: &[u8]) -> Option<Ip> {
    if text.is_empty() || text.contains(&b'%') {
        return None;
    }
    if text.contains(&b':') {
        let ip = parse_v6(text)?;
        if ip == [0u8; 16] || is_v4_mapped(&ip) {
            return None;
        }
        Some(Ip::V6(ip))
    } else {
        let ip = parse_v4(text)?;
        if ip == [0, 0, 0, 0] {
            return None;
        }
        Some(Ip::V4(ip))
    }
}

/// Writes the canonical text. IPv6 is lowercase, with the longest run of
/// zero hextets compressed and no leading zeros.
pub fn format_ip(ip: Ip, out: &mut [u8]) -> Option<usize> {
    match ip {
        Ip::V4(a) => {
            let mut tmp = [0u8; 15];
            let n = format_v4(a, &mut tmp);
            if n > out.len() {
                return None;
            }
            out[..n].copy_from_slice(&tmp[..n]);
            Some(n)
        }
        Ip::V6(a) => format_v6(&a, out),
    }
}

/// A prefix length this address can carry. IPv4 is 0..=32. IPv6 is 0..=128.
pub fn prefix_ok(ip: Ip, prefix: u8) -> bool {
    match ip {
        Ip::V4(_) => prefix <= 32,
        Ip::V6(_) => prefix <= 128,
    }
}

/// True when `addr` and `gateway` are the same family.
pub fn same_family(addr: Ip, gateway: Ip) -> bool {
    addr.is_v4() == gateway.is_v4()
}

/// True when `dest` is inside `addr`/`prefix`.
pub fn prefix_contains(addr: Ip, prefix: u8, dest: Ip) -> bool {
    match (addr, dest) {
        (Ip::V4(a), Ip::V4(d)) => {
            let bits = u32::from(prefix.min(32));
            if bits == 0 {
                return false;
            }
            let mask = if bits >= 32 {
                u32::MAX
            } else {
                u32::MAX << (32 - bits)
            };
            (u32::from_be_bytes(a) & mask) == (u32::from_be_bytes(d) & mask)
        }
        (Ip::V6(a), Ip::V6(d)) => {
            let bits = prefix.min(128) as usize;
            let full = bits / 8;
            let rem = bits % 8;
            if a[..full] != d[..full] {
                return false;
            }
            if rem == 0 {
                return true;
            }
            let mask = 0xffu8 << (8 - rem);
            (a[full] & mask) == (d[full] & mask)
        }
        _ => false,
    }
}

/// Writes an address argument. Four bytes are IPv4. IPv6 is a `6`, the
/// text length, and the canonical text.
pub fn encode_ip_arg(ip: Ip, out: &mut [u8]) -> Option<usize> {
    match ip {
        Ip::V4(a) => {
            if out.len() < 4 {
                return None;
            }
            out[..4].copy_from_slice(&a);
            Some(4)
        }
        Ip::V6(_) => {
            if out.len() < 2 {
                return None;
            }
            let mut text = [0u8; 64];
            let n = format_ip(ip, &mut text)?;
            if out.len() < 2 + n || n > 255 {
                return None;
            }
            out[0] = 6;
            out[1] = n as u8;
            out[2..2 + n].copy_from_slice(&text[..n]);
            Some(2 + n)
        }
    }
}

/// Reads an address argument written by [`encode_ip_arg`].
pub fn decode_ip_arg(buf: &[u8]) -> Option<(Ip, usize)> {
    if buf.first() == Some(&6) && buf.len() >= 2 {
        let n = buf[1] as usize;
        if buf.len() >= 2 + n {
            if let Some(Ip::V6(a)) = parse_ip(&buf[2..2 + n]) {
                return Some((Ip::V6(a), 2 + n));
            }
        }
    }
    if buf.len() >= 4 {
        return Some((Ip::V4([buf[0], buf[1], buf[2], buf[3]]), 4));
    }
    None
}

fn parse_v4(text: &[u8]) -> Option<[u8; 4]> {
    let mut ip = [0u8; 4];
    let mut n = 0usize;
    for part in text.split(|b| *b == b'.') {
        if n == 4 {
            return None;
        }
        ip[n] = parse_octet(part)?;
        n += 1;
    }
    if n == 4 {
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

fn parse_v6(text: &[u8]) -> Option<[u8; 16]> {
    if text.is_empty() || text.contains(&b'.') {
        return None;
    }
    let double = text.windows(2).position(|w| w == b"::");
    if let Some(at) = double {
        if text[at + 2..].windows(2).any(|w| w == b"::") {
            return None;
        }
    }
    let mut head = [0u16; 8];
    let mut tail = [0u16; 8];
    let (hn, tn) = if let Some(at) = double {
        let hn = parse_groups(&text[..at], &mut head)?;
        let tn = parse_groups(&text[at + 2..], &mut tail)?;
        if hn + tn > 7 {
            return None;
        }
        (hn, tn)
    } else {
        let hn = parse_groups(text, &mut head)?;
        if hn != 8 {
            return None;
        }
        (8, 0)
    };
    let mut groups = [0u16; 8];
    groups[..hn].copy_from_slice(&head[..hn]);
    let zeros = 8 - hn - tn;
    groups[hn + zeros..hn + zeros + tn].copy_from_slice(&tail[..tn]);
    let mut ip = [0u8; 16];
    for (i, g) in groups.iter().enumerate() {
        let b = g.to_be_bytes();
        ip[i * 2] = b[0];
        ip[i * 2 + 1] = b[1];
    }
    Some(ip)
}

fn parse_groups(text: &[u8], out: &mut [u16; 8]) -> Option<usize> {
    if text.is_empty() {
        return Some(0);
    }
    let mut n = 0usize;
    for part in text.split(|b| *b == b':') {
        if part.is_empty() || part.len() > 4 || n == 8 {
            return None;
        }
        let mut v = 0u16;
        for b in part {
            v = (v << 4) | u16::from(hex_val(*b)?);
        }
        out[n] = v;
        n += 1;
    }
    Some(n)
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

fn is_v4_mapped(ip: &[u8; 16]) -> bool {
    ip[..10].iter().all(|b| *b == 0) && ip[10] == 0xff && ip[11] == 0xff
}

fn format_v4(a: [u8; 4], out: &mut [u8]) -> usize {
    let mut n = 0usize;
    for (i, octet) in a.iter().enumerate() {
        if i > 0 {
            out[n] = b'.';
            n += 1;
        }
        n += write_dec(*octet, &mut out[n..]);
    }
    n
}

fn write_dec(v: u8, out: &mut [u8]) -> usize {
    if v >= 100 {
        out[0] = b'0' + v / 100;
        out[1] = b'0' + (v / 10) % 10;
        out[2] = b'0' + v % 10;
        3
    } else if v >= 10 {
        out[0] = b'0' + v / 10;
        out[1] = b'0' + v % 10;
        2
    } else {
        out[0] = b'0' + v;
        1
    }
}

fn format_v6(ip: &[u8; 16], out: &mut [u8]) -> Option<usize> {
    let mut g = [0u16; 8];
    for i in 0..8 {
        g[i] = u16::from_be_bytes([ip[i * 2], ip[i * 2 + 1]]);
    }
    let (zs, zl) = best_zero_run(&g);
    let mut tmp = [0u8; 48];
    let mut n = 0usize;
    let mut i = 0usize;
    while i < 8 {
        if zl >= 2 && i == zs {
            if n + 2 > tmp.len() {
                return None;
            }
            tmp[n] = b':';
            tmp[n + 1] = b':';
            n += 2;
            i += zl;
            continue;
        }
        if n > 0 && tmp[n - 1] != b':' {
            if n >= tmp.len() {
                return None;
            }
            tmp[n] = b':';
            n += 1;
        }
        n += write_hex(g[i], &mut tmp[n..]);
        i += 1;
    }
    if n > out.len() {
        return None;
    }
    out[..n].copy_from_slice(&tmp[..n]);
    Some(n)
}

fn best_zero_run(g: &[u16; 8]) -> (usize, usize) {
    let mut best_at = 0usize;
    let mut best_len = 0usize;
    let mut i = 0usize;
    while i < 8 {
        if g[i] == 0 {
            let start = i;
            while i < 8 && g[i] == 0 {
                i += 1;
            }
            let len = i - start;
            if len > best_len {
                best_at = start;
                best_len = len;
            }
        } else {
            i += 1;
        }
    }
    (best_at, best_len)
}

fn write_hex(v: u16, out: &mut [u8]) -> usize {
    let mut started = false;
    let mut n = 0usize;
    for shift in [12, 8, 4, 0] {
        let d = ((v >> shift) & 0xf) as u8;
        if d == 0 && !started && shift != 0 {
            continue;
        }
        started = true;
        out[n] = b"0123456789abcdef"[d as usize];
        n += 1;
    }
    if n == 0 {
        out[0] = b'0';
        1
    } else {
        n
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn v4_and_v6_spellings() {
        assert_eq!(parse_ip(b"10.0.2.15"), Some(Ip::V4([10, 0, 2, 15])));
        assert_eq!(parse_ip(b"01.2.3.4"), None);
        assert_eq!(parse_ip(b"0.0.0.0"), None);
        assert_eq!(parse_ip(b"::"), None);
        assert_eq!(parse_ip(b"fe80::1%eth0"), None);
        assert_eq!(parse_ip(b"fe80::1::2"), None);
        assert_eq!(parse_ip(b"::ffff:192.0.2.1"), None);
        assert_eq!(parse_ip(b"::192.0.2.1"), None);
        assert_eq!(parse_ip(b"::ffff:c000:201"), None);
        let a = parse_ip(b"2001:db8::1").unwrap();
        let b = parse_ip(b"2001:0db8:0:0:0:0:0:1").unwrap();
        assert_eq!(a, b);
        let mut buf = [0u8; 64];
        let n = format_ip(a, &mut buf).unwrap();
        assert_eq!(&buf[..n], b"2001:db8::1");
        let ll = link_local_from_mac([0x52, 0x54, 0x00, 0x12, 0x34, 0x56]);
        let n = format_ip(ll, &mut buf).unwrap();
        assert_eq!(&buf[..n], b"fe80::5054:ff:fe12:3456");
        assert!(ll.is_link_local());
    }

    #[test]
    fn argument_round_trip() {
        let mut buf = [0u8; 64];
        let v4 = Ip::V4([10, 0, 2, 15]);
        let n = encode_ip_arg(v4, &mut buf).unwrap();
        assert_eq!(n, 4);
        assert_eq!(decode_ip_arg(&buf[..n]), Some((v4, 4)));
        let v6 = parse_ip(b"fe80::1").unwrap();
        let n = encode_ip_arg(v6, &mut buf).unwrap();
        assert_eq!(decode_ip_arg(&buf[..n]), Some((v6, n)));
        assert!(prefix_contains(v4, 24, Ip::V4([10, 0, 2, 2])));
        assert!(!prefix_contains(v4, 24, Ip::V4([10, 1, 2, 2])));
    }
}
