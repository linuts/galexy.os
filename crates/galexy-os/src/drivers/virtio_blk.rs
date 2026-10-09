//! Virtio-blk as a [`BlockDevice`].
//!
//! Prefers virtio 1.x (MMIO BARs, `VIRTIO_F_VERSION_1`, MSI-X). The legacy
//! I/O BAR path remains when the function has no modern capabilities, and
//! names itself on the serial line. Completion parks `STATE_WAITING` +
//! `IO_BLOCK`. One outstanding request at a time. A missed interrupt is
//! noticed on the next timer tick.

use core::sync::atomic::{AtomicBool, AtomicU16, AtomicU64, AtomicUsize, Ordering};

use crate::sync::Mutex;
use x86_64::instructions::port::Port;
use x86_64::VirtAddr;

use galexy_abi::SysError;

use super::block::{BlockDevice, SECTOR};
use super::pci;
use super::virtio_pci;
use crate::arch::mm;

const VIRTIO_VENDOR: u16 = 0x1AF4;
/// Legacy / transitional virtio-blk.
const VIRTIO_BLK_LEGACY: u16 = 0x1001;
/// Modern-only virtio-blk (`0x1040 + 2`).
const VIRTIO_BLK_MODERN: u16 = 0x1042;
/// `VIRTIO_BLK_F_FLUSH` in feature word 0.
const VIRTIO_BLK_F_FLUSH: u32 = 1 << 9;

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

/// IDT vector for the virtio-blk INTx line.
pub const VIRTIO_VECTOR: u8 = 0x41;

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

enum Transport {
    /// Legacy I/O BAR. `io` is the port base.
    Legacy { io: u16 },
    /// Virtio 1.x MMIO. `notify_off` is the queue 0 notify offset.
    Modern {
        dev: virtio_pci::Modern,
        notify_off: u16,
    },
}

struct DeviceState {
    transport: Transport,
    last_used: u16,
}

static PROBED: AtomicBool = AtomicBool::new(false);
static READY: AtomicBool = AtomicBool::new(false);
static CAPACITY: AtomicU64 = AtomicU64::new(0);
static IO_BASE: AtomicU16 = AtomicU16::new(0);
/// Virtual address of the virtio 1.x ISR byte, or 0 on the legacy path.
static ISR_VIRT: AtomicU64 = AtomicU64::new(0);
/// True when completion is an I/O APIC level line (needs an IOAPIC EOI).
static INTX_ROUTED: AtomicBool = AtomicBool::new(false);
/// `used.idx` the in-flight request is waiting for. Armed only while
/// a requester is parked or about to park.
static WANT_USED: AtomicU16 = AtomicU16::new(0);
static WANT_ARMED: AtomicBool = AtomicBool::new(false);
/// CPU index inside `wait_used`, or `usize::MAX` when no transfer is
/// parked. The scheduler must not switch that CPU away: the caller
/// still holds [`DEV`] and, for galfs, the sector buffer.
static XFER_CPU: AtomicUsize = AtomicUsize::new(usize::MAX);
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
    let Some(dev) = pci::find_any(VIRTIO_VENDOR, &[VIRTIO_BLK_LEGACY, VIRTIO_BLK_MODERN]) else {
        return false;
    };
    pci::enable_bus_master(dev);
    if let Some(modern) = virtio_pci::claim(dev) {
        if probe_modern(dev, modern) {
            return true;
        }
        crate::serial_println!("[virtio-blk] virtio 1.x setup failed; trying legacy IO BAR");
    }
    if dev.device != VIRTIO_BLK_LEGACY {
        return false;
    }
    probe_legacy(dev)
}

fn probe_modern(dev: pci::Device, modern: virtio_pci::Modern) -> bool {
    if !virtio_pci::negotiate(&modern, VIRTIO_BLK_F_FLUSH) {
        crate::serial_println!("[virtio-blk] device refused VIRTIO_F_VERSION_1");
        return false;
    }
    let mut dma = DMA.lock();
    zero_dma(&mut dma);
    let virt = VirtAddr::new(core::ptr::from_ref(&*dma) as u64);
    let Some(phys) = mm::translate(virt) else {
        crate::serial_println!("[virtio-blk] DMA page not mapped");
        return false;
    };
    let used_virt = VirtAddr::new(core::ptr::from_ref(&dma.used) as u64);
    let Some(used_phys) = mm::translate(used_virt) else {
        crate::serial_println!("[virtio-blk] used ring not mapped");
        return false;
    };
    let Some(notify_off) = virtio_pci::setup_queue(
        &modern,
        0,
        QUEUE_SIZE as u16,
        phys.as_u64(),
        phys.as_u64() + core::mem::offset_of!(DmaRegion, avail) as u64,
        used_phys.as_u64(),
    ) else {
        crate::serial_println!("[virtio-blk] virtio 1.x queue rejected");
        return false;
    };
    let capacity = virtio_pci::read_dev_u64(&modern, 0);
    if capacity == 0 {
        crate::serial_println!("[virtio-blk] zero capacity");
        return false;
    }
    drop(dma);
    ISR_VIRT.store(modern.isr_addr(), Ordering::Release);
    let msix = virtio_pci::enable_msix(
        dev,
        &modern,
        0,
        0,
        VIRTIO_VECTOR,
        crate::arch::apic::lapic_id(),
    );
    let route = if msix {
        "msi-x"
    } else if wire_intx(dev) {
        "intx"
    } else {
        "no-irq"
    };
    virtio_pci::driver_ok(&modern);
    *DEV.lock() = Some(DeviceState {
        transport: Transport::Modern {
            dev: modern,
            notify_off,
        },
        last_used: 0,
    });
    CAPACITY.store(capacity, Ordering::Release);
    crate::serial_println!(
        "[virtio-blk] virtio 1.x ready ({} sectors, {})",
        capacity,
        route
    );
    true
}

fn wire_intx(dev: pci::Device) -> bool {
    let line = pci::interrupt_line(dev);
    if line == 0
        || !crate::arch::ioapic::wire_pci_level(u32::from(line), VIRTIO_VECTOR, "virtio-blk")
    {
        return false;
    }
    INTX_ROUTED.store(true, Ordering::Release);
    true
}

fn probe_legacy(dev: pci::Device) -> bool {
    let Some(io) = pci::io_bar0(dev) else {
        crate::serial_println!("[virtio-blk] legacy IO BAR missing");
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

    IO_BASE.store(io, Ordering::Release);
    outb(
        io + REG_STATUS,
        STATUS_ACKNOWLEDGE | STATUS_DRIVER | STATUS_DRIVER_OK,
    );
    drop(dma);

    let route = if wire_intx(dev) { "intx" } else { "no-irq" };
    *DEV.lock() = Some(DeviceState {
        transport: Transport::Legacy { io },
        last_used: 0,
    });
    CAPACITY.store(capacity, Ordering::Release);
    crate::serial_println!(
        "[virtio-blk] legacy IO BAR ready ({} sectors, io=0x{:x}, {})",
        capacity,
        io,
        route
    );
    true
}

/// Acknowledges the virtio interrupt (1.x ISR byte, or the legacy port).
pub fn ack_isr() {
    let isr = ISR_VIRT.load(Ordering::Acquire);
    if isr != 0 {
        let _ = virtio_pci::ack_isr(isr);
        return;
    }
    let io = IO_BASE.load(Ordering::Acquire);
    if io == 0 {
        return;
    }
    let _ = inb(io + REG_ISR);
}

/// True when virtio-blk completion is a level-triggered INTx line.
pub fn intx_routed() -> bool {
    INTX_ROUTED.load(Ordering::Acquire)
}

/// CPU currently halted inside a transfer, if any.
///
/// The scheduler keeps that CPU on the waiter. A switch would run
/// another thread on the same stack of locks (`DEV`, the galfs sector
/// buffer) and the waiter would never resume.
pub fn xfer_wait_cpu() -> Option<usize> {
    let cpu = XFER_CPU.load(Ordering::Acquire);
    if cpu == usize::MAX {
        None
    } else {
        Some(cpu)
    }
}

/// True when an in-flight request's used index has landed.
pub fn completion_ready() -> bool {
    if !WANT_ARMED.load(Ordering::Acquire) {
        return false;
    }
    used_idx() == WANT_USED.load(Ordering::Acquire)
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
    let enable_after = x86_64::instructions::interrupts::are_enabled();
    x86_64::instructions::interrupts::disable();
    let mut dev = DEV.lock();
    let Some(state) = dev.as_mut() else {
        drop(dev);
        if enable_after {
            x86_64::instructions::interrupts::enable();
        }
        return Err(SysError::Unsupported);
    };
    let submitted = submit_xfer(state, type_, lba, buf, len);
    let want = match submitted {
        Ok(want) => want,
        Err(err) => {
            drop(dev);
            if enable_after {
                x86_64::instructions::interrupts::enable();
            }
            return Err(err);
        }
    };
    // Held across the halt so a second CPU cannot reuse the one queue.
    // `XFER_CPU` stops THIS CPU's timer from switching off the holder.
    XFER_CPU.store(crate::arch::cpu::current_index(), Ordering::Release);
    let ready = wait_used(want);
    x86_64::instructions::interrupts::disable();
    XFER_CPU.store(usize::MAX, Ordering::Release);
    let status = if ready {
        finish_xfer(state, want)
    } else {
        crate::serial_println!("[virtio-blk] request timeout");
        Err(SysError::Unsupported)
    };
    drop(dev);
    if enable_after {
        x86_64::instructions::interrupts::enable();
    }
    status
}

fn submit_xfer(
    state: &mut DeviceState,
    type_: u32,
    lba: u32,
    buf: *mut u8,
    len: usize,
) -> Result<u16, SysError> {
    let mut dma = DMA.lock();
    let transport = &state.transport;

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

    match transport {
        Transport::Legacy { io } => outw(io + REG_QUEUE_NOTIFY, 0),
        Transport::Modern { dev, notify_off } => virtio_pci::notify(dev, 0, *notify_off),
    }

    let want = state.last_used.wrapping_add(1);
    drop(dma);
    Ok(want)
}

fn finish_xfer(state: &mut DeviceState, want: u16) -> Result<(), SysError> {
    let dma = DMA.lock();
    state.last_used = want;
    // SAFETY: status byte written by device.
    let status = unsafe { core::ptr::read_volatile(&dma.status) };
    if status != 0 {
        crate::serial_println!("[virtio-blk] status {}", status);
        return Err(SysError::Unsupported);
    }
    Ok(())
}

fn used_idx() -> u16 {
    // The timer reads this from `completion_ready`. A lock held with
    // interrupts open deadlocks that handler on the same CPU.
    x86_64::instructions::interrupts::without_interrupts(|| {
        let dma = DMA.lock();
        // SAFETY: the device owns this halfword; a torn read is retried.
        unsafe { core::ptr::read_volatile(&dma.used.idx) }
    })
}

/// Parks a user requester until `used.idx == want`. The main loop (no
/// task) halts instead. Either way the 10 M-spin poll is gone.
fn wait_used(want: u16) -> bool {
    WANT_USED.store(want, Ordering::Release);
    WANT_ARMED.store(true, Ordering::Release);
    if used_idx() == want {
        WANT_ARMED.store(false, Ordering::Release);
        return true;
    }
    let parked = crate::sched::current_slot() != 0 && crate::sched::park_io_block().is_ok();
    let start = crate::arch::timer_ticks();
    let mut ok = false;
    loop {
        if used_idx() == want {
            ok = true;
            break;
        }
        if crate::arch::timer_ticks().saturating_sub(start) > 2000 {
            break;
        }
        x86_64::instructions::interrupts::enable_and_hlt();
    }
    if parked {
        crate::sched::clear_io_block();
    }
    WANT_ARMED.store(false, Ordering::Release);
    ok
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
