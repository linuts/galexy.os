//! Ring-3 randomness for `std` (`HashMap` keys and the `rdrand` getrandom
//! backend).
//!
//! There is no random syscall. `Syscall::Lane` stays reserved at 35, and
//! a user task cannot touch the TPM. `RDRAND` is the hardware source.
//! When it is absent or stuck, the monotonic clock is mixed in so the
//! call returns. Kernel salts and volume keys use `arch::rand`, which
//! prefers the TPM, then the firmware sample, then this same instruction,
//! then a software stream.

use crate::sync::atomic::Ordering::Relaxed;
use crate::sync::atomic::{Atomic, AtomicU8, AtomicU64};
use crate::sys::pal::syscall;

/// CPUID.1:ECX bit 30. `0` = unknown, `1` = absent, `2` = present.
static HAS_RDRAND: Atomic<u8> = AtomicU8::new(0);

fn has_rdrand() -> bool {
    match HAS_RDRAND.load(Relaxed) {
        0 => {
            // CPUID is unprivileged on x86-64 (`__cpuid` is safe on this
            // toolchain).
            let leaf = core::arch::x86_64::__cpuid(1);
            let present = leaf.ecx & (1 << 30) != 0;
            HAS_RDRAND.store(if present { 2 } else { 1 }, Relaxed);
            present
        }
        state => state == 2,
    }
}

/// One 64-bit sample into `out`; `false` when the hardware reported no
/// entropy. FFI-shaped (`#[inline(never)] extern "C"`) like every other
/// `asm!` stub in the PAL.
#[inline(never)]
extern "C" fn rdrand64(out: &mut u64) -> bool {
    let value: u64;
    let ok: u8;
    // SAFETY: `rdrand` has no memory effects; CF reports success. The
    // caller checked CPUID.
    unsafe {
        core::arch::asm!(
            "rdrand {0}",
            "setc {1}",
            out(reg) value,
            out(reg_byte) ok,
            options(nomem, nostack),
        );
    }
    *out = value;
    ok != 0
}

fn next_word() -> u64 {
    if has_rdrand() {
        let mut value = 0u64;
        for _ in 0..16 {
            if rdrand64(&mut value) {
                return value;
            }
        }
    }
    fallback_word()
}

static FALLBACK: Atomic<u64> = AtomicU64::new(0);

/// Degraded path (no RDRAND, or it stayed stuck): xorshift64* seeded from
/// the monotonic clock and a stack address. Enough for `HashMap` keys, not
/// for secrets — the same caveat the kernel's own fallback carries.
fn fallback_word() -> u64 {
    let probe = 0u8;
    let salt = syscall::clock_ms() ^ (core::ptr::from_ref(&probe).addr() as u64).rotate_left(17);
    let mut s = FALLBACK.load(Relaxed) ^ salt ^ 0x9E37_79B9_7F4A_7C15;
    s ^= s >> 12;
    s ^= s << 25;
    s ^= s >> 27;
    FALLBACK.store(s, Relaxed);
    s.wrapping_mul(0x2545_F491_4F6C_DD1D)
}

pub fn fill_bytes(bytes: &mut [u8]) {
    let mut chunks = bytes.chunks_exact_mut(8);
    for chunk in &mut chunks {
        chunk.copy_from_slice(&next_word().to_ne_bytes());
    }
    let rest = chunks.into_remainder();
    if !rest.is_empty() {
        let word = next_word().to_ne_bytes();
        rest.copy_from_slice(&word[..rest.len()]);
    }
}

pub fn hashmap_random_keys() -> (u64, u64) {
    (next_word(), next_word())
}
