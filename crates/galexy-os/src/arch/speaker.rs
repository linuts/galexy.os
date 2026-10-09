//! PC speaker (PIT channel 2 + port 0x61).
//!
//! The only audio device. There is no virtio-sound. Feature `pc-speaker`
//! is on by default; with it off, [`beep`] is silent. This is the only
//! PIT user after boot (channel 2 square wave). Calibration may still
//! program channel 2 once when CPUID 0x15 / 0x16 and the HPET are absent.
//!
//! BSP-only: the gate and PIT channel 2 are global. Callers must not nest
//! a beep inside LAPIC calibration.
//!
//! Duration is timed from PIT OUT2 edges so it works with IF=0 (SYSCALL
//! runs under SFMASK with interrupts masked — timer ticks will not advance).

#[cfg(feature = "pc-speaker")]
use x86_64::instructions::port::Port;

/// PIT crystal frequency in Hz.
#[cfg(feature = "pc-speaker")]
const PIT_FREQ: u32 = 1_193_182;
#[cfg(feature = "pc-speaker")]
const PIT_COMMAND_PORT: u16 = 0x43;
const PIT_CHANNEL_2_DATA_PORT: u16 = 0x42;
const PIT_GATE_PORT: u16 = 0x61;
/// Channel 2, lo/hi access, mode 3 (square wave).
const CH2_SQUARE_CMD: u8 = 0xB6;
const GATE_BIT: u8 = 0x1;
const SPEAKER_BIT: u8 = 0x2;
const OUT2_MASK: u8 = 0x20;

/// Default invalid-command pitch (Hz) and duration (ms).
const BEEP_HZ: u32 = 880;
const BEEP_MS: u32 = 80;

/// Short square-wave beep on the PC speaker.
pub fn beep() {
    #[cfg(feature = "pc-speaker")]
    beep_hz(BEEP_HZ, BEEP_MS);
}

/// Tone at `hz` for about `ms` milliseconds, then silence.
#[cfg(feature = "pc-speaker")]
pub fn beep_hz(hz: u32, ms: u32) {
    let hz = hz.clamp(37, 10_000);
    let divisor = (PIT_FREQ / hz) as u16;
    // Mode 3 toggles OUT2 twice per cycle → edges ≈ 2*hz per second.
    let edges = (2 * hz as u64 * ms as u64) / 1000;
    let edges = edges.max(1);
    // SAFETY: fixed PIT + speaker gate ports; delay helpers reprogram
    // channel 2 (mode 0) on their next call.
    unsafe {
        let mut cmd = Port::<u8>::new(PIT_COMMAND_PORT);
        let mut ch2 = Port::<u8>::new(PIT_CHANNEL_2_DATA_PORT);
        let mut gate = Port::<u8>::new(PIT_GATE_PORT);
        cmd.write(CH2_SQUARE_CMD);
        ch2.write((divisor & 0xFF) as u8);
        ch2.write((divisor >> 8) as u8);
        let prev = gate.read();
        gate.write(prev | GATE_BIT | SPEAKER_BIT);
        let mut seen = 0u64;
        let mut last = gate.read() & OUT2_MASK;
        while seen < edges {
            let bit = gate.read() & OUT2_MASK;
            if bit != last {
                seen += 1;
                last = bit;
            }
            core::hint::spin_loop();
        }
        let now = gate.read();
        gate.write(now & !(GATE_BIT | SPEAKER_BIT));
    }
}
