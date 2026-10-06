//! IEEE CRC-32 (ISO 3309 / PNG / Ethernet polynomial).

const POLY: u32 = 0xEDB8_8320;

/// CRC-32 of `data`, IEEE reflected, init `0xffff_ffff`, final xor `0xffff_ffff`.
pub fn crc32(data: &[u8]) -> u32 {
    let mut crc = 0xffff_ffff_u32;
    for &byte in data {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            if crc & 1 != 0 {
                crc = (crc >> 1) ^ POLY;
            } else {
                crc >>= 1;
            }
        }
    }
    !crc
}
