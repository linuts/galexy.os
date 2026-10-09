//! PCI configuration-space access.
//!
//! When ACPI published an `MCFG` window, config cycles go through ECAM
//! MMIO. Ports `0xCF8` / `0xCFC` are used only when that table is absent
//! (the `pc` machine). Enumeration walks PCI-PCI bridges so a virtio
//! function behind a q35 root port is visible.

use core::sync::atomic::{AtomicU64, Ordering};

use crate::sync::Mutex;
use spin::Once;
use x86_64::instructions::port::Port;

use crate::arch::acpi;
use crate::arch::mm;

const CONFIG_ADDR: u16 = 0xCF8;
const CONFIG_DATA: u16 = 0xCFC;

enum Cfg {
    /// Legacy port pair.
    Port,
    /// ECAM window. `base` addresses `start_bus`.
    Ecam { base: u64, start: u8, end: u8 },
}

static CFG: Once<Cfg> = Once::new();

struct EcamPage {
    phys: u64,
    virt: u64,
}

static ECAM_CACHE: Mutex<[Option<EcamPage>; 128]> = Mutex::new([const { None }; 128]);
static ECAM_MAPPED: AtomicU64 = AtomicU64::new(0);

/// One PCI function found by scan.
#[derive(Clone, Copy, Debug)]
pub struct Device {
    /// Bus number (0…255).
    pub bus: u8,
    /// Device/slot (0…31).
    pub slot: u8,
    /// Function (0…7).
    pub func: u8,
    /// Vendor ID.
    pub vendor: u16,
    /// Device ID.
    pub device: u16,
}

fn cfg() -> &'static Cfg {
    CFG.call_once(|| match acpi::mcfg() {
        Some(window) => {
            crate::serial_println!(
                "[pci] ecam {:#x} buses {}-{}",
                window.base,
                window.start_bus,
                window.end_bus
            );
            Cfg::Ecam {
                base: window.base,
                start: window.start_bus,
                end: window.end_bus,
            }
        }
        None => {
            crate::serial_println!("[pci] config via 0xCF8 (no MCFG)");
            Cfg::Port
        }
    })
}

fn ecam_addr(base: u64, start: u8, bus: u8, slot: u8, func: u8, offset: u8) -> Option<u64> {
    if bus < start {
        return None;
    }
    Some(
        base + (u64::from(bus - start) << 20)
            + (u64::from(slot) << 15)
            + (u64::from(func) << 12)
            + u64::from(offset & !3),
    )
}

fn ecam_virt(phys: u64) -> u64 {
    let page = phys & !0xFFF;
    let mut cache = ECAM_CACHE.lock();
    for slot in cache.iter().flatten() {
        if slot.phys == page {
            return slot.virt + (phys & 0xFFF);
        }
    }
    let mapped = mm::map_mmio(page, 4096);
    let virt_page = mapped.as_u64() & !0xFFF;
    let n = ECAM_MAPPED.fetch_add(1, Ordering::Relaxed) as usize;
    if n < cache.len() {
        cache[n] = Some(EcamPage {
            phys: page,
            virt: virt_page,
        });
    }
    virt_page + (phys & 0xFFF)
}

/// Reads a 32-bit config dword at `offset` (must be 4-byte aligned).
pub fn read_u32(bus: u8, slot: u8, func: u8, offset: u8) -> u32 {
    match *cfg() {
        Cfg::Port => port_read(bus, slot, func, offset),
        Cfg::Ecam { base, start, end } => {
            if bus < start || bus > end {
                return 0xFFFF_FFFF;
            }
            let Some(phys) = ecam_addr(base, start, bus, slot, func, offset) else {
                return 0xFFFF_FFFF;
            };
            let virt = ecam_virt(phys);
            // SAFETY: `virt` is the mapped ECAM dword for this function.
            unsafe { (virt as *const u32).read_volatile() }
        }
    }
}

fn port_read(bus: u8, slot: u8, func: u8, offset: u8) -> u32 {
    let addr = 0x8000_0000u32
        | (u32::from(bus) << 16)
        | (u32::from(slot) << 11)
        | (u32::from(func) << 8)
        | (u32::from(offset) & 0xFC);
    // SAFETY: standard PCI config ports; offset aligned.
    unsafe {
        Port::<u32>::new(CONFIG_ADDR).write(addr);
        Port::<u32>::new(CONFIG_DATA).read()
    }
}

/// Writes a 32-bit config dword at `offset` (must be 4-byte aligned).
pub fn write_u32(bus: u8, slot: u8, func: u8, offset: u8, value: u32) {
    match *cfg() {
        Cfg::Port => port_write(bus, slot, func, offset, value),
        Cfg::Ecam { base, start, end } => {
            if bus < start || bus > end {
                return;
            }
            let Some(phys) = ecam_addr(base, start, bus, slot, func, offset) else {
                return;
            };
            let virt = ecam_virt(phys);
            // SAFETY: `virt` is the mapped ECAM dword for this function.
            unsafe { (virt as *mut u32).write_volatile(value) };
        }
    }
}

fn port_write(bus: u8, slot: u8, func: u8, offset: u8, value: u32) {
    let addr = 0x8000_0000u32
        | (u32::from(bus) << 16)
        | (u32::from(slot) << 11)
        | (u32::from(func) << 8)
        | (u32::from(offset) & 0xFC);
    // SAFETY: standard PCI config ports; offset aligned.
    unsafe {
        Port::<u32>::new(CONFIG_ADDR).write(addr);
        Port::<u32>::new(CONFIG_DATA).write(value);
    }
}

/// Reads a 16-bit config word.
pub fn read_u16(bus: u8, slot: u8, func: u8, offset: u8) -> u16 {
    let dword = read_u32(bus, slot, func, offset & !3);
    if offset & 2 != 0 {
        (dword >> 16) as u16
    } else {
        dword as u16
    }
}

/// Writes a 16-bit config word (read-modify-write of the dword).
pub fn write_u16(bus: u8, slot: u8, func: u8, offset: u8, value: u16) {
    let aligned = offset & !3;
    let mut dword = read_u32(bus, slot, func, aligned);
    if offset & 2 != 0 {
        dword = (dword & 0x0000_FFFF) | (u32::from(value) << 16);
    } else {
        dword = (dword & 0xFFFF_0000) | u32::from(value);
    }
    write_u32(bus, slot, func, aligned, dword);
}

/// Enables IO space, memory space, and bus mastering on `dev`.
pub fn enable_bus_master(dev: Device) {
    let mut cmd = read_u16(dev.bus, dev.slot, dev.func, 0x04);
    cmd |= 0x0007; // IO | MEM | BUS_MASTER
    write_u16(dev.bus, dev.slot, dev.func, 0x04, cmd);
}

/// Interrupt line the firmware wrote at config offset `0x3C` (`0` if none).
pub fn interrupt_line(dev: Device) -> u8 {
    read_u32(dev.bus, dev.slot, dev.func, 0x3C) as u8
}

/// BAR0 as an I/O port base, if the BAR is IO-mapped.
pub fn io_bar0(dev: Device) -> Option<u16> {
    let bar = read_u32(dev.bus, dev.slot, dev.func, 0x10);
    if bar & 1 == 0 {
        return None;
    }
    Some((bar & !0x3) as u16)
}

/// Memory BAR physical base, or `None` when the BAR is I/O or empty.
///
/// A 64-bit BAR consumes the following slot. `index` is 0…5.
pub fn mem_bar(dev: Device, index: u8) -> Option<u64> {
    if index > 5 {
        return None;
    }
    let off = 0x10 + index * 4;
    let lo = read_u32(dev.bus, dev.slot, dev.func, off);
    if lo & 1 != 0 {
        return None;
    }
    let kind = (lo >> 1) & 0x3;
    let base = if kind == 0x2 {
        if index >= 5 {
            return None;
        }
        let hi = read_u32(dev.bus, dev.slot, dev.func, off + 4);
        (u64::from(hi) << 32) | u64::from(lo & 0xFFFF_FFF0)
    } else {
        u64::from(lo & 0xFFFF_FFF0)
    };
    (base != 0).then_some(base)
}

/// Scans for the first function matching `vendor`/`device`, including
/// functions behind PCI-PCI bridges.
pub fn find(vendor: u16, device: u16) -> Option<Device> {
    find_any(vendor, &[device])
}

/// Like [`find`], but matches any device id in `devices`.
pub fn find_any(vendor: u16, devices: &[u16]) -> Option<Device> {
    let mut seen = [false; 256];
    find_bus(0, vendor, devices, &mut seen, 0)
}

fn find_bus(
    bus: u8,
    vendor: u16,
    devices: &[u16],
    seen: &mut [bool; 256],
    depth: u8,
) -> Option<Device> {
    if depth > 8 || seen[usize::from(bus)] {
        return None;
    }
    seen[usize::from(bus)] = true;
    for slot in 0..32u8 {
        let id0 = read_u32(bus, slot, 0, 0);
        if id0 == 0xFFFF_FFFF {
            continue;
        }
        let header0 = read_u32(bus, slot, 0, 0x0C);
        let multi = (header0 >> 16) as u8 & 0x80 != 0;
        let funcs = if multi { 8 } else { 1 };
        for func in 0..funcs {
            let id = if func == 0 {
                id0
            } else {
                read_u32(bus, slot, func, 0)
            };
            if id == 0xFFFF_FFFF {
                continue;
            }
            let vend = id as u16;
            let dev_id = (id >> 16) as u16;
            if vend == vendor && devices.contains(&dev_id) {
                return Some(Device {
                    bus,
                    slot,
                    func,
                    vendor: vend,
                    device: dev_id,
                });
            }
            let header = if func == 0 {
                header0
            } else {
                read_u32(bus, slot, func, 0x0C)
            };
            if (header >> 16) as u8 & 0x7F == 0x01 {
                let secondary = (read_u32(bus, slot, func, 0x18) >> 8) as u8;
                if secondary != 0 && secondary != bus {
                    if let Some(found) = find_bus(secondary, vendor, devices, seen, depth + 1) {
                        return Some(found);
                    }
                }
            }
        }
    }
    None
}
