//! Virtio-blk (legacy PCI) as a [`BlockDevice`].
//!
//! QEMU: `-drive if=none,id=galfs,file=galfs.img,format=raw` plus
//! `-device virtio-blk-pci,drive=galfs,disable-legacy=off,disable-modern=on`.
//! Polling only — no MSI-X. One outstanding request at a time.

use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use spin::Mutex;
use x86_64::instructions::port::Port;
use x86_64::VirtAddr;

use galexy_abi::SysError;

use super::block::{BlockDevice, SECTOR};
use super::pci;
use crate::arch::mm;

const VIRTIO_VENDOR: u16 = 0x1AF4;
const VIRTIO_BLK_DEVICE: u16 = 0x1001;

const REG_HOST_FEATURES: u16 = 0x00;
const REG_GUEST_FEATURES: u16 = 0x04;
const REG_QUEUE_PFN: u16 = 0x08;
const REG_QUEUE_SIZE: u16 = 0x0C;
const REG_QUEUE_SELECT: u16 = 0x0E;
const REG_QUEUE_NOTIFY: u16 = 0x10;
const REG_STATUS: u16 = 0x12;
const REG_ISR: u16 = 0x13;
const REG_CONFIG_CAPACITY: u16 = 0x14;

const STATUS_ACKNOWLEDGE: u8 = 1;
const STATUS_DRIVER: u8 = 2;
const STATUS_DRIVER_OK: u8 = 4;
const STATUS_FAILED: u8 = 0x80;

const VRING_DESC_F_NEXT: u16 = 1;
const VRING_DESC_F_WRITE: u16 = 2;

const VIRTIO_BLK_T_IN: u32 = 0;
const VIRTIO_BLK_T_OUT: u32 = 1;
const VIRTIO_BLK_T_FLUSH: u32 = 4;

/// QEMU virtio-blk default queue length; layout must match the device.
const QUEUE_SIZE: usize = 128;
const DESC_BYTES: usize = QUEUE_SIZE * 16;
const AVAIL_BYTES: usize = 4 + 2 * QUEUE_SIZE;
const PAD_TO_USED: usize = 4096 - DESC_BYTES - AVAIL_BYTES;

#[repr(C)]
#[derive(Clone, Copy)]
struct VirtqDesc {
    addr: u64,
    len: u32,
    flags: u16,
    next: u16,
}

#[repr(C)]
struct VirtqAvail {
    flags: u16,
    idx: u16,
    ring: [u16; QUEUE_SIZE],
}

#[repr(C)]
#[derive(Clone, Copy)]
struct VirtqUsedElem {
    id: u32,
    len: u32,
}

#[repr(C)]
struct VirtqUsed {
    flags: u16,
    idx: u16,
    ring: [VirtqUsedElem; QUEUE_SIZE],
}

#[repr(C)]
struct BlkReq {
    type_: u32,
    reserved: u32,
    sector: u64,
}

/// Two-page legacy virtqueue: desc+avail on page 0, used on page 1.
#[repr(C, align(4096))]
struct DmaRegion {
    desc: [VirtqDesc; QUEUE_SIZE],
    avail: VirtqAvail,
    _pad: [u8; PAD_TO_USED],
    used: VirtqUsed,
    req: BlkReq,
    status: u8,
    _tail: [u8; 7],
}

const _: () = assert!(core::mem::offset_of!(DmaRegion, used) == 4096);
const _: () = assert!(core::mem::size_of::<VirtqDesc>() == 16);
const _: () = assert!(core::mem::size_of::<VirtqAvail>() == AVAIL_BYTES);

struct DeviceState {
    io_base: u16,
    last_used: u16,
}

static PROBED: AtomicBool = AtomicBool::new(false);
static READY: AtomicBool = AtomicBool::new(false);
static CAPACITY: AtomicU64 = AtomicU64::new(0);
static DEV: Mutex<Option<DeviceState>> = Mutex::new(None);

// Zero-init via MaybeUninit pattern — large static, filled in probe.
static DMA: Mutex<DmaRegion> = Mutex::new(DmaRegion {
    desc: [VirtqDesc {
        addr: 0,
        len: 0,
        flags: 0,
        next: 0,
    }; QUEUE_SIZE],
    avail: VirtqAvail {
        flags: 0,
        idx: 0,
        ring: [0; QUEUE_SIZE],
    },
    _pad: [0; PAD_TO_USED],
    used: VirtqUsed {
        flags: 0,
        idx: 0,
        ring: [VirtqUsedElem { id: 0, len: 0 }; QUEUE_SIZE],
    },
    req: BlkReq {
        type_: 0,
        reserved: 0,
        sector: 0,
    },
    status: 0xFF,
    _tail: [0; 7],
});

/// Virtio-blk disk selected by galfs when present.
pub struct VirtioBlk;

impl BlockDevice for VirtioBlk {
    fn present(&self) -> bool {
        present()
    }

    fn capacity_sectors(&self) -> u64 {
        capacity_sectors()
    }

    fn read_sectors(&self, lba: u32, dst: &mut [[u8; SECTOR]]) -> Result<(), SysError> {
        read_sectors(lba, dst)
    }

    fn write_sectors(&self, lba: u32, src: &[[u8; SECTOR]]) -> Result<(), SysError> {
        write_sectors(lba, src)
    }

    fn flush(&self) -> Result<(), SysError> {
        flush()
    }
}

/// Probes PCI once for a legacy virtio-blk IO BAR.
pub fn present() -> bool {
    if PROBED.load(Ordering::Acquire) {
        return READY.load(Ordering::Acquire);
    }
    let ok = probe();
    READY.store(ok, Ordering::Release);
    PROBED.store(true, Ordering::Release);
    ok
}

/// Sector capacity from the virtio config space (`0` if absent).
pub fn capacity_sectors() -> u64 {
    if !present() {
        return 0;
    }
    CAPACITY.load(Ordering::Acquire)
}

/// Reads `dst.len()` sectors starting at `lba`.
///
/// Batches physically contiguous runs like [`write_sectors`].
pub fn read_sectors(lba: u32, dst: &mut [[u8; SECTOR]]) -> Result<(), SysError> {
    if dst.is_empty() {
        return Err(SysError::BadValue);
    }
    if !present() {
        return Err(SysError::Unsupported);
    }
    check_range(lba, dst.len())?;
    let mut i = 0usize;
    while i < dst.len() {
        let n = contiguous_sectors(&dst[i..]);
        xfer(
            VIRTIO_BLK_T_IN,
            lba + i as u32,
            dst[i].as_mut_ptr(),
            n * SECTOR,
        )?;
        i += n;
    }
    Ok(())
}

/// Writes `src.len()` sectors starting at `lba`.
///
/// Batches physically contiguous runs (same mapped page) into one virtio
/// request so a 288-sector GALF slot is dozens of kicks, not 288.
pub fn write_sectors(lba: u32, src: &[[u8; SECTOR]]) -> Result<(), SysError> {
    if src.is_empty() {
        return Err(SysError::BadValue);
    }
    if !present() {
        return Err(SysError::Unsupported);
    }
    check_range(lba, src.len())?;
    let mut i = 0usize;
    while i < src.len() {
        let n = contiguous_sectors(&src[i..]);
        // SAFETY: xfer only reads `n * SECTOR` bytes for OUT; slice lives for the call.
        xfer(
            VIRTIO_BLK_T_OUT,
            lba + i as u32,
            src[i].as_ptr().cast_mut(),
            n * SECTOR,
        )?;
        i += n;
    }
    Ok(())
}

/// How many leading sectors of `src` share one physically contiguous run.
fn contiguous_sectors(src: &[[u8; SECTOR]]) -> usize {
    if src.is_empty() {
        return 0;
    }
    let base_virt = VirtAddr::new(core::ptr::from_ref(&src[0]) as u64);
    let Some(base_phys) = mm::translate(base_virt) else {
        return 1;
    };
    let page_left = (0x1000 - (base_phys.as_u64() as usize & 0xFFF)) / SECTOR;
    let max = src.len().min(page_left.max(1));
    let mut n = 1usize;
    while n < max {
        let v = VirtAddr::new(core::ptr::from_ref(&src[n]) as u64);
        let Some(p) = mm::translate(v) else {
            break;
        };
        if p.as_u64() != base_phys.as_u64() + (n * SECTOR) as u64 {
            break;
        }
        n += 1;
    }
    n
}

/// Issues a virtio-blk FLUSH.
pub fn flush() -> Result<(), SysError> {
    if !present() {
        return Err(SysError::Unsupported);
    }
    xfer(VIRTIO_BLK_T_FLUSH, 0, core::ptr::null_mut(), 0)
}

fn check_range(lba: u32, count: usize) -> Result<(), SysError> {
    let cap = CAPACITY.load(Ordering::Acquire);
    let end = u64::from(lba).saturating_add(count as u64);
    if count == 0 || end > cap {
        return Err(SysError::BadValue);
    }
    Ok(())
}

fn probe() -> bool {
    let Some(dev) = pci::find(VIRTIO_VENDOR, VIRTIO_BLK_DEVICE) else {
        return false;
    };
    pci::enable_bus_master(dev);
    let Some(io) = pci::io_bar0(dev) else {
        crate::serial_println!("[virtio-blk] BAR0 is not IO-mapped (need transitional)");
        return false;
    };

    outb(io + REG_STATUS, 0);
    outb(io + REG_STATUS, STATUS_ACKNOWLEDGE);
    outb(io + REG_STATUS, STATUS_ACKNOWLEDGE | STATUS_DRIVER);

    let _host = inl(io + REG_HOST_FEATURES);
    outl(io + REG_GUEST_FEATURES, 0);

    outw(io + REG_QUEUE_SELECT, 0);
    let qsz = usize::from(inw(io + REG_QUEUE_SIZE));
    if qsz != QUEUE_SIZE {
        crate::serial_println!(
            "[virtio-blk] unexpected queue size {} (need {})",
            qsz,
            QUEUE_SIZE
        );
        outb(io + REG_STATUS, STATUS_FAILED);
        return false;
    }

    let mut dma = DMA.lock();
    zero_dma(&mut dma);

    let virt = VirtAddr::new(core::ptr::from_ref(&*dma) as u64);
    let Some(phys) = mm::translate(virt) else {
        crate::serial_println!("[virtio-blk] DMA page not mapped");
        outb(io + REG_STATUS, STATUS_FAILED);
        return false;
    };
    if phys.as_u64() & 0xFFF != 0 {
        crate::serial_println!("[virtio-blk] DMA page not aligned");
        outb(io + REG_STATUS, STATUS_FAILED);
        return false;
    }

    // Legacy used ring is on the next page — require physical contiguity.
    let used_virt = VirtAddr::new(core::ptr::from_ref(&dma.used) as u64);
    let Some(used_phys) = mm::translate(used_virt) else {
        crate::serial_println!("[virtio-blk] used ring not mapped");
        outb(io + REG_STATUS, STATUS_FAILED);
        return false;
    };
    if used_phys.as_u64() != phys.as_u64() + 4096 {
        crate::serial_println!(
            "[virtio-blk] used ring not physically contiguous (desc {:#x} used {:#x})",
            phys.as_u64(),
            used_phys.as_u64()
        );
        outb(io + REG_STATUS, STATUS_FAILED);
        return false;
    }

    outl(io + REG_QUEUE_PFN, (phys.as_u64() / 4096) as u32);

    let capacity = inq(io + REG_CONFIG_CAPACITY);
    if capacity == 0 {
        crate::serial_println!("[virtio-blk] zero capacity");
        outb(io + REG_STATUS, STATUS_FAILED);
        return false;
    }

    outb(
        io + REG_STATUS,
        STATUS_ACKNOWLEDGE | STATUS_DRIVER | STATUS_DRIVER_OK,
    );
    drop(dma);

    *DEV.lock() = Some(DeviceState {
        io_base: io,
        last_used: 0,
    });
    CAPACITY.store(capacity, Ordering::Release);
    crate::serial_println!(
        "[virtio-blk] ready ({} sectors, io=0x{:x})",
        capacity,
        io
    );
    true
}

fn zero_dma(dma: &mut DmaRegion) {
    for d in dma.desc.iter_mut() {
        *d = VirtqDesc {
            addr: 0,
            len: 0,
            flags: 0,
            next: 0,
        };
    }
    dma.avail.flags = 0;
    dma.avail.idx = 0;
    for r in dma.avail.ring.iter_mut() {
        *r = 0;
    }
    dma.used.flags = 0;
    dma.used.idx = 0;
    for r in dma.used.ring.iter_mut() {
        *r = VirtqUsedElem { id: 0, len: 0 };
    }
    dma.req = BlkReq {
        type_: 0,
        reserved: 0,
        sector: 0,
    };
    dma.status = 0xFF;
}

fn xfer(type_: u32, lba: u32, buf: *mut u8, len: usize) -> Result<(), SysError> {
    let mut dev = DEV.lock();
    let Some(state) = dev.as_mut() else {
        return Err(SysError::Unsupported);
    };
    let mut dma = DMA.lock();
    let io = state.io_base;

    dma.req.type_ = type_;
    dma.req.reserved = 0;
    dma.req.sector = u64::from(lba);
    // SAFETY: status is device-written; clear before kick.
    unsafe {
        core::ptr::write_volatile(&mut dma.status, 0xFF);
    }

    let req_phys = phys_of(&dma.req)?;
    let status_phys = phys_of(&dma.status)?;

    // SAFETY: descriptor table is host-owned until notify.
    unsafe {
        core::ptr::write_volatile(
            &mut dma.desc[0],
            VirtqDesc {
                addr: req_phys,
                len: core::mem::size_of::<BlkReq>() as u32,
                flags: VRING_DESC_F_NEXT,
                next: 1,
            },
        );
    }

    let status_idx = if len > 0 {
        let buf_phys = mm::translate(VirtAddr::new(buf as u64))
            .ok_or(SysError::Unsupported)?
            .as_u64();
        let mut flags = VRING_DESC_F_NEXT;
        if type_ == VIRTIO_BLK_T_IN {
            flags |= VRING_DESC_F_WRITE;
        }
        // SAFETY: as above.
        unsafe {
            core::ptr::write_volatile(
                &mut dma.desc[1],
                VirtqDesc {
                    addr: buf_phys,
                    len: len as u32,
                    flags,
                    next: 2,
                },
            );
        }
        2u16
    } else {
        1u16
    };

    // SAFETY: as above.
    unsafe {
        core::ptr::write_volatile(
            &mut dma.desc[status_idx as usize],
            VirtqDesc {
                addr: status_phys,
                len: 1,
                flags: VRING_DESC_F_WRITE,
                next: 0,
            },
        );
    }

    let avail_idx = dma.avail.idx;
    let slot = (avail_idx as usize) % QUEUE_SIZE;
    // SAFETY: avail ring is host-written, device-read after idx bump.
    unsafe {
        core::ptr::write_volatile(&mut dma.avail.ring[slot], 0);
    }
    core::sync::atomic::fence(Ordering::SeqCst);
    // SAFETY: publish new available index.
    unsafe {
        core::ptr::write_volatile(&mut dma.avail.idx, avail_idx.wrapping_add(1));
    }
    core::sync::atomic::fence(Ordering::SeqCst);

    outw(io + REG_QUEUE_NOTIFY, 0);

    let want = state.last_used.wrapping_add(1);
    let mut done = false;
    for _ in 0..10_000_000 {
        core::sync::atomic::fence(Ordering::SeqCst);
        // SAFETY: used.idx is device-written.
        let used_idx = unsafe { core::ptr::read_volatile(&dma.used.idx) };
        if used_idx == want {
            state.last_used = want;
            done = true;
            break;
        }
        // Ack ISR in case the device latches completion there.
        let _ = inb(io + REG_ISR);
    }
    if !done {
        crate::serial_println!("[virtio-blk] request timeout");
        return Err(SysError::Unsupported);
    }
    // SAFETY: status byte written by device.
    let status = unsafe { core::ptr::read_volatile(&dma.status) };
    if status != 0 {
        crate::serial_println!("[virtio-blk] status {}", status);
        return Err(SysError::Unsupported);
    }
    Ok(())
}

fn phys_of<T>(r: &T) -> Result<u64, SysError> {
    let v = VirtAddr::new(core::ptr::from_ref(r) as u64);
    mm::translate(v)
        .map(|p| p.as_u64())
        .ok_or(SysError::Unsupported)
}

fn outb(port: u16, value: u8) {
    // SAFETY: virtio-blk legacy IO BAR registers.
    let mut p = Port::<u8>::new(port);
    unsafe { p.write(value) };
}

fn inb(port: u16) -> u8 {
    // SAFETY: virtio-blk legacy IO BAR registers.
    let mut p = Port::<u8>::new(port);
    unsafe { p.read() }
}

fn inw(port: u16) -> u16 {
    // SAFETY: virtio-blk legacy IO BAR registers.
    let mut p = Port::<u16>::new(port);
    unsafe { p.read() }
}

fn outw(port: u16, value: u16) {
    // SAFETY: virtio-blk legacy IO BAR registers.
    let mut p = Port::<u16>::new(port);
    unsafe { p.write(value) };
}

fn inl(port: u16) -> u32 {
    // SAFETY: virtio-blk legacy IO BAR registers.
    let mut p = Port::<u32>::new(port);
    unsafe { p.read() }
}

fn outl(port: u16, value: u32) {
    // SAFETY: virtio-blk legacy IO BAR registers.
    let mut p = Port::<u32>::new(port);
    unsafe { p.write(value) };
}

fn inq(port: u16) -> u64 {
    let lo = u64::from(inl(port));
    let hi = u64::from(inl(port + 4));
    lo | (hi << 32)
}
