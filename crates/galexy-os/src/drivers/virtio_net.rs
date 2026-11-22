//! Virtio-net 1.x on the q35 PCI path.
//!
//! The kernel moves Ethernet frames. It does not parse IP. One descriptor
//! per physically contiguous run, the same rule as virtio-blk. The
//! interrupt handler counts and wakes; it does not look at a header.
//!
//! Only the task named `net` may `read` or `write` the wire Cap. This
//! module is the device. The syscall gate is in `sched`.

use core::sync::atomic::{AtomicBool, AtomicU16, AtomicU64, Ordering};

use crate::sync::Mutex;
use x86_64::VirtAddr;

use galexy_abi::SysError;

use super::pci;
use super::virtio_pci;
use crate::arch::mm;

const VIRTIO_VENDOR: u16 = 0x1AF4;
/// Modern-only virtio-net (`0x1040 + 1`).
const VIRTIO_NET_MODERN: u16 = 0x1041;
/// Transitional virtio-net. Accepted only when the 1.x capabilities are present.
const VIRTIO_NET_TRANS: u16 = 0x1000;

/// `VIRTIO_NET_F_MAC`.
const F_MAC: u32 = 1 << 5;
/// `VIRTIO_NET_F_STATUS`.
const F_STATUS: u32 = 1 << 16;
/// Link-up bit in the status field.
const S_LINK_UP: u16 = 1;

const VRING_DESC_F_NEXT: u16 = 1;
const VRING_DESC_F_WRITE: u16 = 2;

/// IDT vector for the virtio-net line (blk is 0x41, input is 0x42).
pub const VECTOR: u8 = 0x43;

const Q: usize = 16;
/// Bytes the device may write into one receive buffer, header included.
const RX_LEN: usize = 2048;
/// Virtio-net header without `VIRTIO_NET_F_MRG_RXBUF`.
// QEMU's user backend consumes a 12-byte header (the merged-buffer form)
// even when VIRTIO_NET_F_MRG_RXBUF is not negotiated. A 10-byte header
// makes the device eat the first two bytes of the frame.
const HDR_LEN: usize = 12;

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
    ring: [u16; Q],
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
    ring: [VirtqUsedElem; Q],
}

struct Queue {
    desc: [VirtqDesc; Q],
    avail: VirtqAvail,
    used: VirtqUsed,
    last_used: u16,
}

impl Queue {
    const fn empty() -> Self {
        Self {
            desc: [VirtqDesc {
                addr: 0,
                len: 0,
                flags: 0,
                next: 0,
            }; Q],
            avail: VirtqAvail {
                flags: 0,
                idx: 0,
                ring: [0; Q],
            },
            used: VirtqUsed {
                flags: 0,
                idx: 0,
                ring: [VirtqUsedElem { id: 0, len: 0 }; Q],
            },
            last_used: 0,
        }
    }
}

struct Dev {
    modern: virtio_pci::Modern,
    rx_notify: u16,
    tx_notify: u16,
}

static PROBED: AtomicBool = AtomicBool::new(false);
static READY: AtomicBool = AtomicBool::new(false);
static LINK: AtomicBool = AtomicBool::new(false);
static INTX: AtomicBool = AtomicBool::new(false);
static ISR: AtomicU64 = AtomicU64::new(0);
static MAC: Mutex<[u8; 6]> = Mutex::new([0; 6]);
static IRQS: AtomicU64 = AtomicU64::new(0);
static LAST_TX_DESCS: AtomicU16 = AtomicU16::new(0);
static DEV: Mutex<Option<Dev>> = Mutex::new(None);
static RXQ: Mutex<Queue> = Mutex::new(Queue::empty());
static TXQ: Mutex<Queue> = Mutex::new(Queue::empty());

/// One page each so a receive buffer never straddles a frame.
#[repr(C, align(4096))]
struct RxBuf([u8; 4096]);

static RX_BUFS: Mutex<[RxBuf; Q]> = Mutex::new([const { RxBuf([0; 4096]) }; Q]);

#[repr(C, align(16))]
struct TxHdr([u8; 16]);

static TX_HDR: Mutex<TxHdr> = Mutex::new(TxHdr([0; 16]));

/// Probes PCI once. `false` when no virtio-net 1.x device is attached.
pub fn present() -> bool {
    if PROBED.load(Ordering::Acquire) {
        return READY.load(Ordering::Acquire);
    }
    let ok = probe();
    READY.store(ok, Ordering::Release);
    PROBED.store(true, Ordering::Release);
    ok
}

/// MAC the device config reported. Zeros when the device is absent.
pub fn mac() -> [u8; 6] {
    if !present() {
        return [0; 6];
    }
    *MAC.lock()
}

/// Link bit from the device status field.
pub fn link_up() -> bool {
    if !present() {
        return false;
    }
    refresh_link();
    LINK.load(Ordering::Acquire)
}

/// Transmit completions plus receive interrupts observed so far.
pub fn irq_count() -> u64 {
    IRQS.load(Ordering::Acquire)
}

/// Data descriptors used by the most recent transmit.
pub fn last_tx_data_descs() -> u16 {
    LAST_TX_DESCS.load(Ordering::Acquire)
}

/// True when a received frame is waiting.
pub fn has_rx() -> bool {
    if !present() {
        return false;
    }
    let q = RXQ.lock();
    used_idx(&q.used) != q.last_used
}

/// Copies one Ethernet frame into `dst`. `Ok(0)` means the queue is empty.
pub fn recv(dst: &mut [u8]) -> Result<usize, SysError> {
    if !present() {
        return Err(SysError::Unsupported);
    }
    let mut q = RXQ.lock();
    let idx = used_idx(&q.used);
    if idx == q.last_used {
        return Ok(0);
    }
    let slot = (q.last_used as usize) % Q;
    // SAFETY: the device published this used element before bumping idx.
    let elem = unsafe { core::ptr::read_volatile(&q.used.ring[slot]) };
    q.last_used = q.last_used.wrapping_add(1);
    let id = elem.id as usize;
    if id >= Q {
        return Err(SysError::BadValue);
    }
    let total = elem.len as usize;
    let n = if total <= HDR_LEN {
        0
    } else {
        let body = total - HDR_LEN;
        let copy = body.min(dst.len()).min(RX_LEN - HDR_LEN);
        let bufs = RX_BUFS.lock();
        dst[..copy].copy_from_slice(&bufs[id].0[HDR_LEN..HDR_LEN + copy]);
        copy
    };
    repost_rx(&mut q, id)?;
    let dev = DEV.lock();
    if let Some(dev) = dev.as_ref() {
        virtio_pci::notify(&dev.modern, 0, dev.rx_notify);
    }
    Ok(n)
}

/// Transmits one Ethernet frame. The device reads `frame`; a buffer that
/// straddles two physical pages becomes two descriptors.
pub fn transmit(frame: &[u8]) -> Result<(), SysError> {
    if frame.is_empty() || frame.len() > 1514 {
        return Err(SysError::BadValue);
    }
    if !present() {
        return Err(SysError::Unsupported);
    }
    let enable_after = x86_64::instructions::interrupts::are_enabled();
    x86_64::instructions::interrupts::disable();
    let result = submit_tx(frame);
    if enable_after {
        x86_64::instructions::interrupts::enable();
    }
    result
}

/// Interrupt handler body. Counts, acknowledges, wakes `net`.
pub fn on_irq() {
    let isr = ISR.load(Ordering::Acquire);
    if isr != 0 {
        let _ = virtio_pci::ack_isr(isr);
    }
    IRQS.fetch_add(1, Ordering::Release);
    crate::sched::wake_net();
}

/// True when completion is an I/O APIC level line.
pub fn intx_routed() -> bool {
    INTX.load(Ordering::Acquire)
}

fn probe() -> bool {
    let Some(pci_dev) = pci::find_any(VIRTIO_VENDOR, &[VIRTIO_NET_MODERN, VIRTIO_NET_TRANS]) else {
        return false;
    };
    pci::enable_bus_master(pci_dev);
    let Some(modern) = virtio_pci::claim(pci_dev) else {
        crate::serial_println!("[virtio-net] not a virtio 1.x function");
        return false;
    };
    if !virtio_pci::negotiate(&modern, F_MAC | F_STATUS) {
        crate::serial_println!("[virtio-net] device refused VIRTIO_F_VERSION_1 or MAC");
        return false;
    }
    let mac = read_mac(&modern);
    if mac == [0; 6] {
        crate::serial_println!("[virtio-net] device config MAC is zero");
        return false;
    }
    let mut rx = RXQ.lock();
    let mut tx = TXQ.lock();
    *rx = Queue::empty();
    *tx = Queue::empty();
    let Ok(rx_desc) = phys_of(&rx.desc) else {
        return false;
    };
    let Ok(rx_avail) = phys_of(&rx.avail) else {
        return false;
    };
    let Ok(rx_used) = phys_of(&rx.used) else {
        return false;
    };
    let Ok(tx_desc) = phys_of(&tx.desc) else {
        return false;
    };
    let Ok(tx_avail) = phys_of(&tx.avail) else {
        return false;
    };
    let Ok(tx_used) = phys_of(&tx.used) else {
        return false;
    };
    let Some(rx_notify) = virtio_pci::setup_queue(&modern, 0, Q as u16, rx_desc, rx_avail, rx_used)
    else {
        crate::serial_println!("[virtio-net] rx queue rejected");
        return false;
    };
    let Some(tx_notify) = virtio_pci::setup_queue(&modern, 1, Q as u16, tx_desc, tx_avail, tx_used)
    else {
        crate::serial_println!("[virtio-net] tx queue rejected");
        return false;
    };
    if post_all_rx(&mut rx).is_err() {
        crate::serial_println!("[virtio-net] rx buffers are not mapped");
        return false;
    }
    drop(rx);
    drop(tx);
    ISR.store(modern.isr_addr(), Ordering::Release);
    let msix = virtio_pci::enable_msix(
        pci_dev,
        &modern,
        0,
        0,
        VECTOR,
        crate::arch::apic::lapic_id(),
    ) && virtio_pci::enable_msix(
        pci_dev,
        &modern,
        1,
        0,
        VECTOR,
        crate::arch::apic::lapic_id(),
    );
    let route = if msix {
        "msi-x"
    } else if wire_intx(pci_dev) {
        "intx"
    } else {
        "no-irq"
    };
    virtio_pci::driver_ok(&modern);
    virtio_pci::notify(&modern, 0, rx_notify);
    *MAC.lock() = mac;
    *DEV.lock() = Some(Dev {
        modern,
        rx_notify,
        tx_notify,
    });
    refresh_link();
    crate::serial_println!(
        "[virtio-net] mac {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x} link {} ({})",
        mac[0],
        mac[1],
        mac[2],
        mac[3],
        mac[4],
        mac[5],
        if LINK.load(Ordering::Acquire) {
            "up"
        } else {
            "down"
        },
        route
    );
    true
}

fn refresh_link() {
    let dev = DEV.lock();
    let Some(dev) = dev.as_ref() else {
        return;
    };
    let base = dev.modern.device_cfg().as_u64();
    // SAFETY: the device config region was mapped at claim.
    let status = unsafe { ((base + 6) as *const u16).read_volatile() };
    LINK.store(status & S_LINK_UP != 0, Ordering::Release);
}

fn read_mac(dev: &virtio_pci::Modern) -> [u8; 6] {
    let base = dev.device_cfg().as_u64();
    let mut mac = [0u8; 6];
    for (i, byte) in mac.iter_mut().enumerate() {
        // SAFETY: device config, byte offset of the MAC field.
        *byte = unsafe { ((base + i as u64) as *const u8).read_volatile() };
    }
    mac
}

fn wire_intx(dev: pci::Device) -> bool {
    let line = pci::interrupt_line(dev);
    if line == 0 || !crate::arch::ioapic::wire_pci_level(u32::from(line), VECTOR, "virtio-net") {
        return false;
    }
    INTX.store(true, Ordering::Release);
    true
}

fn post_all_rx(q: &mut Queue) -> Result<(), SysError> {
    for id in 0..Q {
        repost_rx(q, id)?;
    }
    Ok(())
}

fn repost_rx(q: &mut Queue, id: usize) -> Result<(), SysError> {
    let bufs = RX_BUFS.lock();
    let phys = phys_of(&bufs[id].0[..RX_LEN])?;
    drop(bufs);
    write_desc(q, id, phys, RX_LEN, VRING_DESC_F_WRITE);
    let avail_idx = q.avail.idx;
    let slot = (avail_idx as usize) % Q;
    // SAFETY: the available ring is ours until the index is published.
    unsafe {
        core::ptr::write_volatile(&mut q.avail.ring[slot], id as u16);
    }
    core::sync::atomic::fence(Ordering::SeqCst);
    unsafe {
        core::ptr::write_volatile(&mut q.avail.idx, avail_idx.wrapping_add(1));
    }
    Ok(())
}

fn submit_tx(frame: &[u8]) -> Result<(), SysError> {
    let mut tx = TXQ.lock();
    let hdr_phys = {
        let mut hdr = TX_HDR.lock();
        hdr.0[..HDR_LEN].fill(0);
        phys_of(&hdr.0[..HDR_LEN])?
    };
    write_desc(&mut tx, 0, hdr_phys, HDR_LEN, VRING_DESC_F_NEXT);
    let data_descs = push_runs(&mut tx, frame.as_ptr(), frame.len())?;
    LAST_TX_DESCS.store(data_descs, Ordering::Release);
    let avail_idx = tx.avail.idx;
    let slot = (avail_idx as usize) % Q;
    unsafe {
        core::ptr::write_volatile(&mut tx.avail.ring[slot], 0);
    }
    core::sync::atomic::fence(Ordering::SeqCst);
    let want = tx.last_used.wrapping_add(1);
    unsafe {
        core::ptr::write_volatile(&mut tx.avail.idx, avail_idx.wrapping_add(1));
    }
    core::sync::atomic::fence(Ordering::SeqCst);
    let notify = {
        let dev = DEV.lock();
        let Some(dev) = dev.as_ref() else {
            return Err(SysError::Unsupported);
        };
        virtio_pci::notify(&dev.modern, 1, dev.tx_notify);
        dev.tx_notify
    };
    let _ = notify;
    drop(tx);
    if !wait_tx(want) {
        crate::serial_println!("[virtio-net] transmit timeout");
        return Err(SysError::TimedOut);
    }
    let mut tx = TXQ.lock();
    tx.last_used = want;
    Ok(())
}

fn wait_tx(want: u16) -> bool {
    let start = crate::arch::timer_ticks();
    loop {
        let tx = TXQ.lock();
        if used_idx(&tx.used) == want {
            return true;
        }
        drop(tx);
        if crate::arch::timer_ticks().saturating_sub(start) > 2000 {
            return false;
        }
        x86_64::instructions::interrupts::enable_and_hlt();
        x86_64::instructions::interrupts::disable();
    }
}

/// One descriptor per physically contiguous run of `buf`. Returns how many
/// data descriptors were written. The chain starts at descriptor 1.
fn push_runs(q: &mut Queue, buf: *const u8, len: usize) -> Result<u16, SysError> {
    let mut idx = 1usize;
    let mut run_phys = 0u64;
    let mut run_len = 0usize;
    let mut off = 0usize;
    let mut runs = 0u16;
    while off < len {
        let va = (buf as u64)
            .checked_add(off as u64)
            .ok_or(SysError::BadValue)?;
        let phys = mm::translate(VirtAddr::new(va))
            .ok_or(SysError::Unsupported)?
            .as_u64();
        let page_left = 0x1000 - (va as usize & 0xFFF);
        let chunk = page_left.min(len - off);
        if run_len > 0 && phys == run_phys + run_len as u64 {
            run_len += chunk;
        } else {
            if run_len > 0 {
                if idx + 1 >= Q {
                    return Err(SysError::BadValue);
                }
                let flags = if off < len { VRING_DESC_F_NEXT } else { 0 };
                write_desc(q, idx, run_phys, run_len, flags);
                idx += 1;
                runs += 1;
            }
            run_phys = phys;
            run_len = chunk;
        }
        off += chunk;
    }
    if run_len > 0 {
        if idx >= Q {
            return Err(SysError::BadValue);
        }
        write_desc(q, idx, run_phys, run_len, 0);
        runs += 1;
        // The previous data descriptor must point at this one. The header
        // already points at descriptor 1. Fix NEXT on every data desc but
        // the last: rewrite them with NEXT set, then the last without.
        if runs > 1 {
            // Re-walk is unnecessary: each write except the last used NEXT
            // because `off < len` was true when the previous run was flushed.
            // The last write passes flags 0. Confirm the predecessor links.
            let _ = idx;
        }
    }
    if runs == 0 {
        return Err(SysError::BadValue);
    }
    Ok(runs)
}

fn write_desc(q: &mut Queue, idx: usize, addr: u64, len: usize, flags: u16) {
    let next = if flags & VRING_DESC_F_NEXT != 0 {
        (idx + 1) as u16
    } else {
        0
    };
    // SAFETY: the descriptor is host-owned until the available index moves.
    unsafe {
        core::ptr::write_volatile(
            &mut q.desc[idx],
            VirtqDesc {
                addr,
                len: len as u32,
                flags,
                next,
            },
        );
    }
}

fn used_idx(used: &VirtqUsed) -> u16 {
    // SAFETY: the device owns this halfword.
    unsafe { core::ptr::read_volatile(&used.idx) }
}

fn phys_of<T: ?Sized>(r: &T) -> Result<u64, SysError> {
    let v = VirtAddr::new(core::ptr::from_ref(r) as *const u8 as u64);
    mm::translate(v)
        .map(|p| p.as_u64())
        .ok_or(SysError::Unsupported)
}
