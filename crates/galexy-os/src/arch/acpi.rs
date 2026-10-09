//! ACPI discovery: RSDP → XSDT/RSDT → MADT, plus the FADT power registers.
//!
//! `BootInfo` hands us the physical RSDP address; the physical-memory mapping
//! (fixed in `BOOTLOADER_CONFIG`) lets us walk the tables without any extra
//! page mappings — including `arch::init` running BEFORE `mm::init` in some
//! test kernels (the phys map exists from boot).
//!
//! Everything read here is checksum-validated: firmware tables are trusted
//! only after their checksums check out. Invalid or missing data is a loud
//! panic (serial-reported), not a silent default — for the interrupt-routing
//! code downstream there is no sane answer to guess.
//!
//! Published (consumed by `arch/apic.rs` + `arch/ioapic.rs`):
//! - Local APIC MMIO base (MADT field at +36, possibly type-5 overridden)
//! - I/O APIC MMIO base + its GSI base (the IO APIC covering GSI 0)
//! - ISA Interrupt Source Overrides (ISA IRQ → GSI)

use spin::Once;
use x86_64::VirtAddr;

use crate::serial_println;

/// ISA lines whose GSI wiring can differ from `IRQ == GSI` (16 inputs).
const ISA_LINES: usize = 16;
/// Sanity cap for validated table lengths (firmware tables are small).
const TABLE_MAX: usize = 1 << 20;

/// What the MADT says about the interrupt controllers.
///
/// Values are copied out of the tables: the phys map is virtual forever, so
/// keeping only numbers (no table references) can never dangle.
#[derive(Debug, Clone, Copy)]
pub struct Madt {
    /// Local APIC MMIO base (usually `0xFEE00000`).
    lapic_base: u64,
    /// MMIO base of the boot I/O APIC (the one whose GSI range covers 0).
    ioapic_base: u64,
    /// First GSI the boot I/O APIC delivers.
    ioapic_gsi_base: u32,
    /// APIC ID of the first enabled processor (the BSP).
    boot_cpu_apic_id: u8,
    /// Number of enabled Local APIC records.
    cpus: u8,
    /// APIC IDs of the enabled processors, in MADT order (the first is the
    /// BSP — the SMP bring-up consumes this).
    enabled_ids: [u8; 8],
    /// `overrides[irq]` = the GSI this ISA line is wired to, when the wiring
    /// differs from `IRQ == GSI`.
    overrides: [Option<u32>; ISA_LINES],
}

impl Madt {
    /// Local APIC MMIO base.
    pub fn lapic_base(&self) -> u64 {
        self.lapic_base
    }

    /// MMIO base of the boot I/O APIC.
    pub fn ioapic_base(&self) -> u64 {
        self.ioapic_base
    }

    /// First GSI the boot I/O APIC delivers.
    pub fn ioapic_gsi_base(&self) -> u32 {
        self.ioapic_gsi_base
    }

    /// APIC ID of the BSP (first enabled processor).
    pub fn boot_cpu_apic_id(&self) -> u8 {
        self.boot_cpu_apic_id
    }

    /// Number of enabled processors in the MADT.
    pub fn cpus(&self) -> u8 {
        self.cpus
    }

    /// APIC IDs of the enabled processors, in MADT order (index 0 = BSP).
    /// The slice is exactly `cpus()` long.
    pub fn enabled_ids(&self) -> &[u8] {
        &self.enabled_ids[..usize::from(self.cpus)]
    }

    /// The GSI an ISA interrupt line is wired to (override or identity).
    pub fn isa_gsi(&self, irq: u8) -> u32 {
        match self.overrides.get(usize::from(irq)) {
            Some(Some(gsi)) => *gsi,
            _ => u32::from(irq),
        }
    }
}

/// Parsed MADT; panics before [`init`] (a bug, not a condition to handle).
static MADT: Once<Madt> = Once::new();

/// Shutdown and reset registers from the FADT (and `_S5_` in the DSDT).
///
/// Absent when the firmware has no FADT. Shutdown then has nothing to
/// program; reboot can still pulse the keyboard controller.
#[derive(Debug, Clone, Copy)]
pub struct PowerInfo {
    /// PM1a control register, I/O port.
    pub pm1a_cnt: u16,
    /// PM1b control register, or 0 when the platform has only PM1a.
    pub pm1b_cnt: u16,
    /// SLP_TYPa from the DSDT `_S5_` package (low 3 bits).
    pub slp_typa: u8,
    /// SLP_TYPb. Unused when `pm1b_cnt` is 0.
    pub slp_typb: u8,
    /// `_S5_` was present, so [`slp_typa`] is meaningful.
    pub has_s5: bool,
    /// SMI command port. 0 means ACPI is already enabled.
    pub smi_cmd: u16,
    /// Byte written to [`smi_cmd`] to enter ACPI mode.
    pub acpi_enable: u8,
    /// ACPI reset register, I/O port, when the FADT advertises one.
    pub reset_port: u16,
    /// Value that resets the machine through [`reset_port`].
    pub reset_value: u8,
    /// [`reset_port`] is valid.
    pub has_reset: bool,
}

/// Parsed FADT power block. `None` before init, or when the firmware has no FADT.
static POWER: Once<Option<PowerInfo>> = Once::new();

/// One PCI Express ECAM window (segment 0) from the ACPI `MCFG` table.
#[derive(Debug, Clone, Copy)]
pub struct McfgWindow {
    /// Physical base of the window for [`start_bus`].
    pub base: u64,
    /// First bus number this window covers.
    pub start_bus: u8,
    /// Last bus number this window covers, inclusive.
    pub end_bus: u8,
}

/// `MCFG` window, or `None` when the firmware has no segment-0 ECAM.
static MCFG: Once<Option<McfgWindow>> = Once::new();

/// HPET register-block physical address, or `None` when the table is absent.
static HPET_BASE: Once<Option<u64>> = Once::new();

/// FADT `IAPC_BOOT_ARCH`, or `None` when the table is too short to carry it.
///
/// Bit 0 set means a legacy 8259 pair is present.
static BOOT_ARCH: Once<Option<u16>> = Once::new();

/// The parsed interrupt-controller table. Call after [`init`].
pub fn madt() -> &'static Madt {
    MADT.get()
        .unwrap_or_else(|| panic!("acpi: MADT not initialized (arch::init runs first)"))
}

/// FADT shutdown/reset registers, if the firmware published a FADT.
pub fn power_info() -> Option<&'static PowerInfo> {
    POWER.get().and_then(|slot| slot.as_ref())
}

/// PCIe ECAM window from `MCFG`, if the firmware published one for segment 0.
pub fn mcfg() -> Option<&'static McfgWindow> {
    MCFG.get().and_then(|slot| slot.as_ref())
}

/// HPET MMIO base, if the firmware published an `HPET` table in system memory.
pub fn hpet_base() -> Option<u64> {
    HPET_BASE.get().copied().flatten()
}

/// True when the FADT boot-arch flags report an 8259, or when the flags
/// are missing (legacy machines are assumed to have one).
pub fn has_8259() -> bool {
    match BOOT_ARCH.get().copied().flatten() {
        Some(flags) => flags & 1 != 0,
        None => true,
    }
}

/// Walks RSDP → XSDT/RSDT → MADT and publishes the result.
///
/// `rsdp_phys` is the physical RSDP address (from `BootInfo.rsdp_addr`);
/// `phys_offset` maps the whole physical address space virtually.
pub fn init(rsdp_phys: Option<u64>, phys_offset: u64) {
    assert!(
        phys_offset != 0,
        "acpi: physical memory offset not mapped (see BOOTLOADER_CONFIG)"
    );
    let Some(rsdp_phys) = rsdp_phys else {
        panic!("acpi: the bootloader handed us no RSDP address");
    };

    let rsdp = load_rsdp(rsdp_phys, phys_offset);
    let (root_phys, entry_size, root_sig) = match rsdp.xsdt_phys {
        // XSDT (v2+): 8-byte child entries. Supersedes the RSDT entirely.
        Some(xsdt) => (xsdt, 8, b"XSDT"),
        None => (rsdp.rsdt_phys, 4, b"RSDT"),
    };
    let madt_phys = find_table(root_phys, phys_offset, entry_size, root_sig, b"APIC")
        .unwrap_or_else(|| panic!("acpi: no MADT (signature 'APIC') under the root table"));

    let madt = parse_madt(madt_phys, phys_offset);
    serial_println!(
        "[acpi] madt ready: lapic {:#x}, ioapic {:#x} (gsi {}), {} cpu(s), bsp apic id {}",
        madt.lapic_base,
        madt.ioapic_base,
        madt.ioapic_gsi_base,
        madt.cpus,
        madt.boot_cpu_apic_id
    );
    if MADT.get().is_some() {
        panic!("acpi: MADT parsed twice");
    }
    MADT.call_once(|| madt);

    let fadt_phys = find_table(root_phys, phys_offset, entry_size, root_sig, b"FACP");
    let power = fadt_phys.and_then(|fadt| parse_power(fadt, phys_offset));
    let boot_arch = fadt_phys.and_then(|fadt| parse_boot_arch(fadt, phys_offset));
    if boot_arch.is_none() {
        serial_println!("[acpi] boot-arch: 8259=assumed (no IAPC_BOOT_ARCH)");
    }
    let mcfg = find_table(root_phys, phys_offset, entry_size, root_sig, b"MCFG")
        .and_then(|phys| parse_mcfg(phys, phys_offset));
    if let Some(window) = mcfg {
        serial_println!(
            "[acpi] mcfg {:#x} buses {}-{}",
            window.base,
            window.start_bus,
            window.end_bus
        );
    } else {
        serial_println!("[acpi] no MCFG");
    }
    let hpet = find_table(root_phys, phys_offset, entry_size, root_sig, b"HPET")
        .and_then(|phys| parse_hpet(phys, phys_offset));
    if let Some(base) = hpet {
        serial_println!("[acpi] hpet {:#x}", base);
    } else {
        serial_println!("[acpi] no HPET");
    }
    if let Some(info) = power {
        serial_println!(
            "[acpi] power: pm1a {:#x}, pm1b {:#x}, s5 {}, reset {:#x}",
            info.pm1a_cnt,
            info.pm1b_cnt,
            if info.has_s5 {
                info.slp_typa as u16
            } else {
                0xFFFF
            },
            if info.has_reset { info.reset_port } else { 0 }
        );
    } else {
        serial_println!("[acpi] no usable FADT; shutdown unavailable");
    }
    if POWER.get().is_some() {
        panic!("acpi: power info parsed twice");
    }
    POWER.call_once(|| power);
    BOOT_ARCH.call_once(|| boot_arch);
    MCFG.call_once(|| mcfg);
    HPET_BASE.call_once(|| hpet);
}

struct Rsdp {
    rsdt_phys: u64,
    xsdt_phys: Option<u64>,
}

/// Reads `n` little-endian bytes at `at` inside `buf` (caller bounds-checks;
/// slice indexing panics loudly otherwise).
fn le(buf: &[u8], at: usize, n: usize) -> u64 {
    (0..n)
        .rev()
        .fold(0u64, |acc, i| (acc << 8) | u64::from(buf[at + i]))
}

/// Checksum: the byte sum mod 256 must be 0.
fn checksum_ok(bytes: &[u8]) -> bool {
    bytes.iter().fold(0u8, |a, b| a.wrapping_add(*b)) == 0
}

/// Loads + validates the RSDP through the physical-memory mapping.
fn load_rsdp(phys: u64, phys_offset: u64) -> Rsdp {
    let base = VirtAddr::new(phys_offset + phys).as_ptr::<u8>();
    // SAFETY: the phys map covers every physical address; the RSDP is at
    // most 36 bytes (v1 is 20), validated below before it is trusted.
    let bytes = unsafe { core::slice::from_raw_parts(base, 36) };
    assert_eq!(&bytes[0..8], b"RSD PTR ", "acpi: RSDP signature mismatch");
    assert!(
        checksum_ok(&bytes[..20]),
        "acpi: RSDP (v1) checksum mismatch"
    );
    let revision = bytes[15];
    let rsdt_phys = le(bytes, 16, 4);
    let xsdt_phys = if revision >= 2 {
        let len = le(bytes, 20, 4) as usize;
        assert_eq!(
            len, 36,
            "acpi: RSDP v2 must be exactly 36 bytes (got {len})"
        );
        assert!(
            checksum_ok(&bytes[..36]),
            "acpi: RSDP v2 extended checksum mismatch"
        );
        Some(le(bytes, 24, 8))
    } else {
        None
    };
    Rsdp {
        rsdt_phys,
        xsdt_phys,
    }
}

/// Maps `phys` as an ACPI table image: reads the length from the header,
/// validates it, and returns the full (checksummable) byte slice.
///
/// # Safety
///
/// `phys` must head a real ACPI table in physical memory covered by the
/// phys map; the header's length field must describe contiguous bytes.
unsafe fn table_bytes(phys: u64, phys_offset: u64) -> &'static [u8] {
    let base = VirtAddr::new(phys_offset + phys).as_ptr::<u8>();
    // SAFETY: the phys map covers every physical address; the fixed 36-byte
    // header is always present.
    let header = unsafe { core::slice::from_raw_parts(base, 36) };
    let len = le(header, 4, 4) as usize;
    assert!(
        (36..=TABLE_MAX).contains(&len),
        "acpi: unreasonable table length {len} at phys {:#x}",
        phys
    );
    // SAFETY: the length was just validated; ACPI tables are laid out
    // contiguously at [phys, phys+len).
    unsafe { core::slice::from_raw_parts(base, len) }
}

/// Returns the physical address of the first child table with `sig`
/// (`entry_size`: 8 for XSDT, 4 for RSDT).
fn find_table(
    root_phys: u64,
    phys_offset: u64,
    entry_size: usize,
    root_sig: &[u8; 4],
    sig: &[u8; 4],
) -> Option<u64> {
    // SAFETY: the root-table address comes from the validated RSDP.
    let root = unsafe { table_bytes(root_phys, phys_offset) };
    assert_eq!(&root[0..4], root_sig, "acpi: root table signature mismatch");
    assert!(
        checksum_ok(root),
        "acpi: root table checksum mismatch ({:?})",
        root_sig
    );

    (36..root.len()).step_by(entry_size).find_map(|off| {
        let child = le(root, off, entry_size);
        if child == 0 {
            return None;
        }
        // SAFETY: child-table addresses come from the validated root.
        let bytes = unsafe { table_bytes(child, phys_offset) };
        assert!(
            checksum_ok(bytes),
            "acpi: child table checksum mismatch at phys {:#x}",
            child
        );
        (&bytes[0..4] == sig).then_some(child)
    })
}

fn parse_madt(phys: u64, phys_offset: u64) -> Madt {
    // SAFETY: found via the validated root table.
    let bytes = unsafe { table_bytes(phys, phys_offset) };
    assert_eq!(&bytes[0..4], b"APIC", "acpi: MADT signature mismatch");
    assert!(bytes.len() >= 44, "acpi: MADT too short for APIC fields");

    let mut lapic_base = le(bytes, 36, 4);
    let mut boot_cpu: Option<u8> = None;
    let mut cpus = 0u8;
    let mut enabled_ids = [0u8; 8];
    let mut ioapic: Option<(u64, u32)> = None;
    let mut overrides = [None; ISA_LINES];

    let mut off = 44;
    while off < bytes.len() {
        let kind = bytes[off];
        let len = usize::from(bytes[off + 1]);
        assert!(
            len >= 2 && off + len <= bytes.len(),
            "acpi: MADT record of type {kind} is malformed or beyond the table end"
        );
        match kind {
            0 => {
                // Local APIC: processor id, apic id, flags (bit0 = enabled).
                assert!(len >= 8, "acpi: MADT local-APIC record too short");
                let id = bytes[off + 3];
                let flags = le(bytes, off + 4, 4);
                if flags & 1 != 0 {
                    if boot_cpu.is_none() {
                        boot_cpu = Some(id);
                    }
                    if usize::from(cpus) < 8 {
                        enabled_ids[usize::from(cpus)] = id;
                    }
                    cpus += 1;
                }
            }
            1 => {
                // I/O APIC: id, reserved, MMIO address, GSI base.
                assert!(len >= 12, "acpi: MADT io-apic record too short");
                let base = le(bytes, off + 4, 4);
                let gsi = le(bytes, off + 8, 4) as u32;
                ioapic = match ioapic {
                    // Keep the first IO APIC covering GSI 0 — that is the one
                    // the ISA lines route through. Prefer it over a later
                    // record with a nonzero GSI base.
                    Some((b, g)) if g == 0 => Some((b, g)),
                    Some(other) if gsi != 0 => Some(other),
                    _ => Some((base, gsi)),
                };
            }
            2 => {
                // Interrupt Source Override: bus, source, GSI, flags.
                assert!(len >= 10, "acpi: MADT override record too short");
                let bus = bytes[off + 2];
                let source = bytes[off + 3];
                let gsi = le(bytes, off + 4, 4) as u32;
                if bus == 0 && usize::from(source) < ISA_LINES {
                    overrides[usize::from(source)] = Some(gsi);
                }
            }
            5 => {
                // Local APIC Address Override: 64-bit LAPIC MMIO base.
                assert!(len >= 12, "acpi: MADT lapic-override record too short");
                lapic_base = le(bytes, off + 4, 8);
            }
            _ => {} // NMI sources (4), x2APIC entries (0xA/0xB), ... not needed
        }
        off += len;
    }

    let boot_cpu_apic_id =
        boot_cpu.unwrap_or_else(|| panic!("acpi: MADT lists no enabled processor"));
    let (ioapic_base, ioapic_gsi_base) =
        ioapic.unwrap_or_else(|| panic!("acpi: no I/O APIC in the MADT"));
    assert!(lapic_base != 0, "acpi: LAPIC base is 0");

    Madt {
        lapic_base,
        ioapic_base,
        ioapic_gsi_base,
        boot_cpu_apic_id,
        cpus,
        enabled_ids,
        overrides,
    }
}

/// `IAPC_BOOT_ARCH` at FADT offset 109. `None` when the table predates it.
fn parse_boot_arch(fadt_phys: u64, phys_offset: u64) -> Option<u16> {
    // SAFETY: the address came from the validated root table.
    let fadt = unsafe { table_bytes(fadt_phys, phys_offset) };
    if &fadt[0..4] != b"FACP" || fadt.len() < 111 {
        return None;
    }
    let flags = le(fadt, 109, 2) as u16;
    serial_println!(
        "[acpi] boot-arch: 8259={} 8042={}",
        if flags & 1 != 0 { "yes" } else { "no" },
        if flags & 2 != 0 { "yes" } else { "no" }
    );
    Some(flags)
}

/// First segment-0 allocation in an `MCFG` table.
fn parse_mcfg(phys: u64, phys_offset: u64) -> Option<McfgWindow> {
    // SAFETY: the address came from the validated root table.
    let bytes = unsafe { table_bytes(phys, phys_offset) };
    if &bytes[0..4] != b"MCFG" || bytes.len() < 44 + 16 {
        return None;
    }
    let mut off = 44;
    while off + 16 <= bytes.len() {
        let base = le(bytes, off, 8);
        let segment = le(bytes, off + 8, 2);
        let start_bus = bytes[off + 10];
        let end_bus = bytes[off + 11];
        if segment == 0 && base != 0 && start_bus <= end_bus {
            return Some(McfgWindow {
                base,
                start_bus,
                end_bus,
            });
        }
        off += 16;
    }
    None
}

/// HPET base from the GAS at offset 40, when it is system memory.
fn parse_hpet(phys: u64, phys_offset: u64) -> Option<u64> {
    // SAFETY: the address came from the validated root table.
    let bytes = unsafe { table_bytes(phys, phys_offset) };
    if &bytes[0..4] != b"HPET" || bytes.len() < 52 {
        return None;
    }
    // Address space 0 = system memory. Anything else is not an MMIO block.
    if bytes[40] != 0 {
        return None;
    }
    let base = le(bytes, 44, 8);
    (base != 0).then_some(base)
}

/// Reads the FADT's power and reset registers, and `_S5_` out of its DSDT.
fn parse_power(fadt_phys: u64, phys_offset: u64) -> Option<PowerInfo> {
    // SAFETY: the address came from the validated root table.
    let fadt = unsafe { table_bytes(fadt_phys, phys_offset) };
    if &fadt[0..4] != b"FACP" || fadt.len() < 72 {
        serial_println!("[acpi] FADT too short for PM1a_CNT");
        return None;
    }
    let smi_cmd = le(fadt, 48, 4);
    let acpi_enable = fadt[52];
    let pm1a = le(fadt, 64, 4);
    let pm1b = le(fadt, 68, 4);
    // Extended addresses supersede the 32-bit ones when the table carries them.
    let pm1a = gas_io(fadt, 172).unwrap_or(pm1a);
    let pm1b = gas_io(fadt, 184).unwrap_or(pm1b);
    let (reset_port, reset_value, has_reset) = reset_reg(fadt);

    let dsdt_phys = x_dsdt(fadt).or_else(|| {
        let addr = le(fadt, 40, 4);
        (addr != 0).then_some(addr)
    });
    let (slp_typa, slp_typb, has_s5) = dsdt_phys
        .and_then(|phys| {
            // SAFETY: the DSDT pointer came from the checksummed FADT.
            let dsdt = unsafe { table_bytes(phys, phys_offset) };
            if &dsdt[0..4] != b"DSDT" || !checksum_ok(dsdt) {
                serial_println!("[acpi] DSDT signature or checksum mismatch");
                return None;
            }
            parse_s5(dsdt)
        })
        .map(|(a, b)| (a, b, true))
        .unwrap_or((0, 0, false));

    let pm1a_cnt = u16::try_from(pm1a).ok()?;
    if pm1a_cnt == 0 {
        return None;
    }
    let pm1b_cnt = u16::try_from(pm1b).unwrap_or(0);
    Some(PowerInfo {
        pm1a_cnt,
        pm1b_cnt,
        slp_typa,
        slp_typb,
        has_s5,
        smi_cmd: u16::try_from(smi_cmd).unwrap_or(0),
        acpi_enable,
        reset_port,
        reset_value,
        has_reset,
    })
}

/// I/O port from a Generic Address Structure, when it is System I/O and fits
/// in a port number. `off` is the GAS start inside the FADT.
fn gas_io(table: &[u8], off: usize) -> Option<u64> {
    if table.len() < off + 12 {
        return None;
    }
    // 1 = System I/O. Anything else (memory, PCI) is not a port write.
    if table[off] != 1 {
        return None;
    }
    let addr = le(table, off + 4, 8);
    (addr != 0 && addr <= u64::from(u16::MAX)).then_some(addr)
}

/// 64-bit DSDT pointer (FADT offset 140) when the table is long enough.
fn x_dsdt(fadt: &[u8]) -> Option<u64> {
    if fadt.len() < 148 {
        return None;
    }
    let addr = le(fadt, 140, 8);
    (addr != 0).then_some(addr)
}

/// ACPI reset register (FADT flag bit 10, GAS at 116, value at 128).
fn reset_reg(fadt: &[u8]) -> (u16, u8, bool) {
    if fadt.len() < 129 {
        return (0, 0, false);
    }
    let flags = le(fadt, 112, 4);
    if flags & (1 << 10) == 0 {
        return (0, 0, false);
    }
    match gas_io(fadt, 116) {
        Some(port) => (port as u16, fadt[128], true),
        None => (0, 0, false),
    }
}

/// `_S5_` in a DSDT: `Name(_S5_, Package { typ_a, typ_b, ... })`.
///
/// This is not an AML interpreter. It only accepts the NameOp form QEMU and
/// the firmware actually emit for the sleep-state package.
fn parse_s5(dsdt: &[u8]) -> Option<(u8, u8)> {
    let mut i = 0;
    while i + 5 < dsdt.len() {
        if &dsdt[i..i + 4] == b"_S5_" {
            let named = (i >= 1 && dsdt[i - 1] == 0x08)
                || (i >= 2 && dsdt[i - 2] == 0x08 && dsdt[i - 1] == 0x5C);
            if named && dsdt[i + 4] == 0x12 {
                if let Some(types) = s5_package(&dsdt[i + 5..]) {
                    return Some(types);
                }
            }
        }
        i += 1;
    }
    None
}

/// Package body after PackageOp: PkgLength, element count, then integers.
fn s5_package(body: &[u8]) -> Option<(u8, u8)> {
    let (pkg_len, len_bytes) = pkg_length(body)?;
    if pkg_len < len_bytes || pkg_len > body.len() {
        return None;
    }
    let mut at = len_bytes;
    let count = *body.get(at)? as usize;
    at += 1;
    if count == 0 {
        return None;
    }
    let (a, n) = aml_int(body, at)?;
    at += n;
    let b = if count >= 2 {
        aml_int(body, at).map(|(v, _)| v).unwrap_or(a)
    } else {
        a
    };
    Some(((a & 7) as u8, (b & 7) as u8))
}

/// ACPI PkgLength. The returned length includes the length bytes themselves.
fn pkg_length(body: &[u8]) -> Option<(usize, usize)> {
    let lead = *body.first()?;
    let follow = (lead >> 6) as usize;
    if follow == 0 {
        return Some((usize::from(lead & 0x3F), 1));
    }
    if body.len() < 1 + follow {
        return None;
    }
    let mut len = usize::from(lead & 0x0F);
    for (k, byte) in body[1..1 + follow].iter().enumerate() {
        len |= usize::from(*byte) << (4 + 8 * k);
    }
    Some((len, 1 + follow))
}

/// One AML integer. Returns the value and how many bytes it occupied.
fn aml_int(body: &[u8], at: usize) -> Option<(u64, usize)> {
    match *body.get(at)? {
        0x00 => Some((0, 1)),
        0x01 => Some((1, 1)),
        0x0A => Some((u64::from(*body.get(at + 1)?), 2)),
        0x0B if at + 3 <= body.len() => Some((le(body, at + 1, 2), 3)),
        0x0C if at + 5 <= body.len() => Some((le(body, at + 1, 4), 5)),
        _ => None,
    }
}
