//! PIT timer: tick accounting and configuration.
//!
//! The PIT's remaining duties after the APIC-era delivery swap:
//! - channel 2 is the one-SHOT REFERENCE the LAPIC-timer calibration and
//!   these delay helpers use (BSP-only: global ports, global time),
//! - channel 0 keeps its legacy programming for symmetry (masked line).

use core::sync::atomic::{AtomicU64, Ordering};
use x86_64::instructions::port::Port;

/// Number of timer ticks since boot.
static TICKS: AtomicU64 = AtomicU64::new(0);

/// PIT crystal frequency in Hz.
const PIT_FREQ: u32 = 1_193_182;

/// Configures PIT channel 0 for ~1 kHz (required later by scheduling).
pub fn init() {
    const PIT_COMMAND_PORT: u16 = 0x43;
    const PIT_CHANNEL_0_DATA_PORT: u16 = 0x40;
    // 1.193182 MHz / divisor ≈ 1 kHz
    const DIVISOR: u16 = 1193;

    // SAFETY: fixed PIT ports; standard channel-0 square wave programming.
    unsafe {
        Port::new(PIT_COMMAND_PORT).write(0x36_u8); // channel 0, lo/hi, square wave
        Port::new(PIT_CHANNEL_0_DATA_PORT).write((DIVISOR & 0xFF) as u8);
        Port::new(PIT_CHANNEL_0_DATA_PORT).write((DIVISOR >> 8) as u8);
    }
}

/// Called by the timer interrupt handler for every tick.
pub fn tick() {
    let n = TICKS.fetch_add(1, Ordering::Relaxed) + 1;
    // One heartbeat per second on the debug channel, only.
    if n.is_multiple_of(1000) {
        crate::serial_println!("[timer] {}s up", n / 1000);
    }
}

/// Number of timer ticks since boot (monotonic, ~1 kHz resolution).
pub fn ticks() -> u64 {
    TICKS.load(Ordering::Relaxed)
}

/// Busy-waits `ms` milliseconds using a PIT channel-2 one-shot per
/// millisecond (gate ON, speaker OFF). BSP-only: these are GLOBAL ports and
/// a GLOBAL pit — only the boot-time flow (AP bring-up waits, LAPIC
/// calibration) may call before the scheduler runs.
///
/// Used with interrupts DISABLED by its callers (each 1 ms granule is
/// programmed + drained standalone, so virtual-clock under TCG is exact).
pub fn delay_ms(ms: u32) {
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
