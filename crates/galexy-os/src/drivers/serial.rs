//! Serial (COM1, 16550 UART) debug output — a hardware driver speaking
//! through the `arch` port wall via `uart_16550`.
//!
//! Serial is the debugging side channel: user-facing output never goes here.
//! Because the serial writer shares no lock with the VGA writer, interrupt
//! handlers can safely log through it.

use core::fmt;
use spin::{Mutex, Once};
use uart_16550::backend::PioBackend;
use uart_16550::{Config, Uart16550};

/// The first serial port (COM1, I/O base `0x3F8`).
static SERIAL1: Once<Mutex<Uart16550<PioBackend>>> = Once::new();

/// Returns the COM1 port, creating the handle on first use.
fn serial1() -> &'static Mutex<Uart16550<PioBackend>> {
    // SAFETY: port 0x3F8 is the standard COM1 base; valid for the whole run.
    SERIAL1.call_once(|| Mutex::new(unsafe { Uart16550::new_port(0x3F8).unwrap() }))
}

/// Initializes COM1 (configures baud rate, line control).
pub fn init() {
    // A missing device is not fatal: output is just lost.
    let _ = serial1().lock().init(Config::default());
}

/// Blocking string write over COM1.
struct SerialWriter<'a>(&'a mut Uart16550<PioBackend>);

impl fmt::Write for SerialWriter<'_> {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        self.0.send_bytes_exact(s.as_bytes());
        Ok(())
    }
}

/// Sends raw bytes over COM1 (byte-for-byte; used by the write syscall's
/// console mirror).
pub fn write_bytes(bytes: &[u8]) {
    use x86_64::instructions::interrupts;

    interrupts::without_interrupts(|| {
        serial1().lock().send_bytes_exact(bytes);
    });
}

/// Format-hook for the `serial_print!`/`serial_println!` macros.
#[doc(hidden)]
pub fn _print(args: fmt::Arguments) {
    use core::fmt::Write;
    use x86_64::instructions::interrupts;

    interrupts::without_interrupts(|| {
        let mut uart = serial1().lock();
        SerialWriter(&mut uart)
            .write_fmt(args)
            .expect("printing to serial failed");
    });
}

/// Prints to the host console through COM1 (visible with QEMU's `-serial stdio`).
#[macro_export]
macro_rules! serial_print {
    ($($arg:tt)*) => {
        $crate::drivers::serial::_print(format_args!($($arg)*))
    };
}

/// Prints to the host console through COM1, with a newline.
///
/// Prefixed with monotonic uptime (`3s: …`) from the LAPIC/PIT tick
/// counter so second-heartbeats are unnecessary and log lines stay
/// ordered without a separate `[timer]` spam stream.
#[macro_export]
macro_rules! serial_println {
    () => ($crate::serial_print!("\n"));
    ($fmt:expr) => {{
        let secs = $crate::arch::timer_ticks() / 1000;
        $crate::serial_print!(concat!("{}s: ", $fmt, "\n"), secs);
    }};
    ($fmt:expr, $($arg:tt)*) => {{
        let secs = $crate::arch::timer_ticks() / 1000;
        $crate::serial_print!(concat!("{}s: ", $fmt, "\n"), secs, $($arg)*);
    }};
}
