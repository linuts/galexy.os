//! Block device abstraction for durable storage.
//!
//! galfs talks only to [`BlockDevice`]. ATA PIO (primary IDE slave) is the
//! first impl; virtio-blk / primary master can plug in later without
//! rewriting the filesystem.

use galexy_abi::SysError;

/// Bytes in one logical sector (ATA / virtio common size).
pub const SECTOR: usize = 512;

/// Random-access sector device with write-back flush.
pub trait BlockDevice: Sync {
    /// Whether the device answered probe / identify.
    fn present(&self) -> bool;

    /// Addressable sector count (LBA). `0` if unknown / absent.
    fn capacity_sectors(&self) -> u64;

    /// Read `dst.len()` sectors starting at `lba`.
    fn read_sectors(&self, lba: u32, dst: &mut [[u8; SECTOR]]) -> Result<(), SysError>;

    /// Write `src.len()` sectors starting at `lba`.
    fn write_sectors(&self, lba: u32, src: &[[u8; SECTOR]]) -> Result<(), SysError>;

    /// Push prior writes to stable media (FLUSH CACHE / virtio barrier).
    fn flush(&self) -> Result<(), SysError>;
}

const _: () = assert!(SECTOR == crate::drivers::ata::SECTOR);
