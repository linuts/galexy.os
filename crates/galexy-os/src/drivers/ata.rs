//! ATA PIO: LBA28 read/write on the primary IDE slave (second disk).
//!
//! Drive 0 (master) is the boot image — never touch it. Drive 1 (slave)
//! holds the galfs image when the runner attaches one. If the slave is
//! absent, every call returns `Unsupported` and galfs stays RAM-only.

use core::sync::atomic::{AtomicBool, Ordering};

use spin::Mutex;
use x86_64::instructions::port::Port;

use galexy_abi::SysError;

const DATA: u16 = 0x1F0;
const ERROR: u16 = 0x1F1;
const SECTOR_COUNT: u16 = 0x1F2;
const LBA_LO: u16 = 0x1F3;
const LBA_MID: u16 = 0x1F4;
const LBA_HI: u16 = 0x1F5;
const DRIVE: u16 = 0x1F6;
const STATUS: u16 = 0x1F7;
const COMMAND: u16 = 0x1F7;

const SR_ERR: u8 = 1 << 0;
const SR_DRQ: u8 = 1 << 3;
const SR_DF: u8 = 1 << 5;
const SR_BSY: u8 = 1 << 7;

const CMD_READ: u8 = 0x20;
const CMD_WRITE: u8 = 0x30;
const CMD_IDENTIFY: u8 = 0xEC;

/// Bytes in one ATA sector.
pub const SECTOR: usize = 512;
/// Primary IDE slave (index=1).
const SLAVE: u8 = 1;

static READY: AtomicBool = AtomicBool::new(false);
static PROBED: AtomicBool = AtomicBool::new(false);
static LOCK: Mutex<()> = Mutex::new(());

/// Probes the slave once. Returns whether a usable disk is there.
pub fn present() -> bool {
    if PROBED.load(Ordering::Acquire) {
        return READY.load(Ordering::Acquire);
    }
    let _g = LOCK.lock();
    if PROBED.load(Ordering::Acquire) {
        return READY.load(Ordering::Acquire);
    }
    let ok = identify_slave();
    READY.store(ok, Ordering::Release);
    PROBED.store(true, Ordering::Release);
    if ok {
        crate::serial_println!("[ata] primary slave ready (galfs disk)");
    } else {
        crate::serial_println!("[ata] no primary slave; galfs stays in RAM");
    }
    ok
}

fn identify_slave() -> bool {
    select_drive(0);
    delay();
    Port::<u8>::new(SECTOR_COUNT).write(0);
    Port::<u8>::new(LBA_LO).write(0);
    Port::<u8>::new(LBA_MID).write(0);
    Port::<u8>::new(LBA_HI).write(0);
    Port::<u8>::new(COMMAND).write(CMD_IDENTIFY);
    delay();
    let status = Port::<u8>::new(STATUS).read();
    if status == 0 {
        return false;
    }
    if wait_not_bsy().is_err() {
        return false;
    }
    let mid = Port::<u8>::new(LBA_MID).read();
    let hi = Port::<u8>::new(LBA_HI).read();
    if mid != 0 || hi != 0 {
        return false;
    }
    if wait_drq().is_err() {
        return false;
    }
    let mut data = Port::<u16>::new(DATA);
    for _ in 0..256 {
        let _ = data.read();
    }
    true
}

/// Reads `dst.len()` sectors starting at `lba` into `dst` (each SECTOR bytes).
pub fn read_sectors(lba: u32, dst: &mut [[u8; SECTOR]]) -> Result<(), SysError> {
    if dst.is_empty() || dst.len() > 255 {
        return Err(SysError::BadValue);
    }
    if !present() {
        return Err(SysError::Unsupported);
    }
    let _g = LOCK.lock();
    pio_read(lba, dst)
}

/// Writes `src.len()` sectors starting at `lba`.
pub fn write_sectors(lba: u32, src: &[[u8; SECTOR]]) -> Result<(), SysError> {
    if src.is_empty() || src.len() > 255 {
        return Err(SysError::BadValue);
    }
    if !present() {
        return Err(SysError::Unsupported);
    }
    let _g = LOCK.lock();
    pio_write(lba, src)
}

fn select_drive(lba: u32) {
    let head = 0xE0 | (SLAVE << 4) | ((lba >> 24) as u8 & 0x0F);
    Port::<u8>::new(DRIVE).write(head);
    delay();
}

fn pio_read(lba: u32, dst: &mut [[u8; SECTOR]]) -> Result<(), SysError> {
    select_drive(lba);
    Port::<u8>::new(ERROR).write(0);
    Port::<u8>::new(SECTOR_COUNT).write(dst.len() as u8);
    Port::<u8>::new(LBA_LO).write(lba as u8);
    Port::<u8>::new(LBA_MID).write((lba >> 8) as u8);
    Port::<u8>::new(LBA_HI).write((lba >> 16) as u8);
    Port::<u8>::new(COMMAND).write(CMD_READ);
    let mut data = Port::<u16>::new(DATA);
    for sector in dst.iter_mut() {
        wait_drq()?;
        for chunk in sector.chunks_exact_mut(2) {
            let w = data.read();
            chunk[0] = w as u8;
            chunk[1] = (w >> 8) as u8;
        }
    }
    Ok(())
}

fn pio_write(lba: u32, src: &[[u8; SECTOR]]) -> Result<(), SysError> {
    select_drive(lba);
    Port::<u8>::new(ERROR).write(0);
    Port::<u8>::new(SECTOR_COUNT).write(src.len() as u8);
    Port::<u8>::new(LBA_LO).write(lba as u8);
    Port::<u8>::new(LBA_MID).write((lba >> 8) as u8);
    Port::<u8>::new(LBA_HI).write((lba >> 16) as u8);
    Port::<u8>::new(COMMAND).write(CMD_WRITE);
    let mut data = Port::<u16>::new(DATA);
    for sector in src.iter() {
        wait_drq()?;
        for chunk in sector.chunks_exact(2) {
            let w = u16::from(chunk[0]) | (u16::from(chunk[1]) << 8);
            data.write(w);
        }
    }
    wait_not_bsy()?;
    let status = Port::<u8>::new(STATUS).read();
    if status & (SR_ERR | SR_DF) != 0 {
        return Err(SysError::Unsupported);
    }
    Ok(())
}

fn wait_not_bsy() -> Result<(), SysError> {
    let mut status = Port::<u8>::new(STATUS);
    for _ in 0..1_000_000 {
        let s = status.read();
        if s & SR_BSY == 0 {
            if s & SR_ERR != 0 {
                return Err(SysError::Unsupported);
            }
            return Ok(());
        }
    }
    Err(SysError::Unsupported)
}

fn wait_drq() -> Result<(), SysError> {
    let mut status = Port::<u8>::new(STATUS);
    for _ in 0..1_000_000 {
        let s = status.read();
        if s & SR_BSY != 0 {
            continue;
        }
        if s & SR_ERR != 0 {
            return Err(SysError::Unsupported);
        }
        if s & SR_DRQ != 0 {
            return Ok(());
        }
    }
    Err(SysError::Unsupported)
}

fn delay() {
    let mut p = Port::<u8>::new(0x80);
    for _ in 0..4 {
        let _ = p.read();
    }
}
