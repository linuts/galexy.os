//! Tick accounting. The clock is the LAPIC (TSC-deadline or one-shot).
//!
//! [`delay_ms`] busy-waits on the TSC after calibration. The PIT channel 2
//! one-shot remains only as the calibration fallback inside `arch::apic`
//! when CPUID 0x15 / 0x16 and the HPET are both absent, and as the PC
//! speaker. Channel 0 is not programmed.

use core::sync::atomic::{AtomicU64, Ordering};
use x86_64::instructions::port::Port;

/// Number of timer ticks since boot.
static TICKS: AtomicU64 = AtomicU64::new(0);

/// PIT crystal frequency in Hz.
const PIT_FREQ: u32 = 1_193_182;

/// Nothing to program. The LAPIC timer is armed from `arch::apic`.
pub fn init() {}

/// Advances the monotonic tick counter by `n` milliseconds of machine time.
///
/// Under tickless idle a single LAPIC one-shot may cover many ms; the IRQ
/// path reports the armed duration here so uptime / `stats` stay honest.
pub fn tick_by(n: u64) {
    if n > 0 {
        TICKS.fetch_add(n, Ordering::Relaxed);
    }
}

/// One millisecond of machine time (legacy name for the IRQ path).
pub fn tick() {
    tick_by(1);
}

/// Number of timer ticks since boot (monotonic, 1 tick ≈ 1 ms).
pub fn ticks() -> u64 {
    TICKS.load(Ordering::Relaxed)
}

/// Busy-waits `ms` milliseconds.
///
/// Uses the TSC rate from LAPIC calibration. The PIT channel-2 fallback
/// runs only when that rate is not published yet. BSP-only for the PIT
/// path: those ports are global.
pub fn delay_ms(ms: u32) {
    if crate::arch::apic::spin_ms(ms) {
        return;
    }
    pit_delay_ms(ms);
}

fn pit_delay_ms(ms: u32) {
    const PIT_COMMAND_PORT: u16 = 0x43;
    const PIT_CHANNEL_2_DATA_PORT: u16 = 0x42;
    const PIT_GATE_PORT: u16 = 0x61;
    // Channel 2, lo/hi access, mode 0 (one-shot; OUT2 raises on expiry).
    const CH2_ONESHOT_CMD: u8 = 0xB0;
    const OUT2_MASK: u8 = 0x20;
    const GATE_BIT: u8 = 0x1;
    const SPEAKER_BIT: u8 = 0x2;

    let counts = (PIT_FREQ / 1000) as u16; // divisor for exactly 1 ms
                                           // SAFETY: fixed PIT + i8042 gate ports; the channel-2 one-shot dance is
                                           // the same pattern the LAPIC calibration uses.
    unsafe {
        let mut cmd = Port::<u8>::new(PIT_COMMAND_PORT);
        let mut ch2 = Port::<u8>::new(PIT_CHANNEL_2_DATA_PORT);
        let mut gate = Port::<u8>::new(PIT_GATE_PORT);
        for _ in 0..ms {
            cmd.write(CH2_ONESHOT_CMD);
            let base = gate.read() & !(SPEAKER_BIT | GATE_BIT);
            ch2.write((counts & 0xFF) as u8);
            ch2.write((counts >> 8) as u8);
            gate.write(base | GATE_BIT); // rising edge starts the count
                                         // OUT2 (bit 5) is LOW while counting, HIGH on expiry.
            while gate.read() & OUT2_MASK == 0 {
                core::hint::spin_loop();
            }
        }
    }
}
