//! ChaCha20 + HMAC-SHA256 Encrypt-then-MAC (alloc-free).
//!
//! Same nonce/key sizes as IETF ChaCha20-Poly1305 so on-disk layout can
//! switch to RFC 8439 Poly1305 later without a header resize. Tag is the
//! first 16 bytes of HMAC-SHA256 over AAD and ciphertext.

use crate::sha256;

/// ChaCha20 key length.
pub const KEY_LEN: usize = 32;
/// IETF ChaCha20 nonce length.
pub const NONCE_LEN: usize = 12;
/// Authentication tag length (HMAC-SHA256 truncated).
pub const TAG_LEN: usize = 16;

/// Encrypts `buf` in place and writes the tag to `tag`.
pub fn seal(
    key: &[u8; KEY_LEN],
    nonce: &[u8; NONCE_LEN],
    aad: &[u8],
    buf: &mut [u8],
    tag: &mut [u8; TAG_LEN],
) {
    chacha20_xor(key, nonce, 1, buf);
    let full = mac(key, nonce, aad, buf);
    tag.copy_from_slice(&full[..TAG_LEN]);
}

/// Decrypts `buf` in place. Returns `false` if the tag does not verify.
pub fn open(
    key: &[u8; KEY_LEN],
    nonce: &[u8; NONCE_LEN],
    aad: &[u8],
    buf: &mut [u8],
    tag: &[u8; TAG_LEN],
) -> bool {
    let full = mac(key, nonce, aad, buf);
    let mut diff = 0u8;
    for i in 0..TAG_LEN {
        diff |= full[i] ^ tag[i];
    }
    if diff != 0 {
        return false;
    }
    chacha20_xor(key, nonce, 1, buf);
    true
}

fn mac(key: &[u8; KEY_LEN], nonce: &[u8; NONCE_LEN], aad: &[u8], ct: &[u8]) -> [u8; 32] {
    // Domain-separated Encrypt-then-MAC: MAC key from ChaCha20 block 0
    // (same construction Poly1305 uses for its one-time key).
    let mut otk = [0u8; 64];
    chacha20_block(key, nonce, 0, &mut otk);
    let mut mac_key = [0u8; 32];
    mac_key.copy_from_slice(&otk[..32]);
    wipe(&mut otk);

    // HMAC-SHA256(mac_key, aad || le64(aad_len) || ct || le64(ct_len) || nonce)
    let mut ipad = [0x36u8; 64];
    let mut opad = [0x5cu8; 64];
    for i in 0..32 {
        ipad[i] ^= mac_key[i];
        opad[i] ^= mac_key[i];
    }
    wipe(&mut mac_key);
    let mut inner = sha256::Sha256::new();
    inner.update(&ipad);
    inner.update(aad);
    inner.update(&(aad.len() as u64).to_le_bytes());
    inner.update(ct);
    inner.update(&(ct.len() as u64).to_le_bytes());
    inner.update(nonce);
    let inner_hash = inner.finalize();
    let mut outer = sha256::Sha256::new();
    outer.update(&opad);
    outer.update(&inner_hash);
    outer.finalize()
}

fn wipe(buf: &mut [u8]) {
    for b in buf.iter_mut() {
        // SAFETY: defeat DSE on key material.
        unsafe {
            core::ptr::write_volatile(b, 0);
        }
    }
}

fn chacha20_xor(key: &[u8; 32], nonce: &[u8; 12], mut counter: u32, buf: &mut [u8]) {
    let mut off = 0;
    while off < buf.len() {
        let mut block = [0u8; 64];
        chacha20_block(key, nonce, counter, &mut block);
        counter = counter.wrapping_add(1);
        let n = (buf.len() - off).min(64);
        for i in 0..n {
            buf[off + i] ^= block[i];
        }
        wipe(&mut block);
        off += n;
    }
}

fn chacha20_block(key: &[u8; 32], nonce: &[u8; 12], counter: u32, out: &mut [u8; 64]) {
    let mut state = [
        0x6170_7865,
        0x3320_646e,
        0x7962_2d32,
        0x6b20_6574,
        u32::from_le_bytes(key[0..4].try_into().unwrap()),
        u32::from_le_bytes(key[4..8].try_into().unwrap()),
        u32::from_le_bytes(key[8..12].try_into().unwrap()),
        u32::from_le_bytes(key[12..16].try_into().unwrap()),
        u32::from_le_bytes(key[16..20].try_into().unwrap()),
        u32::from_le_bytes(key[20..24].try_into().unwrap()),
        u32::from_le_bytes(key[24..28].try_into().unwrap()),
        u32::from_le_bytes(key[28..32].try_into().unwrap()),
        counter,
        u32::from_le_bytes(nonce[0..4].try_into().unwrap()),
        u32::from_le_bytes(nonce[4..8].try_into().unwrap()),
        u32::from_le_bytes(nonce[8..12].try_into().unwrap()),
    ];
    let mut working = state;
    for _ in 0..10 {
        quarter(&mut working, 0, 4, 8, 12);
        quarter(&mut working, 1, 5, 9, 13);
        quarter(&mut working, 2, 6, 10, 14);
        quarter(&mut working, 3, 7, 11, 15);
        quarter(&mut working, 0, 5, 10, 15);
        quarter(&mut working, 1, 6, 11, 12);
        quarter(&mut working, 2, 7, 8, 13);
        quarter(&mut working, 3, 4, 9, 14);
    }
    for i in 0..16 {
        state[i] = state[i].wrapping_add(working[i]);
        out[i * 4..(i + 1) * 4].copy_from_slice(&state[i].to_le_bytes());
    }
}

fn quarter(s: &mut [u32; 16], a: usize, b: usize, c: usize, d: usize) {
    s[a] = s[a].wrapping_add(s[b]);
    s[d] ^= s[a];
    s[d] = s[d].rotate_left(16);
    s[c] = s[c].wrapping_add(s[d]);
    s[b] ^= s[c];
    s[b] = s[b].rotate_left(12);
    s[a] = s[a].wrapping_add(s[b]);
    s[d] ^= s[a];
    s[d] = s[d].rotate_left(8);
    s[c] = s[c].wrapping_add(s[d]);
    s[b] ^= s[c];
    s[b] = s[b].rotate_left(7);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        let key = [7u8; 32];
        let nonce = [1u8; 12];
        let mut buf = *b"persist-ok sealed!!";
        let mut tag = [0u8; TAG_LEN];
        seal(&key, &nonce, b"GALF", &mut buf, &mut tag);
        assert_ne!(&buf[..], b"persist-ok sealed!!");
        assert!(open(&key, &nonce, b"GALF", &mut buf, &tag));
        assert_eq!(&buf[..], b"persist-ok sealed!!");
    }

    #[test]
    fn wrong_tag_fails() {
        let key = [7u8; 32];
        let nonce = [1u8; 12];
        let mut buf = *b"hello sealed galfs!!";
        let mut tag = [0u8; TAG_LEN];
        seal(&key, &nonce, b"aad", &mut buf, &mut tag);
        tag[0] ^= 1;
        assert!(!open(&key, &nonce, b"aad", &mut buf, &tag));
    }

    #[test]
    fn wrong_aad_fails() {
        let key = [9u8; 32];
        let nonce = [2u8; 12];
        let mut buf = *b"volume payload!!!!!";
        let mut tag = [0u8; TAG_LEN];
        seal(&key, &nonce, b"v6", &mut buf, &mut tag);
        assert!(!open(&key, &nonce, b"v5", &mut buf, &tag));
    }
}
