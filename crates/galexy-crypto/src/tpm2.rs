//! TPM 2.0 command bytes for the kernel's `GetRandom` path.
//!
//! The wire format is big-endian. The kernel sends these through the
//! CRB or TIS register interface; this module does not touch hardware.

/// `TPM2_GetRandom` asking for `bytes` octets. `bytes` is at most 32 here
/// because the kernel only seeds a 32-byte key.
pub fn get_random_command(bytes: u16) -> [u8; 12] {
    let mut cmd = [0u8; 12];
    cmd[0..2].copy_from_slice(&0x8001u16.to_be_bytes());
    cmd[2..6].copy_from_slice(&12u32.to_be_bytes());
    cmd[6..10].copy_from_slice(&0x017Bu32.to_be_bytes());
    cmd[10..12].copy_from_slice(&bytes.to_be_bytes());
    cmd
}

/// `TPM2_Startup(CLEAR)`. Sent once when `GetRandom` says the TPM has
/// not been started.
pub fn startup_clear_command() -> [u8; 12] {
    let mut cmd = [0u8; 12];
    cmd[0..2].copy_from_slice(&0x8001u16.to_be_bytes());
    cmd[2..6].copy_from_slice(&12u32.to_be_bytes());
    cmd[6..10].copy_from_slice(&0x0144u32.to_be_bytes());
    cmd
}

/// What a `GetRandom` response means.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RandomReply {
    /// `n` random bytes were written at the front of the caller's buffer.
    Bytes(usize),
    /// `TPM_RC_INITIALIZE`: the TPM wants `TPM2_Startup` first.
    NeedStartup,
    /// Any other failure, including a short or malformed buffer.
    Failed,
}

/// Parses a `GetRandom` response. On [`RandomReply::Bytes`] the octets
/// are copied into `out`.
pub fn parse_get_random(resp: &[u8], out: &mut [u8]) -> RandomReply {
    if resp.len() < 10 {
        return RandomReply::Failed;
    }
    let Ok(tag) = resp[0..2].try_into().map(u16::from_be_bytes) else {
        return RandomReply::Failed;
    };
    let Ok(size) = resp[2..6].try_into().map(u32::from_be_bytes) else {
        return RandomReply::Failed;
    };
    let Ok(rc) = resp[6..10].try_into().map(u32::from_be_bytes) else {
        return RandomReply::Failed;
    };
    let size = size as usize;
    if tag != 0x8001 || size < 10 || size > resp.len() {
        return RandomReply::Failed;
    }
    if rc == 0x100 {
        return RandomReply::NeedStartup;
    }
    if rc != 0 || size < 12 {
        return RandomReply::Failed;
    }
    let Ok(n) = resp[10..12].try_into().map(u16::from_be_bytes) else {
        return RandomReply::Failed;
    };
    let n = n as usize;
    if n == 0 || 12 + n > size || n > out.len() {
        return RandomReply::Failed;
    }
    out[..n].copy_from_slice(&resp[12..12 + n]);
    RandomReply::Bytes(n)
}

/// `true` when a `Startup` response is a success header.
pub fn startup_ok(resp: &[u8]) -> bool {
    if resp.len() < 10 {
        return false;
    }
    let Ok(tag) = resp[0..2].try_into().map(u16::from_be_bytes) else {
        return false;
    };
    let Ok(rc) = resp[6..10].try_into().map(u32::from_be_bytes) else {
        return false;
    };
    tag == 0x8001 && rc == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn get_random_command_is_big_endian() {
        assert_eq!(
            get_random_command(32),
            [0x80, 0x01, 0x00, 0x00, 0x00, 0x0C, 0x00, 0x00, 0x01, 0x7B, 0x00, 0x20,]
        );
    }

    #[test]
    fn parses_a_random_body() {
        let resp = [
            0x80, 0x01, 0x00, 0x00, 0x00, 0x0E, 0x00, 0x00, 0x00, 0x00, 0x00, 0x02, 0xAB, 0xCD,
        ];
        let mut out = [0u8; 8];
        assert_eq!(parse_get_random(&resp, &mut out), RandomReply::Bytes(2));
        assert_eq!(&out[..2], &[0xAB, 0xCD]);
    }

    #[test]
    fn initialize_is_need_startup() {
        let resp = [0x80, 0x01, 0x00, 0x00, 0x00, 0x0A, 0x00, 0x00, 0x01, 0x00];
        assert_eq!(
            parse_get_random(&resp, &mut [0u8; 8]),
            RandomReply::NeedStartup
        );
    }
}
