//! Local APIC: xAPIC/x2APIC access layer + bring-up.
//!
//! Enabled from the MADT's published base (see `arch::acpi`). Supports both
//! the legacy MMIO interface (xAPIC) and the MSR interface (x2APIC). x2APIC
//! is turned on when CPUID.1 ECX bit 21 reports it. The timer is
//! TSC-deadline when CPUID.1 ECX bit 24 is set; otherwise a one-shot count.
//! Calibration prefers CPUID 0x15 / 0x16, then the HPET, then the PIT.
//!
//! All register access routes through [`reg_read`]/[`reg_write`]: xAPIC
//! reads/writes the mapped MMIO page; x2APIC converts the MMIO offset to an
//! MSR (`0x800 + offset >> 4`). MSR access is unprefixed and ungate-unsafe,
//! so public entry points take the IRQ gate (lock-audit rule).
//!
//! Vectors live in `pics.rs` (still the single source of vector constants):
//! the LAPIC's spurious vector is 0xFF (unused); the timer vector is wired
//! in a later commit, keyboard delivery goes through the I/O APIC.

use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

use spin::Once;
use x86_64::instructions::interrupts;
use x86_64::instructions::port::Port;
use x86_64::registers::model_specific::Msr;
use x86_64::structures::paging::{Page, PageTableFlags, PhysFrame, Size4KiB};
use x86_64::{PhysAddr, VirtAddr};

use crate::arch::mm;
use crate::serial_println;

/// IA32_APIC_BASE: bit 11 = APIC global enable, bit 10 = x2APIC mode.
const IA32_APIC_BASE: u32 = 0x1B;
/// IA32_TSC_DEADLINE. Written with the absolute TSC at which the timer fires.
const IA32_TSC_DEADLINE: u32 = 0x6E0;
/// x2APIC MSR base: an xAPIC MMIO offset maps to 0x800 + (offset >> 4).
const X2APIC_MSR_BASE: u32 = 0x800;
/// Lapic register offsets (subset we drive).
const REG_ID: u32 = 0x020;
const REG_EOI: u32 = 0x0B0;
const REG_SPURIOUS: u32 = 0x0F0;
const REG_DFR: u32 = 0x0E0;
const REG_LDR: u32 = 0x0D0;
const REG_TPR: u32 = 0x080;
/// Timer LVT: vector + mask (bit 16) + periodic mode (bit 17).
const REG_LVT_TIMER: u32 = 0x320;
/// Divide configuration: bus-divider selection.
const REG_DIV_CONF: u32 = 0x3E0;
/// Timer initial count (periodic mode reloads from here on expiry).
const REG_INITIAL_COUNT: u32 = 0x380;
/// Timer current count (counts down to 0).
const REG_CURRENT_COUNT: u32 = 0x390;
/// ICR: interrupt command (IPI send; xAPIC low/high pair, x2APIC one MSR).
const REG_ICR_LOW: u32 = 0x300;
const REG_ICR_HIGH: u32 = 0x310;

/// Spurious-interrupt vector: must have an IDT entry the moment the LAPIC
/// is enabled (an unhandled stray spurious would triple-fault). No device
/// ever deliberately delivers it in our use.
pub const SPURIOUS_VECTOR: u8 = 0xFF;

/// The detected LAPIC interface.
///
/// Values published once at [`init`]; reads before that panic (a bug).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LapicMode {
    /// Legacy MMIO interface (register page at the MADT base).
    XApic,
    /// MSR interface (registers at MSRs 0x800 + offset>>4).
    X2Apic,
}

struct ApicCtrl {
    mode: LapicMode,
    /// xAPIC register page (None under x2APIC — MSRs replace MMIO).
    page: Option<VirtAddr>,
}

/// The one LAPIC controller; `Once` (no interior mutation after init).
static APIC: Once<ApicCtrl> = Once::new();

/// Reads one LAPIC register by its xAPIC MMIO offset.
///
/// SAFETY contract: callers must hold the IRQ gate while touching timer/live
/// state (the lock-audit rule); reads are atomic enough that the gate is a
/// sequencing concern, not a torn-read one.
fn reg_read(apic: &ApicCtrl, offset: u32) -> u32 {
    match apic.mode {
        LapicMode::XApic => {
            // SAFETY: the register page was mapped at init; offsets are
            // register-aligned by construction.
            unsafe {
                let reg = apic.page.unwrap().as_u64() + offset as u64;
                (reg as *const u32).read_volatile()
            }
        }
        LapicMode::X2Apic => {
            // SAFETY: x2APIC MSRs exist once EFER + APIC_BASE confirm the
            // mode; the offset range is fixed by the architecture.
            (unsafe { Msr::new(X2APIC_MSR_BASE + (offset >> 4)).read() }) as u32
        }
    }
}

/// Writes one LAPIC register by its xAPIC MMIO offset.
fn reg_write(apic: &ApicCtrl, offset: u32, value: u32) {
    match apic.mode {
        LapicMode::XApic => {
            // SAFETY: the register page was mapped at init; offsets are
            // register-aligned by construction.
            unsafe {
                let reg = apic.page.unwrap().as_u64() + offset as u64;
                (reg as *mut u32).write_volatile(value);
            }
        }
        LapicMode::X2Apic => {
            // SAFETY: x2APIC MSRs exist once EFER + APIC_BASE confirm the
            // mode; the offset range is fixed by the architecture.
            unsafe {
                Msr::new(X2APIC_MSR_BASE + (offset >> 4)).write(u64::from(value));
            }
        }
    }
}

/// Reads one register (public seam — handlers use it for EOI/ICR work).
pub fn reg(offset: u32) -> u32 {
    let apic = APIC
        .get()
        .unwrap_or_else(|| panic!("apic: not initialized"));
    reg_read(apic, offset)
}

/// Writes one register (public seam — used by `eoi`, timer setup, tests).
pub fn set_reg(offset: u32, value: u32) {
    let apic = APIC
        .get()
        .unwrap_or_else(|| panic!("apic: not initialized"));
    reg_write(apic, offset, value);
}

/// Signals end-of-interrupt to the LAPIC (the one true EOI now).
pub fn eoi() {
    x86_64::instructions::interrupts::without_interrupts(|| {
        set_reg(REG_EOI, 0);
    });
}

/// The virtual address of the mapped LAPIC register page (xAPIC only).
pub fn lapic_page() -> VirtAddr {
    let apic = APIC
        .get()
        .unwrap_or_else(|| panic!("apic: not initialized"));
    apic.page
        .unwrap_or_else(|| panic!("apic: x2APIC mode has no MMIO page"))
}

/// The detected mode (`Once`: fixed at [`init`]).
pub fn mode() -> LapicMode {
    APIC.get()
        .unwrap_or_else(|| panic!("apic: not initialized"))
        .mode
}

/// The timer's delivery vector (single source of truth: `pics::`'s constant).
pub fn timer_interrupt_id() -> u8 {
    crate::arch::pics::TIMER_INTERRUPT_ID
}

/// Reads the LAPIC ID (xAPIC: bits 24..31; x2APIC: the whole register).
pub fn lapic_id() -> u8 {
    let raw = reg(REG_ID);
    let id = match mode() {
        LapicMode::XApic => raw >> 24,
        LapicMode::X2Apic => raw,
    };
    assert!(
        id <= 0xFF,
        "apic: id {id:#x} does not fit an 8-bit destination"
    );
    id as u8
}

/// Ends an interrupt if interrupts are on (convenience for handler paths).
/// No-op under an already-off gate (naked handler paths are gated already).
pub fn init(lapic_base: u64) {
    bring_up(lapic_base);
    // Calibrate against the PIT once (ratio math only — no wall-clock
    // assumptions, TCG safe; interrupts are off inside `arch::init`). The
    // timer is ARMED after `cpu::boot_aps()` — with the final online count
    // so the BSP's share reflects every CPU.
    init_timer();
}

/// Enables x2APIC when CPUID reports it and the LAPIC is still in xAPIC mode.
fn prefer_x2apic() {
    let (_eax, _ebx, ecx, _edx) = crate::arch::cpu::cpuid(1, 0);
    // CPUID.1:ECX[21] — x2APIC.
    if ecx & (1 << 21) == 0 {
        // QEMU TCG through 8.2 drops this bit even with `-cpu max,+x2apic`
        // ("TCG doesn't support requested feature"). Stay on xAPIC MMIO.
        static PRINTED: AtomicBool = AtomicBool::new(false);
        if PRINTED
            .compare_exchange(false, true, Ordering::Relaxed, Ordering::Relaxed)
            .is_ok()
        {
            serial_println!("[apic] x2apic not enumerated; xAPIC MMIO");
        }
        return;
    }
    let mut apic_base = Msr::new(IA32_APIC_BASE);
    // SAFETY: IA32_APIC_BASE exists on every 64-bit x86 CPU with an APIC.
    let value = unsafe { apic_base.read() };
    if value & (1 << 10) == 0 {
        let enabled = value | (1 << 11);
        // SAFETY: the SDM requires EN=1 before EXTD=1. Two writes, not one.
        unsafe {
            apic_base.write(enabled);
            apic_base.write(enabled | (1 << 10));
        }
    }
    static PRINTED: AtomicBool = AtomicBool::new(false);
    if PRINTED
        .compare_exchange(false, true, Ordering::Relaxed, Ordering::Relaxed)
        .is_ok()
    {
        serial_println!("[apic] x2apic enabled (IA32_APIC_BASE.EXTD, msr path)");
    }
}

/// LAPIC bring-up WITHOUT the timer: mode detection, page mapping, spurious
/// vector, TPR. Each CPU calls this at its own bring-up (BSP
/// [`init`] = this + calibration); the LAPIC MMIO base is hardware-
/// redirected per CPU (every address 0xFEE00000 write from this CPU lands
/// in THIS CPU's own LAPIC), so the shared mapped page serves every core.
pub fn bring_up(lapic_base: u64) {
    prefer_x2apic();
    // Detection FIRST: MSR 0x1B bit 11 must be set, bit 10 chooses x2APIC.
    let apic_base = Msr::new(IA32_APIC_BASE);
    // SAFETY: IA32_APIC_BASE exists on every 64-bit x86 CPU; EN is usually
    // set but bit 10 is what decides the register interface.
    let base_value = unsafe { apic_base.read() };
    assert!(
        base_value & (1 << 11) != 0,
        "apic: the local APIC is not globally enabled (IA32_APIC_BASE.EN = 0)"
    );
    let detected = if base_value & (1 << 10) != 0 {
        LapicMode::X2Apic
    } else {
        LapicMode::XApic
    };

    // Mode/page state is machine-global (same for every CPU: same MMIO
    // base, same mode decision read per CPU — coherent by architecture);
    // the APIC static caches the MODE + the shared page once.
    let page = if detected == LapicMode::XApic {
        Some(map_lapic_page(lapic_base))
    } else {
        // x2APIC replaces MMIO with MSRs; the MADT base is informational.
        None
    };
    let ctrl = ApicCtrl {
        mode: detected,
        page,
    };
    // First caller wins (the BSP normally); an AP racing a fresh detection
    // of an ALREADY-armed global is fine — mode/page are machine-wide facts.
    if APIC.get().is_none() {
        APIC.call_once(|| ctrl);
    } else {
        let cached = APIC.get().unwrap();
        assert_eq!(cached.mode, detected, "apic: mode disagreed between CPUs");
    }

    // Enable it: spurious vector with bit 8 (APIC enable via the spurious
    // register), TPR 0 (accept everything), flat destination mode.
    set_reg(REG_SPURIOUS, SPURIOUS_VECTOR as u32 | 0x100);
    set_reg(REG_TPR, 0); // TPR = 0
    if detected == LapicMode::XApic {
        // DFR/LDR are xAPIC-only registers; on x2APIC this MSR range is
        // reserved (writing it would #GP) — skip on the MSR interface.
        set_reg(REG_DFR, 0xFFFF_FFFF); // flat mode
        set_reg(REG_LDR, 0);
    }
    // LVT entries stay MASKED at enable: this CPU delivers nothing until
    // the scheduler arms its own timer (per-CPU cadence, commit follow-up).
    set_reg(REG_LVT_TIMER, MASKED_BIT);

    serial_println!(
        "[apic] lapic up (mode {:?}, id {}, spurious {:#x})",
        detected,
        lapic_id(),
        reg(REG_SPURIOUS)
    );
}

/// Maps the LAPIC register page (MMIO, PRESENT|RW|NX, not from the
/// allocator — device memory).
fn map_lapic_page(base: u64) -> VirtAddr {
    let phys = PhysAddr::new(base);
    assert_eq!(
        phys.as_u64() & 0xFFF,
        0,
        "apic: LAPIC base must be page-aligned"
    );
    // A fresh P4 entry in the KERNEL half would collide with task trees by
    // index only — but the kernel half is SHARED across all trees, so any
    // kernel-half entry is uniformly visible. Use a low fixed P4 entry
    // unused by anything else: 200 (user regions live below 256 top-down,
    // dynamics below 100; 200 is free real estate).
    const LAPIC_P4: u16 = 200;
    // A fixed virtual address under that P4 entry (canonical lower-half
    // sign-extension happens via the address math below).
    let virt = VirtAddr::new(u64::from(LAPIC_P4) << 39);
    let page = Page::<Size4KiB>::containing_address(virt);
    let frame = PhysFrame::from_start_address(phys).expect("apic: LAPIC base is not frame-aligned");
    let flags = PageTableFlags::PRESENT
        | PageTableFlags::WRITABLE
        | PageTableFlags::NO_EXECUTE
        | PageTableFlags::NO_CACHE;
    match mm::map_page_flags(page, frame, flags) {
        Ok(()) => {}
        Err(crate::arch::mm::paging::PageError::AlreadyMapped) => {
            // Every CPU shares the kernel half, so the BSP's mapping serves
            // all APs (xAPIC MMIO is hardware-redirected per CPU). Verify it
            // targets the same physical page, then keep that virtual seat.
            let found = mm::translate(virt);
            assert_eq!(
                found.map(|p| p.as_u64() & !0xFFF),
                Some(phys.as_u64() & !0xFFF),
                "apic: an already-mapped LAPIC page points elsewhere"
            );
        }
        Err(e) => panic!("apic: failed to map the LAPIC page: {e:?}"),
    }
    serial_println!("[apic] lapic page {:#x} -> {:#x}", virt.as_u64(), base);
    virt
}

/* ---------------- LAPIC timer ---------------- */

/// Calibrated timer rate in ticks per millisecond.
static TICKS_PER_MS: Once<u32> = Once::new();
/// TSC ticks per millisecond, for TSC-deadline and `spin_ms`.
static TSC_PER_MS: AtomicU64 = AtomicU64::new(0);
/// True when the LAPIC timer LVT is in TSC-deadline mode.
static USE_DEADLINE: AtomicBool = AtomicBool::new(false);

/// Per-CPU armed one-shot duration (ms). The IRQ path consumes this so
/// `timer_ticks` advances by the deadline that actually fired (tickless).
static ARMED_MS: [AtomicU32; crate::arch::cpu::MAX_CPUS] =
    [const { AtomicU32::new(0) }; crate::arch::cpu::MAX_CPUS];

/// Longest idle sleep (status-bar / uptime second boundary).
pub const IDLE_MAX_MS: u32 = 1000;

/// IPI that only wakes a CPU out of `hlt`. The handler EOIs and returns;
/// the idle loop notices runnable work and arms a quantum.
pub const WAKE_VECTOR: u8 = 0xF7;

/// The calibrated LAPIC-timer rate (ticks per millisecond at divide-by-1).
/// Panics before calibration (a bug, not a condition to handle).
pub fn ticks_per_ms() -> u32 {
    TICKS_PER_MS
        .get()
        .copied()
        .unwrap_or_else(|| panic!("apic: timer not calibrated (init runs first)"))
}

/// Preempt quantum in milliseconds (SMP share-split: N CPUs → N ms each).
pub fn quantum_ms() -> u32 {
    (crate::arch::cpu::online() as u32).max(1)
}

/// Calibrates the timer. Arming is one-shot or TSC-deadline
/// ([`arm_timer`] / [`arm_oneshot_ms`]) — not a 1 kHz periodic metronome.
///
/// Interrupts must be OFF. CPUID 0x15 / 0x16 supply the frequency when
/// they enumerate one that agrees (within 2×) with a measured HPET or PIT
/// window. TCG often advertises a crystal the emulated timer does not run
/// at; the measured window wins in that case so deadlines stay honest.
/// TSC-deadline is used when CPUID.1 ECX bit 24 is set. `IDLE_MAX_MS` and
/// the quantum are not touched here.
fn init_timer() {
    if TICKS_PER_MS.get().is_some() {
        panic!("apic: timer calibrated twice");
    }
    let (_eax, _ebx, ecx, _edx) = crate::arch::cpu::cpuid(1, 0);
    let deadline = ecx & (1 << 24) != 0;
    let (meas_tsc, meas_lapic, meas_name) = measure_reference();
    let (tsc_per_ms, lapic_per_ms, source) = match cpuid_rates() {
        Some((name, tsc_hz, lapic_hz)) => {
            let nom_tsc = tsc_hz / 1000;
            let nom_lapic = lapic_hz.map(|hz| u32::try_from(hz / 1000).unwrap_or(u32::MAX).max(1));
            let tsc_ok = within_2x(nom_tsc, meas_tsc);
            let lapic_ok =
                nom_lapic.is_some_and(|n| within_2x(u64::from(n), u64::from(meas_lapic)));
            if tsc_ok && (deadline || lapic_ok) {
                let lapic = if lapic_ok {
                    nom_lapic.unwrap()
                } else {
                    meas_lapic
                };
                (nom_tsc.max(1), lapic, name)
            } else {
                serial_println!(
                        "[apic] cpuid {} disagrees with {} (nominal {}/ms measured {}/ms); using measured",
                        name,
                        meas_name,
                        nom_tsc,
                        meas_tsc
                    );
                (meas_tsc, meas_lapic, meas_name)
            }
        }
        None => (meas_tsc, meas_lapic, meas_name),
    };
    TSC_PER_MS.store(tsc_per_ms, Ordering::Relaxed);
    USE_DEADLINE.store(deadline, Ordering::Relaxed);
    let published = if deadline {
        u32::try_from(tsc_per_ms).unwrap_or(u32::MAX).max(1)
    } else {
        lapic_per_ms.max(1)
    };
    TICKS_PER_MS.call_once(|| published);
    if deadline {
        // SAFETY: TSC-deadline is enumerated; 0 disarms until `arm_timer`.
        unsafe { Msr::new(IA32_TSC_DEADLINE).write(0) };
        set_reg(REG_LVT_TIMER, MASKED_BIT | LVT_TSC_DEADLINE);
        serial_println!(
            "[apic] timer tsc-deadline source={} {} ticks/ms",
            source,
            published
        );
    } else {
        set_reg(REG_INITIAL_COUNT, 0);
        serial_println!(
            "[apic] timer oneshot source={} {} ticks/ms",
            source,
            published
        );
    }
}

/// Busy-waits `ms` milliseconds on the TSC. False before calibration.
pub fn spin_ms(ms: u32) -> bool {
    let per = TSC_PER_MS.load(Ordering::Relaxed);
    if per == 0 {
        return false;
    }
    let start = rdtsc();
    let need = per.saturating_mul(u64::from(ms));
    while rdtsc().wrapping_sub(start) < need {
        core::hint::spin_loop();
    }
    true
}

fn rdtsc() -> u64 {
    // SAFETY: rdtsc has no side effects.
    unsafe { core::arch::x86_64::_rdtsc() }
}

fn within_2x(a: u64, b: u64) -> bool {
    if a == 0 || b == 0 {
        return false;
    }
    let (lo, hi) = if a < b { (a, b) } else { (b, a) };
    hi / lo <= 2
}

/// `(name, tsc_hz, lapic_hz)` from CPUID 0x15, else 0x16.
fn cpuid_rates() -> Option<(&'static str, u64, Option<u64>)> {
    let (eax, ebx, ecx, _edx) = crate::arch::cpu::cpuid(0x15, 0);
    if eax != 0 && ebx != 0 && ecx != 0 {
        let tsc = u64::from(ecx).saturating_mul(u64::from(ebx)) / u64::from(eax);
        if tsc != 0 {
            return Some(("cpuid-15", tsc, Some(u64::from(ecx))));
        }
    }
    let (eax, _ebx, ecx, _edx) = crate::arch::cpu::cpuid(0x16, 0);
    if eax != 0 {
        let tsc = u64::from(eax).saturating_mul(1_000_000);
        let lapic = (ecx != 0).then_some(u64::from(ecx).saturating_mul(1_000_000));
        return Some(("cpuid-16", tsc, lapic));
    }
    None
}

fn measure_reference() -> (u64, u32, &'static str) {
    if let Some(base) = crate::arch::acpi::hpet_base() {
        if let Some((tsc, lapic)) = measure_hpet(base) {
            return (tsc, lapic, "hpet");
        }
    }
    let (tsc, lapic) = measure_pit();
    (tsc, lapic, "pit")
}

fn measure_hpet(phys: u64) -> Option<(u64, u32)> {
    const CAL_MS: u64 = 10;
    let base = crate::arch::mm::map_mmio(phys, 4096);
    let caps = mmio_read64(base, 0);
    let period_fs = caps >> 32;
    if period_fs == 0 {
        return None;
    }
    let ticks = (CAL_MS * 1_000_000_000_000) / period_fs;
    if ticks == 0 {
        return None;
    }
    let cfg = mmio_read64(base, 0x10);
    // Bit 0 enables the main counter. Bit 1 (legacy replacement) stays off.
    mmio_write64(base, 0x10, cfg | 1);
    set_reg(REG_LVT_TIMER, MASKED_BIT);
    set_reg(REG_DIV_CONF, DIV_1);
    set_reg(REG_INITIAL_COUNT, u32::MAX);
    let t0 = rdtsc();
    let h0 = mmio_read64(base, 0xF0);
    while mmio_read64(base, 0xF0).wrapping_sub(h0) < ticks {
        core::hint::spin_loop();
    }
    let elapsed = u32::MAX.wrapping_sub(reg(REG_CURRENT_COUNT));
    let t1 = rdtsc();
    set_reg(REG_INITIAL_COUNT, 0);
    let tsc_per = t1.wrapping_sub(t0) / CAL_MS;
    if tsc_per == 0 {
        return None;
    }
    Some((tsc_per, (elapsed / CAL_MS as u32).max(1)))
}

fn mmio_read64(base: x86_64::VirtAddr, off: u64) -> u64 {
    // SAFETY: `base` is a mapped HPET page; the counter and config are aligned.
    unsafe { ((base.as_u64() + off) as *const u64).read_volatile() }
}

fn mmio_write64(base: x86_64::VirtAddr, off: u64, value: u64) {
    // SAFETY: as [`mmio_read64`].
    unsafe { ((base.as_u64() + off) as *mut u64).write_volatile(value) };
}

fn measure_pit() -> (u64, u32) {
    const CAL_MS: u32 = 10;
    const PIT_FREQ: u32 = 1_193_182;
    const PIT_COUNTS: u32 = PIT_FREQ / 1000 * CAL_MS;
    set_reg(REG_LVT_TIMER, MASKED_BIT);
    set_reg(REG_DIV_CONF, DIV_1);
    pit_oneshot_10ms(PIT_COUNTS);
    let t0 = rdtsc();
    set_reg(REG_INITIAL_COUNT, u32::MAX);
    while !pit_drained() {}
    let elapsed = u32::MAX.wrapping_sub(reg(REG_CURRENT_COUNT));
    let t1 = rdtsc();
    set_reg(REG_INITIAL_COUNT, 0);
    let tsc_per = t1.wrapping_sub(t0) / u64::from(CAL_MS);
    ((tsc_per.max(1)), (elapsed / CAL_MS).max(1))
}

/// Arms THIS CPU's first deadline (preempt quantum). Called by the BSP
/// AFTER `boot_aps()` and by each AP after LAPIC bring-up.
pub fn arm_timer() {
    arm_oneshot_ms(quantum_ms());
    serial_println!(
        "[apic] timer armed (oneshot, quantum {} ms, cpu {})",
        quantum_ms(),
        crate::arch::cpu::current_index()
    );
}

/// Program a one-shot LAPIC timer for `ms` milliseconds on this CPU.
///
/// Clears periodic mode. IRQ-gated (lock-audit). Clamped to
/// `1..=IDLE_MAX_MS`.
pub fn arm_oneshot_ms(ms: u32) {
    let ms = ms.clamp(1, IDLE_MAX_MS);
    interrupts::without_interrupts(|| {
        let cpu = crate::arch::cpu::current_index();
        ARMED_MS[cpu].store(ms, Ordering::Relaxed);
        if USE_DEADLINE.load(Ordering::Relaxed) {
            let per = TSC_PER_MS.load(Ordering::Relaxed).max(1);
            let delta = per.saturating_mul(u64::from(ms)).max(1);
            set_reg(REG_LVT_TIMER, u32::from(TIMER_VECTOR) | LVT_TSC_DEADLINE);
            let now = rdtsc();
            // SAFETY: TSC-deadline was enumerated at calibration.
            unsafe { Msr::new(IA32_TSC_DEADLINE).write(now.wrapping_add(delta)) };
        } else {
            let icr = ticks_per_ms().saturating_mul(ms).max(1);
            set_reg(REG_DIV_CONF, DIV_1);
            // One-shot: LVT timer bits 18:17 clear.
            set_reg(REG_LVT_TIMER, u32::from(TIMER_VECTOR));
            set_reg(REG_INITIAL_COUNT, icr);
        }
    });
}

/// Idle deadline: ms until the next whole second of `timer_ticks`, or
/// [`IDLE_MAX_MS`]. Keeps status-bar / uptime wakes without a 1 kHz poll.
pub fn idle_deadline_ms() -> u32 {
    let into = (crate::arch::timer_ticks() % 1000) as u32;
    (IDLE_MAX_MS - into).max(1)
}

/// The one-shot currently programmed on `cpu`, in milliseconds (0 if the
/// IRQ already consumed it). The rotation watchdog prints this.
pub fn armed_ms(cpu: usize) -> u32 {
    if cpu >= ARMED_MS.len() {
        return 0;
    }
    ARMED_MS[cpu].load(Ordering::Relaxed)
}

/// Consumes the armed duration for this CPU (called from the timer IRQ).
/// Returns at least 1 if the arming record was lost.
pub fn take_armed_ms() -> u64 {
    let cpu = crate::arch::cpu::current_index();
    let ms = ARMED_MS[cpu].swap(0, Ordering::Relaxed);
    u64::from(ms.max(1))
}

/// Program PIT channel 2 for a one-shot of `counts` (speaker OFF, gate ON).
fn pit_oneshot_10ms(counts: u32) {
    // SAFETY: fixed PIT ports; channel 2 is unused by anything else.
    unsafe {
        let mut cmd = Port::<u8>::new(0x43);
        let mut ch2 = Port::<u8>::new(0x42);
        let mut gate = Port::<u8>::new(0x61);
        // Channel 2, lo/hi access, mode 0 (interrupt on terminal count).
        cmd.write(0xB0);
        ch2.write((counts & 0xFF) as u8);
        ch2.write((counts >> 8) as u8);
        // Gate ON (bit 0), speaker OFF (bit 1): starts the count.
        let v = gate.read();
        gate.write((v & !0b10) | 0b1);
    }
}

/// True once the PIT channel-2 one-shot has drained (OUT2 = port 0x61 bit 5
/// is back to 0 after being set by the count).
fn pit_drained() -> bool {
    // SAFETY: fixed status port (0x61 read side carries OUT2 at bit 5).
    // Mode 0 raises OUT2 when the one-shot drains — HIGH = done.
    unsafe {
        let mut gate = Port::<u8>::new(0x61);
        gate.read() & 0x20 != 0
    }
}

/// Timer vector — file-level alias for the single-number source of truth
/// (`pics::TIMER_INTERRUPT_ID`, still 32): the naked handler, tick path and
/// scheduler quantum all keep their meaning across the delivery swap.
const TIMER_VECTOR: u8 = crate::arch::pics::TIMER_INTERRUPT_ID;

/// LVT timer flags: masked-off (bit 16). Periodic mode (bit 17) is unused.
/// Bit 18 selects TSC-deadline when CPUID reports it.
const MASKED_BIT: u32 = 1 << 16;
const LVT_TSC_DEADLINE: u32 = 1 << 18;
/// Divide-by-1 (divide-configuration register encoding).
const DIV_1: u32 = 0b1011;

/* ---------------- IPIs (inter-processor interrupts) ---------------- */

/// ICR delivery-mode encodings.
const ICR_DELIVERY_INIT: u32 = 0b101;
const ICR_DELIVERY_STARTUP: u32 = 0b110;
/// ICR delivery mode 0b000 = FIXED: the vector is dispatched as an ordinary
/// interrupt on the target CPU (the IPI machinery's workhorse — shootdowns).
const ICR_DELIVERY_FIXED: u32 = 0b000;
/// ICR level bit (bit 14): 1 = assert (for INIT).
const ICR_LEVEL_ASSERT: u32 = 1 << 14;
/// ICR destination-mode: physical (bit 11 = 0) — per-APIC-id targeting.
const ICR_DEST_PHYSICAL: u32 = 0;

/// Sends an IPI to `apic_id` with the delivery mode + payload `vector`.
///
/// xAPIC ordering contract: the HIGH register (destination) is written
/// FIRST, the LOW register (command) LAST — the low write is what dispatches.
/// x2APIC: one wide MSR write (dest in bits 32..63) with that same
/// effective ordering (low bits last).
///
/// Per-CPU by hardware: the WRITER's own LAPIC dispatches the IPI (all
/// IPIs are sent by the BSP in our flow; ordering writes make that safe).
pub fn send_ipi(apic_id: u8, delivery: u32, vector: u8, level: bool) {
    let apic = APIC
        .get()
        .unwrap_or_else(|| panic!("apic: not initialized"));
    let level_bit = if level { ICR_LEVEL_ASSERT } else { 0 };
    let low = (u32::from(vector) & 0xFF) | (delivery << 8) | ICR_DEST_PHYSICAL | level_bit;
    match apic.mode {
        LapicMode::XApic => {
            // HIGH first (destination), LOW last (dispatches the IPI).
            reg_write(apic, REG_ICR_HIGH, u32::from(apic_id) << 24);
            reg_write(apic, REG_ICR_LOW, low);
        }
        LapicMode::X2Apic => {
            // One wide MSR: bits 0..31 command, 32..63 destination. The
            // write is atomic — the single write dispatches.
            let wide = (u64::from(apic_id) << 32) | u64::from(low);
            // SAFETY: x2APIC ICR MSR (0x830 = base + 0x300>>4); mode and
            // feature verified at bring-up.
            unsafe {
                Msr::new(X2APIC_MSR_BASE + (REG_ICR_LOW >> 4)).write(wide);
            }
        }
    }
}

/// Sends a FIXED-delivery IPI (an ordinary interrupt on `vector`) to
/// `apic_id`. Runtime consumers (TLB shootdowns) send these from any CPU —
/// unlike INIT/SIPI bring-up, nothing here is BSP-only.
pub fn send_fixed_ipi(apic_id: u8, vector: u8) {
    send_ipi(apic_id, ICR_DELIVERY_FIXED, vector, false);
}

/// Sends the INIT IPI (assert) to `apic_id`.
pub fn send_init(apic_id: u8) {
    send_ipi(apic_id, ICR_DELIVERY_INIT, 0, true);
}

/// Sends the SIPI (start-up) IPI to `apic_id` at 4-KiB-page `page_phys`.
pub fn send_sipi(apic_id: u8, page_phys: u64) {
    debug_assert_eq!(page_phys & 0xFFF, 0, "SIPI vector must be 4 KiB aligned");
    debug_assert!(page_phys < 0x10_0000, "SIPI page must live under 1 MiB");
    send_ipi(
        apic_id,
        ICR_DELIVERY_STARTUP,
        (page_phys >> 12) as u8,
        false,
    );
}
