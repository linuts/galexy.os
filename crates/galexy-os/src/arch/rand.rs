//! Kernel CSPRNG for salts and (later) AEAD nonces.
//!
//! Prefers RDSEED, then RDRAND. When both are missing or stuck, mixes the
//! LAPIC tick counter into a small xorshift state so boot can still format
//! a disk (documented fallback — not a substitute for hardware RNG on
//! real hardware that advertises RDRAND).

use core::sync::atomic::{AtomicU64, Ordering};

use x86_64::instructions::random::RdRand;

static FALLBACK: AtomicU64 = AtomicU64::new(0xC0FFEE_u64.wrapping_mul(0x9E37_79B9_7F4A_7C15));

/// Fills `buf` with random bytes.
pub fn fill_bytes(buf: &mut [u8]) {
    let mut i = 0;
    while i < buf.len() {
        if let Some(word) = next_u64() {
            let bytes = word.to_ne_bytes();
            let n = (buf.len() - i).min(8);
            buf[i..i + n].copy_from_slice(&bytes[..n]);
            i += n;
        } else {
            // Should be unreachable: next_u64 always returns Some after
            // the fallback path, but keep the loop total.
            buf[i] = 0xA5;
            i += 1;
        }
    }
}

fn next_u64() -> Option<u64> {
    // RDSEED / RDRAND via the `x86_64` helper (RDRAND only in this crate
    // version — treat it as the hardware source).
    if let Some(rdrand) = RdRand::new() {
        for _ in 0..16 {
            if let Some(v) = rdrand.get_u64() {
                mix_fallback(v);
                return Some(v);
            }
        }
    }
    Some(fallback_u64())
}

fn mix_fallback(v: u64) {
    let ticks = crate::arch::timer_ticks();
    FALLBACK.fetch_xor(v.wrapping_mul(0xD1B5_4A32_D192_ED03) ^ ticks, Ordering::Relaxed);
}

fn fallback_u64() -> u64 {
    let ticks = crate::arch::timer_ticks();
    let mut s = FALLBACK.load(Ordering::Relaxed) ^ ticks.wrapping_shl(1) ^ ticks;
    // xorshift64*
    s ^= s >> 12;
    s ^= s << 25;
    s ^= s >> 27;
    let out = s.wrapping_mul(0x2545_F491_4F6C_DD1D);
    FALLBACK.store(s ^ out, Ordering::Relaxed);
    out
}
