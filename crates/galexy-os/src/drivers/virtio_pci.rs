//! Virtio 1.x over PCI: capability walk, MMIO BARs, feature negotiation,
//! split-queue setup, and MSI-X.
//!
//! A device without the virtio vendor capabilities is not modern; the
//! caller keeps the legacy I/O BAR path.

use x86_64::VirtAddr;

use super::pci;
use crate::arch::mm;

const CAP_VENDOR: u8 = 0x09;
const CAP_MSIX: u8 = 0x11;

const CFG_COMMON: u8 = 1;
const CFG_NOTIFY: u8 = 2;
const CFG_ISR: u8 = 3;
const CFG_DEVICE: u8 = 4;

const STATUS_ACKNOWLEDGE: u8 = 1;
const STATUS_DRIVER: u8 = 2;
const STATUS_DRIVER_OK: u8 = 4;
const STATUS_FEATURES_OK: u8 = 8;

/// `VIRTIO_F_VERSION_1` lives in feature word 1, bit 0 (feature bit 32).
pub const VERSION_1_WORD1: u32 = 1;

const MSIX_ENABLE: u16 = 1 << 15;
const MSIX_MASK: u16 = 1 << 14;
const MSIX_NO_VECTOR: u16 = 0xFFFF;

struct CapLoc {
    bar: u8,
    offset: u32,
    length: u32,
}

struct Msix {
    /// Config-space offset of the MSI-X capability.
    cap: u8,
    table: VirtAddr,
    vectors: u16,
}

/// Mapped virtio 1.x regions for one function.
pub struct Modern {
    common: VirtAddr,
    notify: VirtAddr,
    notify_mul: u32,
    /// Virtual address of the ISR status byte.
    isr: u64,
    device_cfg: VirtAddr,
    msix: Option<Msix>,
}

impl Modern {
    /// Virtual address of the ISR byte (for the interrupt handler).
    pub fn isr_addr(&self) -> u64 {
        self.isr
    }

    /// Device-specific config region.
    pub fn device_cfg(&self) -> VirtAddr {
        self.device_cfg
    }
}

/// Locates virtio 1.x PCI capabilities and maps their BARs.
///
/// Returns `None` when the common / notify / ISR / device regions are
/// not all present. Resets the device status on the way in.
pub fn claim(dev: pci::Device) -> Option<Modern> {
    let common = cap_of(dev, CFG_COMMON)?;
    let notify = cap_of(dev, CFG_NOTIFY)?;
    let isr = cap_of(dev, CFG_ISR)?;
    let device = cap_of(dev, CFG_DEVICE)?;
    let common_v = map_cap(dev, &common)?;
    let notify_v = map_cap(dev, &notify)?;
    let isr_v = map_cap(dev, &isr)?;
    let device_v = map_cap(dev, &device)?;
    let notify_mul = notify_multiplier(dev, CFG_NOTIFY).unwrap_or(0);
    let modern = Modern {
        common: common_v,
        notify: notify_v,
        notify_mul,
        isr: isr_v.as_u64(),
        device_cfg: device_v,
        msix: map_msix(dev),
    };
    reset(&modern);
    Some(modern)
}

/// Acknowledges the device and accepts `want0` (masked to what the device
/// offers) plus `VIRTIO_F_VERSION_1`.
///
/// Fails when the device does not offer version 1 or clears `FEATURES_OK`.
pub fn negotiate(dev: &Modern, want0: u32) -> bool {
    write8(dev.common, 0x14, STATUS_ACKNOWLEDGE);
    write8(dev.common, 0x14, STATUS_ACKNOWLEDGE | STATUS_DRIVER);
    write32(dev.common, 0x00, 0);
    let word0 = read32(dev.common, 0x04) & want0;
    write32(dev.common, 0x00, 1);
    let offered_hi = read32(dev.common, 0x04);
    if offered_hi & VERSION_1_WORD1 == 0 {
        return false;
    }
    write32(dev.common, 0x08, 0);
    write32(dev.common, 0x0C, word0);
    write32(dev.common, 0x08, 1);
    write32(dev.common, 0x0C, VERSION_1_WORD1);
    let status = STATUS_ACKNOWLEDGE | STATUS_DRIVER | STATUS_FEATURES_OK;
    write8(dev.common, 0x14, status);
    read8(dev.common, 0x14) & STATUS_FEATURES_OK != 0
}

/// Maximum split-queue size the device reports for `index` (0 if none).
pub fn max_queue(dev: &Modern, index: u16) -> u16 {
    write16(dev.common, 0x16, index);
    read16(dev.common, 0x18)
}

/// Programs split queue `index` of `size` descriptors.
///
/// `desc`, `driver` (avail), and `device` (used) are guest-physical.
/// Returns the queue's notify offset, or `None` when the device rejects
/// the size.
pub fn setup_queue(
    dev: &Modern,
    index: u16,
    size: u16,
    desc: u64,
    driver: u64,
    device: u64,
) -> Option<u16> {
    write16(dev.common, 0x16, index);
    let max = read16(dev.common, 0x18);
    if max < size || size == 0 || !size.is_power_of_two() {
        return None;
    }
    write16(dev.common, 0x18, size);
    if read16(dev.common, 0x18) != size {
        return None;
    }
    write64(dev.common, 0x20, desc);
    write64(dev.common, 0x28, driver);
    write64(dev.common, 0x30, device);
    write16(dev.common, 0x1C, 1);
    Some(read16(dev.common, 0x1E))
}

/// Writes the queue index at this queue's notify address.
pub fn notify(dev: &Modern, queue: u16, notify_off: u16) {
    let addr =
        dev.notify.as_u64() + u64::from(notify_off).saturating_mul(u64::from(dev.notify_mul));
    // SAFETY: the notify BAR was mapped in [`claim`].
    unsafe { (addr as *mut u16).write_volatile(queue) };
}

/// Reads the ISR status byte, which acknowledges a legacy INTx.
pub fn ack_isr(isr: u64) -> u8 {
    if isr == 0 {
        return 0;
    }
    // SAFETY: `isr` is the mapped ISR capability byte.
    unsafe { (isr as *const u8).read_volatile() }
}

/// Marks the driver ready. The device may interrupt after this returns.
pub fn driver_ok(dev: &Modern) {
    let status = read8(dev.common, 0x14) | STATUS_DRIVER_OK;
    write8(dev.common, 0x14, status);
}

/// Points queue `queue` at MSI-X table entry `entry` delivering `vector`
/// to `dest_apic` in physical mode.
///
/// Returns false when the function has no MSI-X table or the device
/// rejects the vector. INTx stays available in that case.
pub fn enable_msix(
    pci_dev: pci::Device,
    dev: &Modern,
    queue: u16,
    entry: u16,
    vector: u8,
    dest_apic: u8,
) -> bool {
    let Some(msix) = dev.msix.as_ref() else {
        return false;
    };
    if entry >= msix.vectors {
        return false;
    }
    let ctrl = pci::read_u16(pci_dev.bus, pci_dev.slot, pci_dev.func, msix.cap + 2);
    // Function-mask while the table is programmed, then enable.
    pci::write_u16(
        pci_dev.bus,
        pci_dev.slot,
        pci_dev.func,
        msix.cap + 2,
        (ctrl & 0x07FF) | MSIX_ENABLE | MSIX_MASK,
    );
    let slot = msix.table.as_u64() + u64::from(entry) * 16;
    let addr = 0xFEE0_0000u64 | (u64::from(dest_apic) << 12);
    // SAFETY: the MSI-X table BAR was mapped in [`claim`].
    unsafe {
        (slot as *mut u32).write_volatile(addr as u32);
        ((slot + 4) as *mut u32).write_volatile((addr >> 32) as u32);
        ((slot + 8) as *mut u32).write_volatile(u32::from(vector));
        ((slot + 12) as *mut u32).write_volatile(0);
    }
    // No config-change interrupt. Queue `queue` uses table entry `entry`.
    write16(dev.common, 0x10, MSIX_NO_VECTOR);
    write16(dev.common, 0x16, queue);
    write16(dev.common, 0x1A, entry);
    if read16(dev.common, 0x1A) != entry {
        return false;
    }
    // Drop the function mask. PCI INTx is disabled while MSI-X is enabled.
    pci::write_u16(
        pci_dev.bus,
        pci_dev.slot,
        pci_dev.func,
        msix.cap + 2,
        (ctrl & 0x07FF) | MSIX_ENABLE,
    );
    let mut cmd = pci::read_u16(pci_dev.bus, pci_dev.slot, pci_dev.func, 0x04);
    cmd |= 1 << 10;
    pci::write_u16(pci_dev.bus, pci_dev.slot, pci_dev.func, 0x04, cmd);
    true
}

/// Little-endian `u64` at `off` in the device config region.
pub fn read_dev_u64(dev: &Modern, off: u64) -> u64 {
    read64(dev.device_cfg, off)
}

fn reset(dev: &Modern) {
    write8(dev.common, 0x14, 0);
    for _ in 0..10_000 {
        if read8(dev.common, 0x14) == 0 {
            return;
        }
        core::hint::spin_loop();
    }
}

fn cap_of(dev: pci::Device, kind: u8) -> Option<CapLoc> {
    let mut off = (pci::read_u32(dev.bus, dev.slot, dev.func, 0x34) & 0xFF) as u8;
    let mut guard = 0;
    while off >= 0x40 && guard < 48 {
        let header = pci::read_u32(dev.bus, dev.slot, dev.func, off);
        let id = header as u8;
        let next = (header >> 8) as u8;
        let len = (header >> 16) as u8;
        let cfg_type = (header >> 24) as u8;
        if id == CAP_VENDOR && cfg_type == kind && len >= 16 {
            let bar_word = pci::read_u32(dev.bus, dev.slot, dev.func, off + 4);
            let offset = pci::read_u32(dev.bus, dev.slot, dev.func, off + 8);
            let length = pci::read_u32(dev.bus, dev.slot, dev.func, off + 12);
            if length != 0 {
                return Some(CapLoc {
                    bar: bar_word as u8,
                    offset,
                    length,
                });
            }
        }
        if next == 0 || next == off {
            break;
        }
        off = next;
        guard += 1;
    }
    None
}

fn notify_multiplier(dev: pci::Device, kind: u8) -> Option<u32> {
    let mut off = (pci::read_u32(dev.bus, dev.slot, dev.func, 0x34) & 0xFF) as u8;
    let mut guard = 0;
    while off >= 0x40 && guard < 48 {
        let header = pci::read_u32(dev.bus, dev.slot, dev.func, off);
        let id = header as u8;
        let next = (header >> 8) as u8;
        let len = (header >> 16) as u8;
        let cfg_type = (header >> 24) as u8;
        if id == CAP_VENDOR && cfg_type == kind && len >= 20 {
            return Some(pci::read_u32(dev.bus, dev.slot, dev.func, off + 16));
        }
        if next == 0 || next == off {
            break;
        }
        off = next;
        guard += 1;
    }
    None
}

fn map_cap(dev: pci::Device, cap: &CapLoc) -> Option<VirtAddr> {
    let base = pci::mem_bar(dev, cap.bar)?;
    let phys = base.checked_add(u64::from(cap.offset))?;
    Some(mm::map_mmio(phys, cap.length as usize))
}

fn map_msix(dev: pci::Device) -> Option<Msix> {
    let mut off = (pci::read_u32(dev.bus, dev.slot, dev.func, 0x34) & 0xFF) as u8;
    let mut guard = 0;
    while off >= 0x40 && guard < 48 {
        let header = pci::read_u32(dev.bus, dev.slot, dev.func, off);
        let id = header as u8;
        let next = (header >> 8) as u8;
        if id == CAP_MSIX {
            let ctrl = (header >> 16) as u16;
            let vectors = (ctrl & 0x07FF) + 1;
            let table = pci::read_u32(dev.bus, dev.slot, dev.func, off + 4);
            let bir = (table & 0x7) as u8;
            let offset = table & !0x7;
            let base = pci::mem_bar(dev, bir)?;
            let phys = base.checked_add(u64::from(offset))?;
            let bytes = usize::from(vectors).saturating_mul(16);
            let mapped = mm::map_mmio(phys, bytes.max(16));
            return Some(Msix {
                cap: off,
                table: mapped,
                vectors,
            });
        }
        if next == 0 || next == off {
            break;
        }
        off = next;
        guard += 1;
    }
    None
}

fn read8(base: VirtAddr, off: u64) -> u8 {
    // SAFETY: `base` is a mapped virtio MMIO region and `off` is a register.
    unsafe { ((base.as_u64() + off) as *const u8).read_volatile() }
}

fn write8(base: VirtAddr, off: u64, value: u8) {
    // SAFETY: as [`read8`].
    unsafe { ((base.as_u64() + off) as *mut u8).write_volatile(value) };
}

fn read16(base: VirtAddr, off: u64) -> u16 {
    // SAFETY: as [`read8`]. The virtio common cfg registers are aligned.
    unsafe { ((base.as_u64() + off) as *const u16).read_volatile() }
}

fn write16(base: VirtAddr, off: u64, value: u16) {
    // SAFETY: as [`read8`].
    unsafe { ((base.as_u64() + off) as *mut u16).write_volatile(value) };
}

fn read32(base: VirtAddr, off: u64) -> u32 {
    // SAFETY: as [`read8`].
    unsafe { ((base.as_u64() + off) as *const u32).read_volatile() }
}

fn write32(base: VirtAddr, off: u64, value: u32) {
    // SAFETY: as [`read8`].
    unsafe { ((base.as_u64() + off) as *mut u32).write_volatile(value) };
}

fn read64(base: VirtAddr, off: u64) -> u64 {
    let lo = u64::from(read32(base, off));
    let hi = u64::from(read32(base, off + 4));
    lo | (hi << 32)
}

fn write64(base: VirtAddr, off: u64, value: u64) {
    write32(base, off, value as u32);
    write32(base, off + 4, (value >> 32) as u32);
}
