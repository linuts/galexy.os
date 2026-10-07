//! Minimal PCI configuration-space access for device discovery.
//!
//! Only what virtio-blk needs: enumerate bus 0, read BARs / IDs, and
//! enable IO+memory+bus-master on a function. No MSI, bridges, or hotplug.

use x86_64::instructions::port::Port;

const CONFIG_ADDR: u16 = 0xCF8;
const CONFIG_DATA: u16 = 0xCFC;

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

/// Reads a 32-bit config dword at `offset` (must be 4-byte aligned).
pub fn read_u32(bus: u8, slot: u8, func: u8, offset: u8) -> u32 {
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

/// BAR0 as an I/O port base, if the BAR is IO-mapped.
pub fn io_bar0(dev: Device) -> Option<u16> {
    let bar = read_u32(dev.bus, dev.slot, dev.func, 0x10);
    if bar & 1 == 0 {
        return None;
    }
    Some((bar & !0x3) as u16)
}

/// Scans bus 0 for the first function matching `vendor`/`device`.
pub fn find(vendor: u16, device: u16) -> Option<Device> {
    for slot in 0..32u8 {
        for func in 0..8u8 {
            let id = read_u32(0, slot, func, 0);
            if id == 0xFFFF_FFFF {
                if func == 0 {
                    break;
                }
                continue;
            }
            let vend = id as u16;
            let dev = (id >> 16) as u16;
            if vend == vendor && dev == device {
                return Some(Device {
                    bus: 0,
                    slot,
                    func,
                    vendor: vend,
                    device: dev,
                });
            }
            // Single-function device: skip other funcs.
            if func == 0 {
                let header = read_u32(0, slot, 0, 0x0C);
                if (header >> 16) as u8 & 0x80 == 0 {
                    break;
                }
            }
        }
    }
    None
}
