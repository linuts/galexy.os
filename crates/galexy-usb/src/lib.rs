//! Device and configuration descriptors, and the one driver walk.
//!
//! The kernel moves transfer rings. This crate decides whether a
//! descriptor is well formed and which table row, if any, claims it.
//! A function that is not in the table is not called.

#![cfg_attr(not(test), no_std)]

/// Configuration descriptors longer than this are `BadValue`.
pub const CFG_MAX: usize = 4096;
/// Product strings keep this many bytes after UTF-16 decode.
pub const STR_MAX: usize = 64;

/// Identifiers a row can match. Class is the device class, or the first
/// interface class when the device class is zero.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Ids {
    pub vendor: u16,
    pub product: u16,
    pub class: u8,
    pub sub: u8,
    pub proto: u8,
}

/// Why a descriptor was refused. The rest of the bus stays up.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Bad {
    Truncated,
    Length,
    String,
}

/// One row in the driver table.
pub struct Driver {
    pub id: Match,
    pub module: &'static str,
    /// Called only when [`Match`] hits. Not called for any other device.
    pub claim: fn(&Ids),
}

/// How a row picks a device.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Match {
    VendorProduct(u16, u16),
    Class(u8, u8, u8),
}

impl Match {
    fn hits(self, ids: &Ids) -> bool {
        match self {
            Match::VendorProduct(v, p) => ids.vendor == v && ids.product == p,
            Match::Class(c, s, p) => ids.class == c && ids.sub == s && ids.proto == p,
        }
    }
}

/// Walk `table` once. Each device is claimed by the first hit, or handed
/// to `unclaimed` when no row matches. Removing a row means its `claim`
/// function is not called.
pub fn dispatch(devs: &[Ids], table: &[Driver], mut unclaimed: impl FnMut(&Ids)) {
    for dev in devs {
        if let Some(row) = table.iter().find(|row| row.id.hits(dev)) {
            (row.claim)(dev);
        } else {
            unclaimed(dev);
        }
    }
}

/// Vendor, product, and class from a device descriptor plus the first
/// configuration. `cfg` may be empty when the configuration was refused.
pub fn ids_of(dev: &[u8], cfg: &[u8]) -> Result<Ids, Bad> {
    if dev.len() < 18 || dev[0] < 18 || dev[1] != 1 {
        return Err(Bad::Truncated);
    }
    let mut class = dev[4];
    let mut sub = dev[5];
    let mut proto = dev[6];
    if class == 0 {
        if let Some((c, s, p)) = interface_class(cfg) {
            class = c;
            sub = s;
            proto = p;
        }
    }
    Ok(Ids {
        vendor: u16::from_le_bytes([dev[8], dev[9]]),
        product: u16::from_le_bytes([dev[10], dev[11]]),
        class,
        sub,
        proto,
    })
}

/// Accept a configuration descriptor or refuse it.
///
/// `declared` is `wTotalLength`. A value above [`CFG_MAX`], or a buffer
/// shorter than the descriptor's own length, is [`Bad`].
pub fn check_config(cfg: &[u8], declared: usize) -> Result<(), Bad> {
    if declared > CFG_MAX {
        return Err(Bad::Length);
    }
    if cfg.is_empty() {
        return Err(Bad::Truncated);
    }
    let need = cfg[0] as usize;
    if need < 9 || cfg.len() < need || cfg[1] != 2 {
        return Err(Bad::Truncated);
    }
    if declared < need {
        return Err(Bad::Truncated);
    }
    Ok(())
}

/// A USB string descriptor: length, type 3, then UTF-16LE. The decoded
/// prefix is what the list line prints.
pub fn decode_string(raw: &[u8], out: &mut [u8]) -> Result<usize, Bad> {
    if raw.len() < 2 || raw[0] < 2 || raw[1] != 3 {
        return Err(Bad::String);
    }
    // A stored prefix may be shorter than `bLength`. Decode what is here.
    let n = (raw[0] as usize).min(raw.len());
    if !n.is_multiple_of(2) {
        return Err(Bad::String);
    }
    let mut w = 0usize;
    let mut i = 2;
    while i + 1 < n && w < out.len() && w < STR_MAX {
        if raw[i + 1] != 0 {
            out[w] = b'?';
        } else {
            let b = raw[i];
            out[w] = if b.is_ascii_graphic() || b == b' ' {
                b
            } else {
                b'?'
            };
        }
        w += 1;
        i += 2;
    }
    Ok(w)
}

/// One list line. `name` is the decoded product string.
pub fn list_line(port: u8, addr: u8, ids: &Ids, name: &str, out: &mut [u8]) -> usize {
    let mut tmp = [0u8; 96];
    let mut n = 0usize;
    n += put_dec(&mut tmp[n..], port as u32);
    push(&mut tmp, &mut n, b' ');
    n += put_hex(&mut tmp[n..], addr as u32, 2);
    push(&mut tmp, &mut n, b' ');
    n += put_hex(&mut tmp[n..], ids.vendor as u32, 4);
    push(&mut tmp, &mut n, b':');
    n += put_hex(&mut tmp[n..], ids.product as u32, 4);
    push(&mut tmp, &mut n, b' ');
    n += put_hex(&mut tmp[n..], ids.class as u32, 2);
    push(&mut tmp, &mut n, b' ');
    n += put_hex(&mut tmp[n..], ids.sub as u32, 2);
    push(&mut tmp, &mut n, b' ');
    n += put_hex(&mut tmp[n..], ids.proto as u32, 2);
    push(&mut tmp, &mut n, b' ');
    for b in name.bytes().take(STR_MAX) {
        if n >= tmp.len() - 1 {
            break;
        }
        tmp[n] = b;
        n += 1;
    }
    if n < tmp.len() {
        tmp[n] = b'\n';
        n += 1;
    }
    let take = n.min(out.len());
    out[..take].copy_from_slice(&tmp[..take]);
    take
}

/// How many interface descriptors the first configuration carries.
///
/// More than eight is [`Bad::Length`]. A truncated descriptor is
/// [`Bad::Truncated`].
pub fn interface_count(cfg: &[u8]) -> Result<u8, Bad> {
    check_config(cfg, cfg.len())?;
    let mut i = cfg[0] as usize;
    let mut n = 0u8;
    while i + 1 < cfg.len() {
        let len = cfg[i] as usize;
        if len < 2 || i + len > cfg.len() {
            return Err(Bad::Truncated);
        }
        if cfg[i + 1] == 4 {
            n = n.saturating_add(1);
            if n > 8 {
                return Err(Bad::Length);
            }
        }
        i += len;
    }
    Ok(n)
}

fn interface_class(cfg: &[u8]) -> Option<(u8, u8, u8)> {
    if check_config(cfg, cfg.len()).is_err() {
        return None;
    }
    let mut i = cfg[0] as usize;
    while i + 2 < cfg.len() {
        let len = cfg[i] as usize;
        if len < 2 || i + len > cfg.len() {
            return None;
        }
        if cfg[i + 1] == 4 && len >= 9 {
            return Some((cfg[i + 5], cfg[i + 6], cfg[i + 7]));
        }
        i += len;
    }
    None
}

fn push(buf: &mut [u8], n: &mut usize, b: u8) {
    if *n < buf.len() {
        buf[*n] = b;
        *n += 1;
    }
}

fn put_dec(out: &mut [u8], mut v: u32) -> usize {
    let mut tmp = [0u8; 10];
    let mut i = 10;
    if v == 0 {
        out[0] = b'0';
        return 1;
    }
    while v > 0 {
        i -= 1;
        tmp[i] = b'0' + (v % 10) as u8;
        v /= 10;
    }
    let n = 10 - i;
    out[..n].copy_from_slice(&tmp[i..]);
    n
}

fn put_hex(out: &mut [u8], v: u32, width: usize) -> usize {
    const HEX: &[u8] = b"0123456789abcdef";
    let mut i = 0;
    while i < width && i < out.len() {
        let shift = (width - 1 - i) * 4;
        out[i] = HEX[((v >> shift) & 0xf) as usize];
        i += 1;
    }
    i
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn keyboard() -> Ids {
        Ids {
            vendor: 0x0627,
            product: 0x0001,
            class: 3,
            sub: 1,
            proto: 1,
        }
    }

    static FAKE_CALLS: AtomicUsize = AtomicUsize::new(0);
    static OTHER_CALLS: AtomicUsize = AtomicUsize::new(0);

    fn fake(_: &Ids) {
        FAKE_CALLS.fetch_add(1, Ordering::SeqCst);
    }

    fn other(_: &Ids) {
        OTHER_CALLS.fetch_add(1, Ordering::SeqCst);
    }

    #[test]
    fn fake_row_claims_one_device_and_leaves_the_other() {
        FAKE_CALLS.store(0, Ordering::SeqCst);
        OTHER_CALLS.store(0, Ordering::SeqCst);
        let table = [Driver {
            id: Match::VendorProduct(0x0627, 0x0001),
            module: "fake",
            claim: fake,
        }];
        let devs = [
            keyboard(),
            Ids {
                vendor: 0x1234,
                product: 0x5678,
                class: 0,
                sub: 0,
                proto: 0,
            },
        ];
        let mut free = 0;
        dispatch(&devs, &table, |_| free += 1);
        assert_eq!(FAKE_CALLS.load(Ordering::SeqCst), 1);
        assert_eq!(OTHER_CALLS.load(Ordering::SeqCst), 0);
        assert_eq!(free, 1);
    }

    #[test]
    fn removing_the_row_does_not_call_the_module() {
        FAKE_CALLS.store(0, Ordering::SeqCst);
        let mut free = 0;
        dispatch(&[keyboard()], &[], |_| free += 1);
        assert_eq!(FAKE_CALLS.load(Ordering::SeqCst), 0);
        assert_eq!(free, 1);
        let _ = other;
    }

    #[test]
    fn hostile_config_length_is_refused() {
        let cfg = [9u8, 2, 0, 0x20, 1, 1, 0, 0x80, 50];
        assert_eq!(check_config(&cfg, 0x2000), Err(Bad::Length));
    }

    #[test]
    fn truncated_device_descriptor_is_refused() {
        assert_eq!(ids_of(&[18, 1, 0, 2], &[]), Err(Bad::Truncated));
    }

    #[test]
    fn a_bad_string_shape_is_refused() {
        assert_eq!(
            decode_string(&[4, 2, 0x41, 0], &mut [0; 8]),
            Err(Bad::String)
        );
        assert_eq!(decode_string(&[3, 3, 0x41], &mut [0; 8]), Err(Bad::String));
    }

    #[test]
    fn keyboard_ids_come_from_the_interface() {
        let mut dev = [0u8; 18];
        dev[0] = 18;
        dev[1] = 1;
        dev[8] = 0x27;
        dev[9] = 0x06;
        dev[10] = 0x01;
        let mut cfg = [0u8; 18];
        cfg[0] = 9;
        cfg[1] = 2;
        cfg[2] = 18;
        cfg[9] = 9;
        cfg[10] = 4;
        cfg[14] = 3;
        cfg[15] = 1;
        cfg[16] = 1;
        let ids = ids_of(&dev, &cfg).unwrap();
        assert_eq!(ids.vendor, 0x0627);
        assert_eq!(ids.product, 1);
        assert_eq!((ids.class, ids.sub, ids.proto), (3, 1, 1));
        let mut line = [0u8; 64];
        let n = list_line(1, 1, &ids, "QEMU USB Keyboard", &mut line);
        let text = core::str::from_utf8(&line[..n]).unwrap();
        assert_eq!(text, "1 01 0627:0001 03 01 01 QEMU USB Keyboard\n");
    }
}
