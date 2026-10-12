//! TPM 2.0 `GetRandom` over the interface the ACPI `TPM2` table names.
//!
//! Start method 6 is the FIFO (TIS) locality. Start methods 7 and 8 are
//! the command-response buffer. A missing table, a stuck device, or a
//! rejected command returns `false`. The boot does not wait on it.

use galexy_crypto::{
    get_random_command, parse_get_random, startup_clear_command, startup_ok, RandomReply,
};

use super::acpi::{self, Tpm2};

const START_TIS: u32 = 6;
const START_CRB: u32 = 7;
const START_CRB_ACPI: u32 = 8;

const SPINS: u32 = 4_000_000;

/// Fills `out` from `TPM2_GetRandom`. `out` is at most 32 bytes.
pub(super) fn get_random(phys_offset: u64, out: &mut [u8; 32]) -> bool {
    let Some(info) = acpi::tpm2() else {
        return false;
    };
    match info.start_method {
        START_CRB | START_CRB_ACPI => crb_random(phys_offset, info, out),
        START_TIS => tis_random(phys_offset, info, out),
        _ => false,
    }
}

fn crb_random(phys_offset: u64, info: Tpm2, out: &mut [u8; 32]) -> bool {
    let base = info.control;
    if !crb_ready(phys_offset, base) {
        return false;
    }
    let cmd_size = read32(phys_offset, base + 0x58);
    let cmd_pa = read64(phys_offset, base + 0x5C);
    let rsp_size = read32(phys_offset, base + 0x64);
    let rsp_pa = read64(phys_offset, base + 0x68);
    if cmd_pa == 0 || rsp_pa == 0 || cmd_size < 12 || rsp_size < 64 {
        return false;
    }
    let mut resp = [0u8; 64];
    if !crb_submit(
        phys_offset,
        base,
        cmd_pa,
        rsp_pa,
        &get_random_command(32),
        &mut resp,
    ) {
        return false;
    }
    match parse_get_random(&resp, out) {
        RandomReply::Bytes(32) => !uniform(out),
        RandomReply::NeedStartup => {
            if !crb_ready(phys_offset, base) {
                return false;
            }
            if !crb_submit(
                phys_offset,
                base,
                cmd_pa,
                rsp_pa,
                &startup_clear_command(),
                &mut resp,
            ) || !startup_ok(&resp)
            {
                return false;
            }
            if !crb_ready(phys_offset, base) {
                return false;
            }
            if !crb_submit(
                phys_offset,
                base,
                cmd_pa,
                rsp_pa,
                &get_random_command(32),
                &mut resp,
            ) {
                return false;
            }
            matches!(parse_get_random(&resp, out), RandomReply::Bytes(32)) && !uniform(out)
        }
        _ => false,
    }
}

fn crb_ready(phys_offset: u64, base: u64) -> bool {
    write32(phys_offset, base + 0x08, 1);
    if !spin_until(|| read32(phys_offset, base) & 0x82 == 0x82) {
        return false;
    }
    write32(phys_offset, base + 0x40, 1);
    spin_until(|| read32(phys_offset, base + 0x40) & 1 == 0)
}

fn crb_submit(
    phys_offset: u64,
    base: u64,
    cmd_pa: u64,
    rsp_pa: u64,
    cmd: &[u8],
    resp: &mut [u8; 64],
) -> bool {
    write_bytes(phys_offset, cmd_pa, cmd);
    fence();
    write32(phys_offset, base + 0x4C, 1);
    if !spin_until(|| read32(phys_offset, base + 0x4C) & 1 == 0) {
        return false;
    }
    read_bytes(phys_offset, rsp_pa, resp);
    true
}

fn tis_random(phys_offset: u64, info: Tpm2, out: &mut [u8; 32]) -> bool {
    let base = info.control;
    let vid = read32(phys_offset, base + 0xF00);
    if vid == 0 || vid == 0xFFFF_FFFF {
        return false;
    }
    write8(phys_offset, base, 0x02);
    if !spin_until(|| read8(phys_offset, base) & 0xA0 == 0xA0) {
        return false;
    }
    let mut resp = [0u8; 64];
    if !tis_raw(phys_offset, base, &get_random_command(32), &mut resp) {
        return false;
    }
    match parse_get_random(&resp, out) {
        RandomReply::Bytes(32) => !uniform(out),
        RandomReply::NeedStartup => {
            tis_raw(phys_offset, base, &startup_clear_command(), &mut resp)
                && startup_ok(&resp)
                && tis_fill(phys_offset, base, out)
        }
        _ => false,
    }
}

fn tis_fill(phys_offset: u64, base: u64, out: &mut [u8; 32]) -> bool {
    let mut resp = [0u8; 64];
    tis_raw(phys_offset, base, &get_random_command(32), &mut resp)
        && matches!(parse_get_random(&resp, out), RandomReply::Bytes(32))
        && !uniform(out)
}

fn tis_raw(phys_offset: u64, base: u64, cmd: &[u8], resp: &mut [u8; 64]) -> bool {
    write8(phys_offset, base + 0x18, 0x40);
    if !spin_until(|| read8(phys_offset, base + 0x18) & 0xC0 == 0xC0) {
        return false;
    }
    for &byte in cmd {
        if !spin_until(|| tis_burst(phys_offset, base) > 0) {
            return false;
        }
        write8(phys_offset, base + 0x24, byte);
    }
    if !spin_until(|| read8(phys_offset, base + 0x18) & 0x88 == 0x80) {
        return false;
    }
    write8(phys_offset, base + 0x18, 0x20);
    if !spin_until(|| read8(phys_offset, base + 0x18) & 0x90 == 0x90) {
        return false;
    }
    let mut got = 0;
    while got < 10 {
        if tis_burst(phys_offset, base) == 0 && !spin_until(|| tis_burst(phys_offset, base) > 0) {
            return false;
        }
        resp[got] = read8(phys_offset, base + 0x24);
        got += 1;
    }
    let size = u32::from_be_bytes(resp[2..6].try_into().unwrap()) as usize;
    if !(10..=resp.len()).contains(&size) {
        return false;
    }
    while got < size {
        if tis_burst(phys_offset, base) == 0 && !spin_until(|| tis_burst(phys_offset, base) > 0) {
            return false;
        }
        resp[got] = read8(phys_offset, base + 0x24);
        got += 1;
    }
    write8(phys_offset, base + 0x18, 0x40);
    true
}

fn tis_burst(phys_offset: u64, base: u64) -> u16 {
    (read32(phys_offset, base + 0x18) >> 8) as u16
}

fn uniform(bytes: &[u8; 32]) -> bool {
    bytes.iter().all(|b| *b == bytes[0])
}

fn spin_until(mut ready: impl FnMut() -> bool) -> bool {
    for _ in 0..SPINS {
        if ready() {
            return true;
        }
        core::hint::spin_loop();
    }
    false
}

fn fence() {
    core::sync::atomic::fence(core::sync::atomic::Ordering::SeqCst);
}

fn read8(phys_offset: u64, addr: u64) -> u8 {
    // SAFETY: `addr` is a TPM register inside the control area the ACPI
    // table named, and the phys map covers it.
    unsafe { ((phys_offset + addr) as *const u8).read_volatile() }
}

fn read32(phys_offset: u64, addr: u64) -> u32 {
    // SAFETY: same as [`read8`], and the register offsets are 4-aligned.
    unsafe { ((phys_offset + addr) as *const u32).read_volatile() }
}

fn read64(phys_offset: u64, addr: u64) -> u64 {
    let low = read32(phys_offset, addr) as u64;
    let high = read32(phys_offset, addr + 4) as u64;
    low | (high << 32)
}

fn write8(phys_offset: u64, addr: u64, value: u8) {
    // SAFETY: same as [`read8`]. The write targets a command or status register.
    unsafe { ((phys_offset + addr) as *mut u8).write_volatile(value) }
}

fn write32(phys_offset: u64, addr: u64, value: u32) {
    // SAFETY: same as [`read32`].
    unsafe { ((phys_offset + addr) as *mut u32).write_volatile(value) }
}

fn write_bytes(phys_offset: u64, phys: u64, bytes: &[u8]) {
    for (i, byte) in bytes.iter().enumerate() {
        write8(phys_offset, phys + i as u64, *byte);
    }
}

fn read_bytes(phys_offset: u64, phys: u64, out: &mut [u8]) {
    for (i, byte) in out.iter_mut().enumerate() {
        *byte = read8(phys_offset, phys + i as u64);
    }
}
