//! Password KDF and secret helpers for galexy.os.
//!
//! `no_std` + `alloc` (Argon2 needs a short-lived scratch buffer). Platform
//! CSPRNG lives in the kernel (`arch::rand`); this crate is pure crypto.

#![no_std]
#![deny(clippy::all)]
#![deny(missing_docs)]

extern crate alloc;

use argon2::{Algorithm, Argon2, Params, Version};

/// Salt length stored on each actor (CSPRNG-filled at set-password).
pub const SALT_LEN: usize = 8;
/// Derived-key length stored on each actor.
pub const HASH_LEN: usize = 16;

/// Argon2id memory cost in KiB (64 KiB — fit for IF=0 login on QEMU TCG).
pub const ARGON2_M_KIB: u32 = 64;
/// Argon2id time cost (passes).
pub const ARGON2_T_COST: u32 = 3;
/// Argon2id parallelism (lanes).
pub const ARGON2_P_COST: u32 = 1;

fn argon2() -> Argon2<'static> {
    let params = Params::new(ARGON2_M_KIB, ARGON2_T_COST, ARGON2_P_COST, Some(HASH_LEN))
        .expect("argon2 params");
    Argon2::new(Algorithm::Argon2id, Version::V0x13, params)
}

/// Fills `out` with Argon2id(password, salt).
pub fn hash_password(password: &[u8], salt: &[u8; SALT_LEN], out: &mut [u8; HASH_LEN]) {
    argon2()
        .hash_password_into(password, salt, out)
        .expect("argon2id hash");
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
