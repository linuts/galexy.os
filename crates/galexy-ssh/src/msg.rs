//! SSH message numbers this subset uses, and the few messages it builds.

use zeroize::Zeroize;

pub const DISCONNECT: u8 = 1;
pub const KEXINIT: u8 = 20;
pub const NEWKEYS: u8 = 21;
pub const KEX_ECDH_INIT: u8 = 30;
pub const KEX_ECDH_REPLY: u8 = 31;
pub const SERVICE_REQUEST: u8 = 5;
pub const SERVICE_ACCEPT: u8 = 6;
pub const USERAUTH_REQUEST: u8 = 50;
pub const USERAUTH_FAILURE: u8 = 51;
pub const USERAUTH_SUCCESS: u8 = 52;
pub const CHANNEL_OPEN: u8 = 90;
pub const CHANNEL_OPEN_CONFIRMATION: u8 = 91;
pub const CHANNEL_DATA: u8 = 94;
pub const CHANNEL_REQUEST: u8 = 98;
pub const CHANNEL_SUCCESS: u8 = 99;

pub const CLIENT_KEX: &str = "curve25519-sha256,kex-strict-c-v00@openssh.com";
pub const SERVER_KEX: &str = "curve25519-sha256,kex-strict-s-v00@openssh.com";
pub const HOSTKEY: &str = "ssh-ed25519";
pub const CIPHER: &str = "chacha20-poly1305@openssh.com";
pub const STRICT_CLIENT: &str = "kex-strict-c-v00@openssh.com";
pub const STRICT_SERVER: &str = "kex-strict-s-v00@openssh.com";
pub const CURVE: &str = "curve25519-sha256";

/// One decoded SSH message. Strings are counted from the type byte.
pub struct Msg {
    pub kind: u8,
    pub body: Vec<u8>,
}

impl Msg {
    pub fn simple(kind: u8) -> Self {
        Self {
            kind,
            body: vec![kind],
        }
    }

    pub fn from_payload(payload: Vec<u8>) -> Option<Self> {
        let kind = *payload.first()?;
        Some(Self {
            kind,
            body: payload,
        })
    }

    pub fn service(kind: u8, name: &str) -> Self {
        let mut body = vec![kind];
        push_str(&mut body, name.as_bytes());
        Self { kind, body }
    }

    pub fn auth_failure() -> Self {
        let mut body = vec![USERAUTH_FAILURE];
        push_str(&mut body, b"password");
        body.push(0);
        Self {
            kind: USERAUTH_FAILURE,
            body,
        }
    }

    pub fn password_request(user: &str, password: &[u8]) -> Self {
        let mut body = vec![USERAUTH_REQUEST];
        push_str(&mut body, user.as_bytes());
        push_str(&mut body, b"ssh-connection");
        push_str(&mut body, b"password");
        body.push(0);
        push_str(&mut body, password);
        Self {
            kind: USERAUTH_REQUEST,
            body,
        }
    }

    pub fn kexinit(cookie: &[u8; 16], kex: &str) -> Self {
        let mut body = vec![KEXINIT];
        body.extend_from_slice(cookie);
        push_str(&mut body, kex.as_bytes());
        push_str(&mut body, HOSTKEY.as_bytes());
        push_str(&mut body, CIPHER.as_bytes());
        push_str(&mut body, CIPHER.as_bytes());
        push_str(&mut body, b"");
        push_str(&mut body, b"");
        push_str(&mut body, b"none");
        push_str(&mut body, b"none");
        push_str(&mut body, b"");
        push_str(&mut body, b"");
        body.push(0);
        body.extend_from_slice(&0u32.to_be_bytes());
        Self {
            kind: KEXINIT,
            body,
        }
    }

    pub fn ecdh_init(public: &[u8; 32]) -> Self {
        let mut body = vec![KEX_ECDH_INIT];
        push_str(&mut body, public);
        Self {
            kind: KEX_ECDH_INIT,
            body,
        }
    }

    pub fn ecdh_reply(host_blob: &[u8], public: &[u8; 32], signature: &[u8]) -> Self {
        let mut body = vec![KEX_ECDH_REPLY];
        push_str(&mut body, host_blob);
        push_str(&mut body, public);
        push_str(&mut body, signature);
        Self {
            kind: KEX_ECDH_REPLY,
            body,
        }
    }

    pub fn channel_open() -> Self {
        let mut body = vec![CHANNEL_OPEN];
        push_str(&mut body, b"session");
        body.extend_from_slice(&0u32.to_be_bytes());
        body.extend_from_slice(&(1u32 << 20).to_be_bytes());
        body.extend_from_slice(&32768u32.to_be_bytes());
        Self {
            kind: CHANNEL_OPEN,
            body,
        }
    }

    pub fn channel_confirm(theirs: u32) -> Self {
        let mut body = vec![CHANNEL_OPEN_CONFIRMATION];
        body.extend_from_slice(&theirs.to_be_bytes());
        body.extend_from_slice(&0u32.to_be_bytes());
        body.extend_from_slice(&(1u32 << 20).to_be_bytes());
        body.extend_from_slice(&32768u32.to_be_bytes());
        Self {
            kind: CHANNEL_OPEN_CONFIRMATION,
            body,
        }
    }

    pub fn subsystem(theirs: u32) -> Self {
        let mut body = vec![CHANNEL_REQUEST];
        body.extend_from_slice(&theirs.to_be_bytes());
        push_str(&mut body, b"subsystem");
        body.push(1);
        push_str(&mut body, b"sftp");
        Self {
            kind: CHANNEL_REQUEST,
            body,
        }
    }

    pub fn channel_ok(theirs: u32) -> Self {
        let mut body = vec![CHANNEL_SUCCESS];
        body.extend_from_slice(&theirs.to_be_bytes());
        Self {
            kind: CHANNEL_SUCCESS,
            body,
        }
    }

    pub fn channel_data(theirs: u32, data: &[u8]) -> Self {
        let mut body = vec![CHANNEL_DATA];
        body.extend_from_slice(&theirs.to_be_bytes());
        push_str(&mut body, data);
        Self {
            kind: CHANNEL_DATA,
            body,
        }
    }

    pub fn disconnect(why: &str) -> Self {
        let mut body = vec![DISCONNECT];
        body.extend_from_slice(&3u32.to_be_bytes());
        push_str(&mut body, why.as_bytes());
        push_str(&mut body, b"");
        Self {
            kind: DISCONNECT,
            body,
        }
    }

    /// The `n`th SSH string when the payload is a run of strings.
    pub fn text(&self, nth: usize) -> Option<&str> {
        let bytes = self.field(nth)?;
        std::str::from_utf8(bytes).ok()
    }

    pub fn raw_field(&self, nth: usize) -> Option<&[u8]> {
        self.field(nth)
    }

    /// Name lists in a KEXINIT, after the 16-byte cookie.
    pub fn kex_list(&self, index: usize) -> Option<&str> {
        let mut at = 17usize;
        for i in 0..=index {
            let (bytes, next) = read_string(&self.body, at)?;
            if i == index {
                return std::str::from_utf8(bytes).ok();
            }
            at = next;
        }
        None
    }

    /// Sender channel in `SSH_MSG_CHANNEL_OPEN` (string, then uint32).
    pub fn open_sender(&self) -> Option<u32> {
        let (_, at) = read_string(&self.body, 1)?;
        read_u32(&self.body, at).map(|(n, _)| n)
    }

    /// Peer channel in `SSH_MSG_CHANNEL_OPEN_CONFIRMATION` (second uint32).
    pub fn confirm_sender(&self) -> Option<u32> {
        read_u32(&self.body, 5).map(|(n, _)| n)
    }

    /// `"subsystem"` and the subsystem name, when this is that request.
    pub fn subsystem_name(&self) -> Option<&str> {
        let (_, at) = read_u32(&self.body, 1)?;
        let (kind, at) = read_string(&self.body, at)?;
        if kind != b"subsystem" {
            return None;
        }
        let at = at + 1;
        let (name, _) = read_string(&self.body, at)?;
        std::str::from_utf8(name).ok()
    }

    /// Recipient channel and the data string of `SSH_MSG_CHANNEL_DATA`.
    pub fn data_bytes(&self) -> Option<(u32, &[u8])> {
        let (recipient, at) = read_u32(&self.body, 1)?;
        let (data, _) = read_string(&self.body, at)?;
        Some((recipient, data))
    }

    fn field(&self, nth: usize) -> Option<&[u8]> {
        let mut at = 1usize;
        let mut seen = 0usize;
        while at < self.body.len() {
            if self.body.len() < at + 4 {
                return None;
            }
            let n = u32::from_be_bytes(self.body[at..at + 4].try_into().ok()?) as usize;
            let start = at + 4;
            let end = start + n;
            if self.body.len() < end {
                return None;
            }
            if seen == nth {
                return Some(&self.body[start..end]);
            }
            seen += 1;
            at = end;
        }
        None
    }

    /// Password bytes from a password request. Empty when the method
    /// is not password.
    pub fn take_password(&mut self) -> Vec<u8> {
        let Some((start, end)) = self.password_span() else {
            return Vec::new();
        };
        self.body[start..end].to_vec()
    }

    pub fn wipe_password(&mut self) {
        if let Some((start, end)) = self.password_span() {
            self.body[start..end].zeroize();
        }
    }

    fn password_span(&self) -> Option<(usize, usize)> {
        if self.kind != USERAUTH_REQUEST {
            return None;
        }
        // user, service, method, then a boolean, then the password string.
        let mut at = 1usize;
        for _ in 0..3 {
            if self.body.len() < at + 4 {
                return None;
            }
            let n = u32::from_be_bytes(self.body[at..at + 4].try_into().ok()?) as usize;
            at += 4 + n;
        }
        if self.body.len() < at + 1 {
            return None;
        }
        at += 1;
        if self.body.len() < at + 4 {
            return None;
        }
        let n = u32::from_be_bytes(self.body[at..at + 4].try_into().ok()?) as usize;
        let start = at + 4;
        let end = start + n;
        if self.body.len() < end {
            return None;
        }
        Some((start, end))
    }
}

impl Drop for Msg {
    fn drop(&mut self) {
        self.wipe_password();
        self.body.zeroize();
    }
}

pub fn push_str(out: &mut Vec<u8>, bytes: &[u8]) {
    out.extend_from_slice(&(bytes.len() as u32).to_be_bytes());
    out.extend_from_slice(bytes);
}

fn read_u32(body: &[u8], at: usize) -> Option<(u32, usize)> {
    let bytes: [u8; 4] = body.get(at..at + 4)?.try_into().ok()?;
    Some((u32::from_be_bytes(bytes), at + 4))
}

fn read_string(body: &[u8], at: usize) -> Option<(&[u8], usize)> {
    let (n, start) = read_u32(body, at)?;
    let end = start + n as usize;
    Some((body.get(start..end)?, end))
}

pub fn name_has(list: &str, name: &str) -> bool {
    list.split(',').any(|item| item == name)
}

/// First name in `ours` that `theirs` also offers, skipping the strict marker.
pub fn choose(ours: &str, theirs: &str) -> Option<String> {
    for name in ours.split(',') {
        if name.starts_with("kex-strict-") {
            continue;
        }
        if name_has(theirs, name) {
            return Some(name.to_string());
        }
    }
    None
}
