//! PIT timer: tick accounting and configuration.

use core::sync::atomic::{AtomicU64, Ordering};
use x86_64::instructions::port::Port;

/// Number of timer ticks since boot.
static TICKS: AtomicU64 = AtomicU64::new(0);

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
