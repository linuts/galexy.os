//! Password KDF and secret helpers for galexy.os.
//!
//! `no_std` and **alloc-free** (fits IF=0 syscalls and thin kstacks).
//! Platform CSPRNG lives in the kernel (`arch::rand`).
//!
//! KDF: PBKDF2-HMAC-SHA256 with a freestanding SHA-256 (no `sha2` crate —
//! its asm path does not build for `x86_64-unknown-none`).

#![no_std]
#![deny(clippy::all)]
#![deny(missing_docs)]

mod sha256;

/// Salt length stored on each actor (CSPRNG-filled at set-password).
pub const SALT_LEN: usize = 8;
/// Derived-key length stored on each actor.
pub const HASH_LEN: usize = 16;

/// PBKDF2 iteration count (HMAC-SHA256).
pub const PBKDF2_ITERS: u32 = 100_000;

/// Fills `out` with PBKDF2-HMAC-SHA256(password, salt, [`PBKDF2_ITERS`]).
pub fn hash_password(password: &[u8], salt: &[u8; SALT_LEN], out: &mut [u8; HASH_LEN]) {
    pbkdf2_hmac_sha256(password, salt, PBKDF2_ITERS, out);
}

/// Constant-time compare of two digests.
pub fn hash_eq(a: &[u8; HASH_LEN], b: &[u8; HASH_LEN]) -> bool {
    let mut diff = 0u8;
    for i in 0..HASH_LEN {
        diff |= a[i] ^ b[i];
    }
    diff == 0
}

/// Overwrites `buf` with zeros (volatile-ish loop so the wipe is not elided).
pub fn wipe_bytes(buf: &mut [u8]) {
    for b in buf.iter_mut() {
        // SAFETY: single-byte write through a raw pointer; used to defeat
        // dead-store elimination on secret buffers.
        unsafe {
            core::ptr::write_volatile(b, 0);
        }
    }
}

fn hmac_sha256(key: &[u8], msg: &[u8]) -> [u8; 32] {
    let mut key_block = [0u8; 64];
    if key.len() > 64 {
        key_block[..32].copy_from_slice(&sha256::hash(key));
    } else {
        key_block[..key.len()].copy_from_slice(key);
    }
    let mut ipad = [0x36u8; 64];
    let mut opad = [0x5cu8; 64];
    for i in 0..64 {
        ipad[i] ^= key_block[i];
        opad[i] ^= key_block[i];
    }
    let mut inner = sha256::Sha256::new();
    inner.update(&ipad);
    inner.update(msg);
    let inner_hash = inner.finalize();
    let mut outer = sha256::Sha256::new();
    outer.update(&opad);
    outer.update(&inner_hash);
    outer.finalize()
}

fn pbkdf2_hmac_sha256(password: &[u8], salt: &[u8], iters: u32, out: &mut [u8]) {
    let mut block_index = 1u32;
    let mut offset = 0usize;
    while offset < out.len() {
        let mut block = {
            let mut msg = [0u8; 256];
            let n = salt.len().min(252);
            msg[..n].copy_from_slice(&salt[..n]);
            msg[n..n + 4].copy_from_slice(&block_index.to_be_bytes());
            hmac_sha256(password, &msg[..n + 4])
        };
        let mut u = block;
        for _ in 1..iters {
            u = hmac_sha256(password, &u);
            for i in 0..32 {
                block[i] ^= u[i];
            }
        }
        let n = (out.len() - offset).min(32);
        out[offset..offset + n].copy_from_slice(&block[..n]);
        offset += n;
        block_index = block_index.wrapping_add(1);
    }
}

/// Deterministic salt for host unit tests only.
#[cfg(test)]
pub fn salt_from_seed(seed: &[u8], out: &mut [u8; SALT_LEN]) {
    let mut state = 0xA5A5_u32;
    for (i, b) in seed.iter().enumerate() {
        state = state.wrapping_mul(16777619) ^ (*b as u32).wrapping_add(i as u32);
    }
    for byte in out.iter_mut() {
        state = state.wrapping_mul(1664525).wrapping_add(1013904223);
        *byte = (state >> 16) as u8;
    }
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

    #[test]
    fn truncated_password_differs() {
        let mut salt = [0u8; SALT_LEN];
        salt_from_seed(b"x", &mut salt);
        let mut a = [0u8; HASH_LEN];
        let mut b = [0u8; HASH_LEN];
        hash_password(b"password", &salt, &mut a);
        hash_password(b"passwor", &salt, &mut b);
        assert!(!hash_eq(&a, &b));
    }

    #[test]
    fn same_password_different_salts_differ() {
        let mut s1 = [0u8; SALT_LEN];
        let mut s2 = [0u8; SALT_LEN];
        salt_from_seed(b"one", &mut s1);
        salt_from_seed(b"two", &mut s2);
        let mut a = [0u8; HASH_LEN];
        let mut b = [0u8; HASH_LEN];
        hash_password(b"admin", &s1, &mut a);
        hash_password(b"admin", &s2, &mut b);
        assert!(!hash_eq(&a, &b));
    }

    #[test]
    fn wipe_clears() {
        let mut buf = *b"secret!!";
        wipe_bytes(&mut buf);
        assert_eq!(buf, [0u8; 8]);
    }
}
