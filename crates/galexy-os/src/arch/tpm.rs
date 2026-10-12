//! TPM 2.0 `GetRandom` over the interface the ACPI `TPM2` table names.
//!
//! Start method 6 is the FIFO (TIS) locality. Start methods 7 and 8 are
//! the command-response buffer. A missing table, a stuck device, or a
//! rejected command returns `false`. The boot does not wait on it.

use galexy_crypto::{
    get_random_command, parse_get_random, startup_clear_command, startup_ok, RandomReply,
};

use crate::serial_println;

use super::acpi::{self, Tpm2};

/// The ACPI `TPM2` control address is the CRB tail (`ctrl_req`), not the
/// locality block. QEMU publishes `0xFED40040` for start methods 7 and 8.
const CRB_CTRL_REQ: u64 = 0x00;
const CRB_CTRL_START: u64 = 0x0C;
const CRB_CTRL_CMD_SIZE: u64 = 0x18;
const CRB_CTRL_CMD_PA: u64 = 0x1C;
const CRB_CTRL_RSP_SIZE: u64 = 0x24;
const CRB_CTRL_RSP_PA: u64 = 0x28;
/// Locality registers sit 0x40 bytes before the tail.
const CRB_LOC_STATE: u64 = 0x00;
const CRB_LOC_CTRL: u64 = 0x08;
/// `tpmRegValidSts` (bit 7) and `locAssigned` (bit 1).
const CRB_LOC_ASSIGNED: u32 = 0x82;

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
    let ctrl = info.control;
    // Locality is optional. A device that ignores the request still
    // accepts the command on the tail registers.
    crb_request_locality(phys_offset, ctrl.wrapping_sub(0x40));
    if !crb_cmd_ready(phys_offset, ctrl) {
        serial_println!("[tpm] crb not ready");
        return false;
    }
    let cmd_size = read32(phys_offset, ctrl + CRB_CTRL_CMD_SIZE);
    let cmd_pa = read64(phys_offset, ctrl + CRB_CTRL_CMD_PA);
    let rsp_size = read32(phys_offset, ctrl + CRB_CTRL_RSP_SIZE);
    let rsp_pa = read64(phys_offset, ctrl + CRB_CTRL_RSP_PA);
    if cmd_pa == 0 || rsp_pa == 0 || cmd_size < 12 || rsp_size < 44 {
        serial_println!(
            "[tpm] crb buffers cmd={:#x}/{} rsp={:#x}/{}",
            cmd_pa,
            cmd_size,
            rsp_pa,
            rsp_size
        );
        return false;
    }
    let mut resp = [0u8; 64];
    let rsp_len = core::cmp::min(rsp_size as usize, resp.len());
    if !crb_submit(
        phys_offset,
        ctrl,
        cmd_pa,
        rsp_pa,
        rsp_len,
        &get_random_command(32),
        &mut resp,
    ) {
        serial_println!("[tpm] crb start timed out");
        return false;
    }
    match parse_get_random(&resp, out) {
        RandomReply::Bytes(32) => {
            if uniform(out) {
                serial_println!("[tpm] getrandom uniform");
                false
            } else {
                true
            }
        }
        RandomReply::NeedStartup => {
            if !crb_cmd_ready(phys_offset, ctrl)
                || !crb_submit(
                    phys_offset,
                    ctrl,
                    cmd_pa,
                    rsp_pa,
                    rsp_len,
                    &startup_clear_command(),
                    &mut resp,
                )
                || !startup_ok(&resp)
            {
                log_reply("startup", &resp);
                return false;
            }
            if !crb_cmd_ready(phys_offset, ctrl)
                || !crb_submit(
                    phys_offset,
                    ctrl,
                    cmd_pa,
                    rsp_pa,
                    rsp_len,
                    &get_random_command(32),
                    &mut resp,
                )
            {
                serial_println!("[tpm] getrandom after startup timed out");
                return false;
            }
            let ok =
                matches!(parse_get_random(&resp, out), RandomReply::Bytes(32)) && !uniform(out);
            if !ok {
                log_reply("getrandom", &resp);
            }
            ok
        }
        _ => {
            log_reply("getrandom", &resp);
            false
        }
    }
}

/// Writes `requestAccess` and waits briefly. Failure is not fatal.
fn crb_request_locality(phys_offset: u64, loc: u64) {
    write32(phys_offset, loc + CRB_LOC_CTRL, 1);
    // The command still runs if this never sticks. A short wait keeps a
    // device that ignores locality from stalling boot.
    for _ in 0..4096 {
        if read32(phys_offset, loc + CRB_LOC_STATE) & CRB_LOC_ASSIGNED == CRB_LOC_ASSIGNED {
            return;
        }
        core::hint::spin_loop();
    }
}

/// `cmdReady`: write 1 to `ctrl_req` and wait until the device clears it.
fn crb_cmd_ready(phys_offset: u64, ctrl: u64) -> bool {
    write32(phys_offset, ctrl + CRB_CTRL_REQ, 1);
    spin_until(|| read32(phys_offset, ctrl + CRB_CTRL_REQ) & 1 == 0)
}

fn crb_submit(
    phys_offset: u64,
    ctrl: u64,
    cmd_pa: u64,
    rsp_pa: u64,
    rsp_len: usize,
    cmd: &[u8],
    resp: &mut [u8; 64],
) -> bool {
    write_bytes(phys_offset, cmd_pa, cmd);
    fence();
    write32(phys_offset, ctrl + CRB_CTRL_START, 1);
    if !spin_until(|| read32(phys_offset, ctrl + CRB_CTRL_START) & 1 == 0) {
        return false;
    }
    read_bytes(phys_offset, rsp_pa, &mut resp[..rsp_len]);
    true
}

fn log_reply(what: &str, resp: &[u8]) {
    if resp.len() >= 10 {
        let rc = u32::from_be_bytes([resp[6], resp[7], resp[8], resp[9]]);
        serial_println!("[tpm] {} rc={:#x}", what, rc);
    } else {
        serial_println!("[tpm] {} short", what);
    }
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
