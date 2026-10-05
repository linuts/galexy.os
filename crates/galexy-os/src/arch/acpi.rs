//! ACPI discovery: RSDP → XSDT/RSDT → MADT.
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

/// The parsed interrupt-controller table. Call after [`init`].
pub fn madt() -> &'static Madt {
    MADT.get()
        .unwrap_or_else(|| panic!("acpi: MADT not initialized (arch::init runs first)"))
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
    let madt_phys = match rsdp.xsdt_phys {
        // XSDT (v2+): 8-byte child entries. Supersedes the RSDT entirely.
        Some(xsdt) => find_madt(xsdt, phys_offset, 8, b"XSDT"),
        None => find_madt(rsdp.rsdt_phys, phys_offset, 4, b"RSDT"),
    }
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

/// Returns the physical address of the first `APIC` table under the root
/// system-description table (`entry_size`: 8 for XSDT, 4 for RSDT).
fn find_madt(
    root_phys: u64,
    phys_offset: u64,
    entry_size: usize,
    root_sig: &[u8; 4],
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
        (&bytes[0..4] == b"APIC").then_some(child)
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
