//! The firewall table.
//!
//! One line is one verb, one object, and one subject. `net` and `usb`
//! match a kernel stamp against these lines. The kernel does not parse
//! them.

#![no_std]

use galexy_abi::{SEC_NAME_MAX, STAMP_ENTRY, STAMP_NAME, STAMP_PARENT};

/// No galfs object. Ramdisk programs stamp this.
pub const NO_OBJECT: u32 = u32::MAX;

pub const VERB_DIAL: u8 = 1;
pub const VERB_LISTEN: u8 = 2;
pub const VERB_ICMP: u8 = 3;
pub const VERB_USB: u8 = 4;
/// One NTP server. The object is `*`, one IPv4 address, or one DNS
/// name. The port is 123 and is not written in the line.
pub const VERB_TIME: u8 = 5;

const SUB_ID: u8 = 1;
const SUB_PARENT: u8 = 2;
const SUB_NAME: u8 = 3;

/// Who the kernel says the caller is. Copied from the Send header.
#[derive(Clone, Copy)]
pub struct Stamp {
    pub entry: u32,
    pub parent: u32,
    pub name: [u8; SEC_NAME_MAX],
    pub name_len: u8,
}

impl Stamp {
    pub fn from_header(msg: &[u8]) -> Option<Self> {
        if msg.len() < STAMP_NAME + SEC_NAME_MAX {
            return None;
        }
        let entry = u32::from_le_bytes(msg[STAMP_ENTRY..STAMP_ENTRY + 4].try_into().ok()?);
        let parent = u32::from_le_bytes(msg[STAMP_PARENT..STAMP_PARENT + 4].try_into().ok()?);
        let name_len = msg[2].min(SEC_NAME_MAX as u8);
        let mut name = [0u8; SEC_NAME_MAX];
        name[..name_len as usize].copy_from_slice(&msg[STAMP_NAME..STAMP_NAME + name_len as usize]);
        Some(Self {
            entry,
            parent,
            name,
            name_len,
        })
    }
}

#[derive(Clone, Copy)]
struct Rule {
    verb: u8,
    object: [u8; 64],
    object_len: u8,
    subject_kind: u8,
    subject_id: u32,
    subject_name: [u8; SEC_NAME_MAX],
    subject_name_len: u8,
}

/// Whether any line in `text` lets `stamp` perform `verb` on `object`.
pub fn allows(text: &[u8], stamp: &Stamp, verb: u8, object: &[u8]) -> bool {
    let mut rest = text;
    while !rest.is_empty() {
        let end = rest.iter().position(|b| *b == b'\n').unwrap_or(rest.len());
        let line = trim(&rest[..end]);
        if let Some(rule) = parse_line(line) {
            if rule.verb == verb && subject_ok(&rule, stamp) && object_ok(&rule, object) {
                return true;
            }
        }
        rest = if end == rest.len() {
            b""
        } else {
            &rest[end + 1..]
        };
    }
    false
}

/// `true` when `line` is one rule. A dial star and a bare subject star
/// are refused here.
pub fn valid_line(line: &[u8]) -> bool {
    parse_line(trim(line)).is_some()
}

fn parse_line(line: &[u8]) -> Option<Rule> {
    let mut words = Words::new(line);
    let kind = words.next()?;
    let verb_word = words.next()?;
    let object = words.next()?;
    let sub_kind = words.next()?;
    let sub_value = words.next()?;
    if words.next().is_some() || object.len() > 64 || object.is_empty() {
        return None;
    }
    let verb = match (kind, verb_word) {
        (b"net", b"dial") => VERB_DIAL,
        (b"net", b"listen") => VERB_LISTEN,
        (b"net", b"icmp") => VERB_ICMP,
        (b"net", b"time") => VERB_TIME,
        (b"usb", b"use") => VERB_USB,
        _ => return None,
    };
    if !object_shape(verb, object) {
        return None;
    }
    let (subject_kind, subject_id, subject_name, subject_name_len) = match sub_kind {
        b"id" => (SUB_ID, parse_u32(sub_value)?, [0u8; SEC_NAME_MAX], 0),
        b"parent" => (SUB_PARENT, parse_u32(sub_value)?, [0u8; SEC_NAME_MAX], 0),
        b"name" => {
            if sub_value.is_empty() || sub_value.len() > SEC_NAME_MAX || sub_value.contains(&b'/') {
                return None;
            }
            let mut name = [0u8; SEC_NAME_MAX];
            name[..sub_value.len()].copy_from_slice(sub_value);
            (SUB_NAME, 0, name, sub_value.len() as u8)
        }
        _ => return None,
    };
    let mut stored = [0u8; 64];
    stored[..object.len()].copy_from_slice(object);
    Some(Rule {
        verb,
        object: stored,
        object_len: object.len() as u8,
        subject_kind,
        subject_id,
        subject_name,
        subject_name_len,
    })
}

fn object_shape(verb: u8, object: &[u8]) -> bool {
    match verb {
        VERB_DIAL => dial_shape(object),
        VERB_LISTEN => listen_shape(object),
        VERB_ICMP => object == b"*" || ipv4(object).is_some(),
        VERB_TIME => object == b"*" || ipv4(object).is_some() || time_name(object),
        VERB_USB => usb_id(object),
        _ => false,
    }
}

fn dial_shape(object: &[u8]) -> bool {
    let Some((host, port)) = split_port(object) else {
        return false;
    };
    if port == 0 || host.is_empty() || host == b"*" {
        return false;
    }
    if let Some(rest) = host.strip_prefix(b"*.") {
        return !rest.is_empty() && !rest.contains(&b'*') && !rest.starts_with(b".");
    }
    !host.contains(&b'*')
}

fn listen_shape(object: &[u8]) -> bool {
    let Some((host, port)) = split_port(object) else {
        return false;
    };
    port != 0 && (host == b"*" || ipv4(host).is_some())
}

fn subject_ok(rule: &Rule, stamp: &Stamp) -> bool {
    match rule.subject_kind {
        SUB_ID => stamp.entry != NO_OBJECT && stamp.entry == rule.subject_id,
        SUB_PARENT => stamp.entry != NO_OBJECT && stamp.parent == rule.subject_id,
        SUB_NAME => {
            // A name is a ramdisk program. A file keeps its object id,
            // so a desktop file named `ssh` does not match `name ssh`.
            if stamp.entry != NO_OBJECT {
                return false;
            }
            let n = rule.subject_name_len as usize;
            stamp.name_len as usize == n && stamp.name[..n] == rule.subject_name[..n]
        }
        _ => false,
    }
}

fn object_ok(rule: &Rule, object: &[u8]) -> bool {
    let want = &rule.object[..rule.object_len as usize];
    match rule.verb {
        VERB_DIAL => dial_match(want, object),
        VERB_LISTEN => listen_match(want, object),
        VERB_ICMP | VERB_TIME => want == b"*" || want == object,
        VERB_USB => eq_ignore_hex(want, object),
        _ => false,
    }
}

fn dial_match(rule: &[u8], object: &[u8]) -> bool {
    let (rule_host, rule_port) = match split_port(rule) {
        Some(v) => v,
        None => return false,
    };
    let (host, port) = match split_port(object) {
        Some(v) => v,
        None => return false,
    };
    if rule_port != port {
        return false;
    }
    if let Some(suffix) = rule_host.strip_prefix(b"*.") {
        let Some(rest) = host.strip_suffix(suffix) else {
            return false;
        };
        let Some(label) = rest.strip_suffix(b".") else {
            return false;
        };
        return !label.is_empty() && !label.contains(&b'.') && !label.contains(&b'*');
    }
    rule_host == host
}

fn listen_match(rule: &[u8], object: &[u8]) -> bool {
    let (rule_host, rule_port) = match split_port(rule) {
        Some(v) => v,
        None => return false,
    };
    let (host, port) = match split_port(object) {
        Some(v) => v,
        None => return rule == object,
    };
    if rule_port != port {
        return false;
    }
    rule_host == b"*" || rule_host == host
}

fn split_port(text: &[u8]) -> Option<(&[u8], u16)> {
    let colon = text.iter().rposition(|b| *b == b':')?;
    let port = parse_u16(&text[colon + 1..])?;
    Some((&text[..colon], port))
}

/// A DNS name on a `net time` line. No port and no star. A three-byte
/// name is not a name on the time opcode: four payload bytes are an
/// address.
fn time_name(name: &[u8]) -> bool {
    if name.is_empty() || name.len() == 3 || name.len() > 64 {
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

fn ipv4(text: &[u8]) -> Option<[u8; 4]> {
    let mut out = [0u8; 4];
    let mut part = 0usize;
    let mut acc = 0u16;
    let mut digits = 0u8;
    for &b in text {
        if b == b'.' {
            if digits == 0 || part == 3 {
                return None;
            }
            out[part] = acc as u8;
            part += 1;
            acc = 0;
            digits = 0;
            continue;
        }
        if !b.is_ascii_digit() {
            return None;
        }
        acc = acc.checked_mul(10)?.checked_add((b - b'0') as u16)?;
        if acc > 255 {
            return None;
        }
        digits += 1;
    }
    if part != 3 || digits == 0 {
        return None;
    }
    out[3] = acc as u8;
    Some(out)
}

fn usb_id(text: &[u8]) -> bool {
    let Some(colon) = text.iter().position(|b| *b == b':') else {
        return false;
    };
    hex4(&text[..colon]) && hex4(&text[colon + 1..])
}

fn hex4(text: &[u8]) -> bool {
    text.len() == 4 && text.iter().all(|b| b.is_ascii_hexdigit())
}

fn eq_ignore_hex(a: &[u8], b: &[u8]) -> bool {
    a.eq_ignore_ascii_case(b)
}

fn parse_u32(text: &[u8]) -> Option<u32> {
    if text.is_empty() || text.len() > 10 {
        return None;
    }
    let mut n = 0u32;
    for &b in text {
        if !b.is_ascii_digit() {
            return None;
        }
        n = n.checked_mul(10)?.checked_add((b - b'0') as u32)?;
    }
    Some(n)
}

fn parse_u16(text: &[u8]) -> Option<u16> {
    let n = parse_u32(text)?;
    u16::try_from(n).ok().filter(|n| *n != 0 || text == b"0")
}

fn trim(text: &[u8]) -> &[u8] {
    let start = text.iter().position(|b| *b != b' ').unwrap_or(text.len());
    let end = text
        .iter()
        .rposition(|b| *b != b' ' && *b != b'\r')
        .map(|i| i + 1)
        .unwrap_or(start);
    if start > end {
        b""
    } else {
        &text[start..end]
    }
}

struct Words<'a> {
    rest: &'a [u8],
}

impl<'a> Words<'a> {
    fn new(text: &'a [u8]) -> Self {
        Self { rest: text }
    }

    fn next(&mut self) -> Option<&'a [u8]> {
        self.rest = trim(self.rest);
        if self.rest.is_empty() {
            return None;
        }
        let end = self
            .rest
            .iter()
            .position(|b| *b == b' ')
            .unwrap_or(self.rest.len());
        let word = &self.rest[..end];
        self.rest = &self.rest[end..];
        Some(word)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stamp_name(name: &str) -> Stamp {
        let mut stamp = Stamp {
            entry: NO_OBJECT,
            parent: NO_OBJECT,
            name: [0; SEC_NAME_MAX],
            name_len: name.len() as u8,
        };
        stamp.name[..name.len()].copy_from_slice(name.as_bytes());
        stamp
    }

    #[test]
    fn dial_matches_one_label() {
        let text = b"net dial *.google.com:80 id 4\n";
        let mut stamp = stamp_name("python");
        stamp.entry = 4;
        stamp.parent = 2;
        assert!(allows(text, &stamp, VERB_DIAL, b"www.google.com:80"));
        assert!(!allows(text, &stamp, VERB_DIAL, b"google.com:80"));
        assert!(!allows(text, &stamp, VERB_DIAL, b"www.google.com:443"));
        assert!(!allows(text, &stamp, VERB_DIAL, b"a.b.google.com:80"));
    }

    #[test]
    fn dial_refuses_a_bare_star() {
        assert!(!valid_line(b"net dial *:80 name ping"));
        assert!(!valid_line(b"net dial * name ping"));
        assert!(valid_line(b"net icmp * name ping"));
        assert!(valid_line(b"net listen *:22 name ssh"));
    }

    #[test]
    fn folder_rule_matches_a_child() {
        let text = b"net dial 10.0.2.2:22 parent 7\n";
        let mut stamp = stamp_name("dial.py");
        stamp.entry = 11;
        stamp.parent = 7;
        assert!(allows(text, &stamp, VERB_DIAL, b"10.0.2.2:22"));
        stamp.parent = 8;
        assert!(!allows(text, &stamp, VERB_DIAL, b"10.0.2.2:22"));
    }

    #[test]
    fn time_names_one_address() {
        let text = b"net time 10.0.2.2 name shell\nnet time * name shell2\n";
        assert!(allows(text, &stamp_name("shell"), VERB_TIME, b"10.0.2.2"));
        assert!(!allows(text, &stamp_name("shell"), VERB_TIME, b"1.2.3.4"));
        assert!(!allows(text, &stamp_name("ping"), VERB_TIME, b"10.0.2.2"));
        assert!(allows(text, &stamp_name("shell2"), VERB_TIME, b"9.9.9.9"));
        assert!(valid_line(b"net time * name shell"));
        assert!(!valid_line(b"net time *:123 name shell"));
        assert!(valid_line(b"net time time.example name shell"));
        assert!(valid_line(b"net time cloudflare-dns.com name shell"));
        assert!(!valid_line(b"net time *.example name shell"));
        assert!(!valid_line(b"net time time.example:123 name shell"));
        let named = b"net time cloudflare-dns.com name shell\n";
        assert!(allows(
            named,
            &stamp_name("shell"),
            VERB_TIME,
            b"cloudflare-dns.com"
        ));
        assert!(!allows(named, &stamp_name("shell"), VERB_TIME, b"1.1.1.1"));
        assert!(!allows(
            named,
            &stamp_name("shell"),
            VERB_TIME,
            b"time.example"
        ));
    }

    #[test]
    fn name_does_not_cover_another_program() {
        let text = b"net icmp * name ping\n";
        assert!(allows(text, &stamp_name("ping"), VERB_ICMP, b"10.0.2.2"));
        assert!(!allows(text, &stamp_name("shell"), VERB_ICMP, b"10.0.2.2"));
        let mut file = stamp_name("ping");
        file.entry = 9;
        assert!(!allows(text, &file, VERB_ICMP, b"10.0.2.2"));
    }
}
