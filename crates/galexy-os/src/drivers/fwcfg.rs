//! QEMU fw_cfg (I/O ports `0x510` / `0x511`).
//!
//! `cargo run -- --debug` publishes `opt/galexy.debug` with the byte
//! `1`. The suite publishes the same item. A boot without it leaves
//! kernel log lines in the dmesg ring and off COM1. The device is part
//! of the PC and q35 machines; a missing signature means "not QEMU".

use x86_64::instructions::port::Port;

const PORT_SEL: u16 = 0x510;
const PORT_DATA: u16 = 0x511;
/// `FW_CFG_SIGNATURE`.
const KEY_SIGNATURE: u16 = 0x00;
/// `FW_CFG_FILE_DIR`.
const KEY_FILE_DIR: u16 = 0x19;
const FILE_NAME: &[u8] = b"opt/galexy.debug";
/// A wild directory count means the ports are not fw_cfg.
const MAX_FILES: u32 = 128;

/// True when the published item's first byte is `1`.
pub fn debug_requested() -> bool {
    if !signature_is_qemu() {
        return false;
    }
    let Some(select) = find_file(FILE_NAME) else {
        return false;
    };
    select_key(select);
    read_byte() == b'1'
}

fn signature_is_qemu() -> bool {
    select_key(KEY_SIGNATURE);
    let mut sig = [0u8; 4];
    for slot in &mut sig {
        *slot = read_byte();
    }
    &sig == b"QEMU"
}

/// Selector of `name`, if the directory lists it.
fn find_file(name: &[u8]) -> Option<u16> {
    select_key(KEY_FILE_DIR);
    let count = read_be32();
    if count > MAX_FILES {
        return None;
    }
    for _ in 0..count {
        let _size = read_be32();
        let select = read_be16();
        let _reserved = read_be16();
        let mut raw = [0u8; 56];
        for slot in &mut raw {
            *slot = read_byte();
        }
        let end = raw.iter().position(|byte| *byte == 0).unwrap_or(raw.len());
        if &raw[..end] == name {
            return Some(select);
        }
    }
    None
}

/// The I/O selector is the key as a native `outw`. The MMIO control
/// register is big-endian; this port is not.
fn select_key(key: u16) {
    // SAFETY: 0x510 is the QEMU fw_cfg selector. Other firmware reads
    // back nothing useful and `signature_is_qemu` rejects it.
    unsafe {
        Port::<u16>::new(PORT_SEL).write(key);
    }
}

fn read_byte() -> u8 {
    // SAFETY: 0x511 is the fw_cfg data port; each read advances one byte.
    unsafe { Port::<u8>::new(PORT_DATA).read() }
}

fn read_be16() -> u16 {
    let hi = read_byte() as u16;
    let lo = read_byte() as u16;
    (hi << 8) | lo
}

fn read_be32() -> u32 {
    let mut value = 0u32;
    for _ in 0..4 {
        value = (value << 8) | read_byte() as u32;
    }
    value
}
