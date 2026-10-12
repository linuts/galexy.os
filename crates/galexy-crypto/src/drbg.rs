//! ChaCha20 keystream used as the kernel's software generator.
//!
//! The seed is chosen by `arch::rand`. This type only expands it. A weak
//! seed still produces a weak stream; the cipher does not add entropy.

use crate::aead::chacha20_block;

/// Expanding generator. One seed, then a counter. Not reseeded.
pub struct Drbg {
    key: [u8; 32],
    nonce: [u8; 12],
    counter: u32,
}

impl Drbg {
    /// `seed` is the entire entropy input. The nonce is domain-separating
    /// and fixed so two equal seeds produce the same stream.
    pub fn from_seed(seed: &[u8; 32]) -> Self {
        Self {
            key: *seed,
            nonce: *b"GXRN\0\0\0\0\0\0\0\x01",
            counter: 1,
        }
    }

    /// Writes the next keystream bytes into `buf`.
    pub fn fill(&mut self, buf: &mut [u8]) {
        let mut off = 0;
        while off < buf.len() {
            let mut block = [0u8; 64];
            chacha20_block(&self.key, &self.nonce, self.counter, &mut block);
            self.counter = self.counter.wrapping_add(1);
            if self.counter == 0 {
                let n = u64::from_le_bytes(self.nonce[4..12].try_into().unwrap()).wrapping_add(1);
                self.nonce[4..12].copy_from_slice(&n.to_le_bytes());
                self.counter = 1;
            }
            let n = (buf.len() - off).min(64);
            buf[off..off + n].copy_from_slice(&block[..n]);
            for byte in &mut block {
                // SAFETY: the block is keystream; a dead store would leave it.
                unsafe { core::ptr::write_volatile(byte, 0) };
            }
            off += n;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_seed_matches() {
        let seed = [9u8; 32];
        let mut a = Drbg::from_seed(&seed);
        let mut b = Drbg::from_seed(&seed);
        let mut left = [0u8; 80];
        let mut right = [0u8; 80];
        a.fill(&mut left);
        b.fill(&mut right);
        assert_eq!(left, right);
        assert!(left.iter().any(|b| *b != 0));
    }

    #[test]
    fn different_seeds_differ() {
        let mut a = Drbg::from_seed(&[1u8; 32]);
        let mut b = Drbg::from_seed(&[2u8; 32]);
        let mut left = [0u8; 32];
        let mut right = [0u8; 32];
        a.fill(&mut left);
        b.fill(&mut right);
        assert_ne!(left, right);
    }
}
