//! Interim password hashing for actor accounts.
//!
//! Not a real KDF. Iterated CRC-32 mixing of salt and password bytes.
//! Replace with Argon2/scrypt when a crypto crate is acceptable; the
//! on-disk layout (8-byte salt + 16-byte hash) stays the same.

use crate::crc32;

/// Salt length stored on each actor.
pub const SALT_LEN: usize = 8;
/// Hash length stored on each actor.
pub const HASH_LEN: usize = 16;
/// Mixing rounds (cheap on boot; enough to avoid a plain CRC of the password).
const ROUNDS: u32 = 4096;

/// Fills `out` with the interim hash of `password` and `salt`.
pub fn hash_password(password: &[u8], salt: &[u8; SALT_LEN], out: &mut [u8; HASH_LEN]) {
    let mut state = [0u8; 32];
    state[..SALT_LEN].copy_from_slice(salt);
    let mut acc = crc32(salt);
    for round in 0..ROUNDS {
        acc = crc32_u32(acc, round);
        acc = crc32_bytes(acc, password);
        acc = crc32_bytes(acc, salt);
        let i = (round as usize) % state.len();
        state[i] ^= (acc & 0xff) as u8;
        state[(i + 1) % state.len()] ^= ((acc >> 8) & 0xff) as u8;
        state[(i + 2) % state.len()] ^= ((acc >> 16) & 0xff) as u8;
        state[(i + 3) % state.len()] ^= ((acc >> 24) & 0xff) as u8;
    }
    out.copy_from_slice(&state[..HASH_LEN]);
}

/// Constant-time compare of two hashes.
pub fn hash_eq(a: &[u8; HASH_LEN], b: &[u8; HASH_LEN]) -> bool {
    let mut diff = 0u8;
    for i in 0..HASH_LEN {
        diff |= a[i] ^ b[i];
    }
    diff == 0
}

/// Derives a deterministic salt from a seed string (boot format / tests).
pub fn salt_from_seed(seed: &[u8], out: &mut [u8; SALT_LEN]) {
    let mut acc = crc32(seed);
    for byte in out.iter_mut() {
        acc = crc32_u32(acc, acc);
        *byte = (acc & 0xff) as u8;
    }
}

fn crc32_u32(acc: u32, n: u32) -> u32 {
    let mut buf = [0u8; 8];
    buf[..4].copy_from_slice(&acc.to_le_bytes());
    buf[4..].copy_from_slice(&n.to_le_bytes());
    crc32(&buf)
}

fn crc32_bytes(acc: u32, bytes: &[u8]) -> u32 {
    let mut buf = [0u8; 4];
    buf.copy_from_slice(&acc.to_le_bytes());
    let mut mix = [0u8; 4];
    // Fold password through the accumulator in chunks.
    let mut a = crc32(&buf);
    for chunk in bytes.chunks(4) {
        mix.fill(0);
        mix[..chunk.len()].copy_from_slice(chunk);
        a = {
            let mut both = [0u8; 8];
            both[..4].copy_from_slice(&a.to_le_bytes());
            both[4..].copy_from_slice(&mix);
            crc32(&both)
        };
    }
    a
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_inputs_same_hash() {
        let mut salt = [0u8; SALT_LEN];
        salt_from_seed(b"admin", &mut salt);
        let mut a = [0u8; HASH_LEN];
        let mut b = [0u8; HASH_LEN];
        hash_password(b"admin", &salt, &mut a);
        hash_password(b"admin", &salt, &mut b);
        assert!(hash_eq(&a, &b));
    }

    #[test]
    fn different_password_differs() {
        let mut salt = [0u8; SALT_LEN];
        salt_from_seed(b"admin", &mut salt);
        let mut a = [0u8; HASH_LEN];
        let mut b = [0u8; HASH_LEN];
        hash_password(b"admin", &salt, &mut a);
        hash_password(b"nope", &salt, &mut b);
        assert!(!hash_eq(&a, &b));
    }
}
