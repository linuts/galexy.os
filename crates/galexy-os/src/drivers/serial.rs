//! Serial (COM1, 16550 UART) — debug output and the headless console.
//!
//! Transmit is the debug log plus the visible TTY's mirror (`console`).
//! Receive bytes are decoded into the active TTY's keyboard queue, so
//! `cargo run` can be typed from the host terminal. The UART lock is never
//! held while delivering a byte: delivery can log, and that log takes the
//! same lock.

use core::fmt;
use core::sync::atomic::{AtomicBool, Ordering};
use spin::{Mutex, Once};
use uart_16550::backend::PioBackend;
use uart_16550::spec::registers::{FifoTriggerLevel, IER};
use uart_16550::{Config, Uart16550};
use x86_64::instructions::interrupts;
use x86_64::structures::idt::InterruptStackFrame;

/// The first serial port (COM1, I/O base `0x3F8`).
static SERIAL1: Once<Mutex<Uart16550<PioBackend>>> = Once::new();

/// Returns the COM1 port, creating the handle on first use.
fn serial1() -> &'static Mutex<Uart16550<PioBackend>> {
    // SAFETY: port 0x3F8 is the standard COM1 base; valid for the whole run.
    SERIAL1.call_once(|| Mutex::new(unsafe { Uart16550::new_port(0x3F8).unwrap() }))
}

/// Initializes COM1 (8-N-1, FIFO on, receive interrupt armed).
///
/// The I/O APIC route is wired later (`arch::ioapic`); until then the
/// line can assert into a masked pin. [`drain_rx`] runs once that pin is
/// unmasked so a byte that arrived early is not stuck on a level that an
/// edge-triggered entry will never see rise.
pub fn init() {
    // One byte is enough to interrupt. A burst still drains in the handler;
    // the trigger only decides when the line rises.
    let config = Config {
        fifo_trigger_level: Some(FifoTriggerLevel::One),
        interrupts: IER::DATA_READY,
        ..Config::default()
    };
    // A missing device is not fatal: output is just lost.
    let _ = serial1().lock().init(config);
}

/// Blocking string write over COM1. Does not hold the UART: each chunk
/// goes through [`send_harvesting`], which also pulls any bytes that
/// arrived while transmit was busy.
struct SerialWriter;

impl fmt::Write for SerialWriter {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        send_harvesting(s.as_bytes());
        Ok(())
    }
}

/// Sends raw bytes over COM1 (byte-for-byte; used by the write syscall's
/// console mirror).
pub fn write_bytes(bytes: &[u8]) {
    send_harvesting(bytes);
}

/// Format-hook for the `serial_print!`/`serial_println!` macros.
#[doc(hidden)]
pub fn _print(args: fmt::Arguments) {
    use core::fmt::Write;

    SerialWriter
        .write_fmt(args)
        .expect("printing to serial failed");
}

/// COM1 receive interrupt (vector 36, ISA IRQ4). Drains the FIFO and
/// EOIs the LAPIC. Lock-free against the screen; the UART lock is dropped
/// before a byte is delivered to the keyboard queue.
pub(crate) extern "x86-interrupt" fn rx_handler(_frame: InterruptStackFrame) {
    drain_rx();
    crate::arch::apic::eoi();
}

/// Pulls every pending receive byte and delivers it to the keyboard.
pub fn drain_rx() {
    loop {
        let mut stash = [0u8; 32];
        let got = interrupts::without_interrupts(|| {
            let mut uart = serial1().lock();
            pull_rx(&mut uart, &mut stash)
        });
        if got == 0 {
            break;
        }
        deliver_bytes(&stash[..got]);
        if got < stash.len() {
            break;
        }
    }
}

/// The next `\n` is the second half of a `\r\n` and is not a second Enter.
static DROP_LF: AtomicBool = AtomicBool::new(false);

/// Transmits `bytes`, and while the transmitter FIFO is busy pulls any
/// receive bytes so a paste during a long log line cannot overrun the
/// 16-byte FIFO. The UART lock is not held across delivery.
fn send_harvesting(bytes: &[u8]) {
    let mut offset = 0;
    while offset < bytes.len() {
        let mut stash = [0u8; 32];
        let (sent, got) = interrupts::without_interrupts(|| {
            let mut uart = serial1().lock();
            let got = pull_rx(&mut uart, &mut stash);
            let sent = if got < stash.len() {
                uart.send_bytes(&bytes[offset..])
            } else {
                0
            };
            let more = pull_rx(&mut uart, &mut stash[got..]);
            (sent, got + more)
        });
        if got > 0 {
            deliver_bytes(&stash[..got]);
        }
        if sent == 0 {
            if got == 0 {
                core::hint::spin_loop();
            }
        } else {
            offset += sent;
        }
    }
}

/// Reads until the FIFO is empty or `dst` is full.
fn pull_rx(uart: &mut Uart16550<PioBackend>, dst: &mut [u8]) -> usize {
    let mut n = 0;
    while n < dst.len() {
        match uart.try_receive_byte() {
            Ok(b) => {
                dst[n] = b;
                n += 1;
            }
            Err(_) => break,
        }
    }
    n
}

fn deliver_bytes(bytes: &[u8]) {
    for &b in bytes {
        deliver_byte(b);
    }
}

/// One host byte becomes one keyboard character. `\r` is Enter (a raw
/// terminal sends CR); a following `\n` is swallowed so CRLF is one key.
/// DEL and BS are both backspace. Bytes above ASCII are dropped — the
/// line editor is ASCII.
fn deliver_byte(b: u8) {
    if b == b'\n' && DROP_LF.swap(false, Ordering::Relaxed) {
        return;
    }
    DROP_LF.store(false, Ordering::Relaxed);
    let c = match b {
        b'\r' => {
            DROP_LF.store(true, Ordering::Relaxed);
            '\n'
        }
        0x7f | 0x08 => '\u{8}',
        0x00 => return,
        byte if byte < 0x80 => char::from(byte),
        _ => return,
    };
    crate::drivers::keyboard::push_char(c);
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
        $crate::drivers::dmesg::record(format_args!(concat!("{}s: ", $fmt), secs));
        $crate::serial_print!(concat!("{}s: ", $fmt, "\n"), secs);
    }};
    ($fmt:expr, $($arg:tt)*) => {{
        let secs = $crate::arch::timer_ticks() / 1000;
        $crate::drivers::dmesg::record(format_args!(concat!("{}s: ", $fmt), secs, $($arg)*));
        $crate::serial_print!(concat!("{}s: ", $fmt, "\n"), secs, $($arg)*);
    }};
}
