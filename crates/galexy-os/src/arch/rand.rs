//! Kernel generator for salts and volume-key material.
//!
//! Preference, first sample that yields 32 non-uniform bytes:
//!
//! 1. TPM 2.0 `GetRandom` (the ACPI `TPM2` table).
//! 2. Firmware RNG. The UEFI bootloader calls `EFI_RNG_PROTOCOL` before
//!    `ExitBootServices` and leaves the bytes at physical `0x80000`.
//!    Legacy BIOS has no RNG interrupt that survives into long mode, so
//!    that boot does not have this sample.
//! 3. `RDSEED`, then `RDRAND`.
//! 4. A ChaCha20 stream seeded from the time-stamp counter. That seed is
//!    weak. It exists so a machine with none of the above still formats
//!    a disk.
//!
//! Later draws come from the stream. They do not round-trip the TPM.

use core::sync::atomic::{AtomicU8, Ordering};

use galexy_crypto::Drbg;
use spin::Once;
use x86_64::instructions::random::RdRand;

use crate::serial_println;

use super::tpm;

/// Physical page where the firmware stash lives. Same address the UEFI
/// bootloader allocates.
const FIRMWARE_PHYS: u64 = 0x8_0000;
const FIRMWARE_MAGIC: &[u8; 8] = b"GALXBIOS";

const SRC_UNSET: u8 = 0;
const SRC_TPM: u8 = 1;
const SRC_BIOS: u8 = 2;
const SRC_RDSEED: u8 = 3;
const SRC_RDRAND: u8 = 4;
const SRC_SOFT: u8 = 5;

static SOURCE: AtomicU8 = AtomicU8::new(SRC_UNSET);
static DRBG: spin::Mutex<Option<Drbg>> = spin::Mutex::new(None);
static FIRMWARE: Once<Option<[u8; 32]>> = Once::new();

/// Copies a firmware sample out of the stash page before the frame
/// allocator can reuse it. Safe to call more than once.
pub(super) fn capture_firmware(phys_offset: u64) {
    FIRMWARE.call_once(|| read_firmware(phys_offset));
}

/// Selects the seed and logs `[rand] source=...`. Call once from
/// `arch::init`, after ACPI.
pub fn init(phys_offset: u64) {
    if SOURCE.load(Ordering::Relaxed) != SRC_UNSET {
        return;
    }
    capture_firmware(phys_offset);
    let (seed, source) = select_seed(phys_offset);
    *DRBG.lock() = Some(Drbg::from_seed(&seed));
    SOURCE.store(source, Ordering::Relaxed);
    serial_println!("[rand] source={}", source_name(source));
}

/// The source chosen by [`init`]: `tpm`, `bios`, `rdseed`, `rdrand`,
/// `software`, or `unset` if [`init`] has not run.
pub fn source() -> &'static str {
    source_name(SOURCE.load(Ordering::Relaxed))
}

/// Fills `buf` from the stream selected by [`init`].
pub fn fill_bytes(buf: &mut [u8]) {
    if SOURCE.load(Ordering::Relaxed) == SRC_UNSET {
        let seed = software_seed();
        *DRBG.lock() = Some(Drbg::from_seed(&seed));
        SOURCE.store(SRC_SOFT, Ordering::Relaxed);
    }
    let mut guard = DRBG.lock();
    let drbg = guard.get_or_insert_with(|| Drbg::from_seed(&software_seed()));
    drbg.fill(buf);
}

fn select_seed(phys_offset: u64) -> ([u8; 32], u8) {
    let mut seed = [0u8; 32];
    if tpm::get_random(phys_offset, &mut seed) {
        return (seed, SRC_TPM);
    }
    if let Some(bytes) = FIRMWARE.get().copied().flatten() {
        return (bytes, SRC_BIOS);
    }
    if cpu_words(true, &mut seed) {
        return (seed, SRC_RDSEED);
    }
    if cpu_words(false, &mut seed) {
        return (seed, SRC_RDRAND);
    }
    (software_seed(), SRC_SOFT)
}

fn source_name(source: u8) -> &'static str {
    match source {
        SRC_TPM => "tpm",
        SRC_BIOS => "bios",
        SRC_RDSEED => "rdseed",
        SRC_RDRAND => "rdrand",
        SRC_SOFT => "software",
        _ => "unset",
    }
}

fn read_firmware(phys_offset: u64) -> Option<[u8; 32]> {
    let ptr = (phys_offset + FIRMWARE_PHYS) as *mut u8;
    // SAFETY: the phys map covers the low page. A missing stash is a
    // magic mismatch, not a fault: the bootloader mapped all of physical
    // memory.
    let magic = unsafe { core::slice::from_raw_parts(ptr, 8) };
    if magic != FIRMWARE_MAGIC {
        return None;
    }
    let kind = unsafe { ptr.add(8).read_volatile() };
    if kind != 1 {
        return None;
    }
    let mut bytes = [0u8; 32];
    for (i, byte) in bytes.iter_mut().enumerate() {
        // SAFETY: the 32 data bytes sit immediately after the kind byte.
        *byte = unsafe { ptr.add(9 + i).read_volatile() };
    }
    for i in 0..41 {
        // SAFETY: wiping the stash so a later reader cannot replay it.
        unsafe { ptr.add(i).write_volatile(0) };
    }
    (!uniform(&bytes)).then_some(bytes)
}

fn cpu_words(rdseed: bool, out: &mut [u8; 32]) -> bool {
    if rdseed {
        if !cpuid_bit(7, 0, true, 18) {
            return false;
        }
    } else if RdRand::new().is_none() {
        return false;
    }
    let mut filled = 0;
    while filled < 4 {
        let Some(word) = cpu_u64(rdseed) else {
            return false;
        };
        out[filled * 8..filled * 8 + 8].copy_from_slice(&word.to_ne_bytes());
        filled += 1;
    }
    !uniform(out)
}

fn cpu_u64(rdseed: bool) -> Option<u64> {
    if !rdseed {
        let rdrand = RdRand::new()?;
        for _ in 0..16 {
            if let Some(value) = rdrand.get_u64() {
                return Some(value);
            }
        }
        return None;
    }
    for _ in 0..16 {
        let value: u64;
        let ok: u8;
        // SAFETY: `rdseed` has no memory effects. CF is the success bit.
        // The caller checked CPUID.
        unsafe {
            core::arch::asm!(
                "rdseed {0}",
                "setc {1}",
                out(reg) value,
                out(reg_byte) ok,
                options(nomem, nostack),
            );
        }
        if ok != 0 {
            return Some(value);
        }
    }
    None
}

fn cpuid_bit(leaf: u32, sub: u32, ebx: bool, bit: u32) -> bool {
    let result = core::arch::x86_64::__cpuid_count(leaf, sub);
    let word = if ebx { result.ebx } else { result.ecx };
    word & (1 << bit) != 0
}

fn software_seed() -> [u8; 32] {
    let mut seed = [0u8; 32];
    let mut mixed = rdtsc();
    for _ in 0..32 {
        core::hint::spin_loop();
        mixed = mixed
            .wrapping_mul(0x9E37_79B9_7F4A_7C15)
            .wrapping_add(rdtsc())
            ^ crate::arch::timer_ticks();
    }
    let addr = &seed as *const _ as u64;
    seed[0..8].copy_from_slice(&mixed.to_le_bytes());
    seed[8..16].copy_from_slice(&rdtsc().to_le_bytes());
    seed[16..24].copy_from_slice(&addr.to_le_bytes());
    seed[24..32].copy_from_slice(&mixed.rotate_left(17).to_le_bytes());
    seed
}

fn rdtsc() -> u64 {
    // SAFETY: `rdtsc` is unprivileged and has no memory effects.
    unsafe { core::arch::x86_64::_rdtsc() }
}

fn uniform(bytes: &[u8; 32]) -> bool {
    bytes.iter().all(|b| *b == bytes[0])
}
