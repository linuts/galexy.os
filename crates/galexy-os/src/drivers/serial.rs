//! Serial (COM1, 16550 UART) — debug output and the headless console.
//!
//! Transmit is the debug log plus the visible TTY's mirror (`console`).
//! Receive bytes are decoded into the active TTY's keyboard queue, so
//! `cargo run` can be typed from the host terminal. The UART lock is never
//! held while delivering a byte: delivery can log, and that log takes the
//! same lock.

use crate::sync::Mutex;
use core::fmt;
use core::sync::atomic::{AtomicBool, Ordering};
use spin::Once;
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

/// How many receive bytes one transmit will hold before it has to deliver
/// them. A formatted line keeps the UART lock the whole time (two CPUs
/// must not tear a line); this absorbs what arrives while that lock is
/// held. Anything beyond it stays in the FIFO until the lock drops and
/// the receive IRQ runs.
const RX_STASH: usize = 64;

/// Sends raw bytes over COM1 (byte-for-byte; used by the write syscall's
/// console mirror). One call is one contiguous burst.
pub fn write_bytes(bytes: &[u8]) {
    let mut stash = [0u8; RX_STASH];
    let got = interrupts::without_interrupts(|| {
        let mut uart = serial1().lock();
        let mut got = 0;
        send_locked(&mut uart, bytes, &mut stash, &mut got);
        harvest_rest(&mut uart, &mut stash, &mut got);
        got
    });
    if got > 0 {
        deliver_bytes(&stash[..got]);
    }
}

/// Format-hook for the `serial_print!`/`serial_println!` macros.
///
/// The whole formatted line is transmitted under one UART-lock hold, so a
/// second CPU's log cannot land in the middle of this one.
#[doc(hidden)]
pub fn _print(args: fmt::Arguments) {
    use core::fmt::Write;

    struct Line<'a> {
        uart: &'a mut Uart16550<PioBackend>,
        stash: &'a mut [u8],
        got: &'a mut usize,
    }

    impl fmt::Write for Line<'_> {
        fn write_str(&mut self, s: &str) -> fmt::Result {
            send_locked(self.uart, s.as_bytes(), self.stash, self.got);
            Ok(())
        }
    }

    let mut stash = [0u8; RX_STASH];
    let got = interrupts::without_interrupts(|| {
        let mut uart = serial1().lock();
        let mut got = 0;
        Line {
            uart: &mut uart,
            stash: &mut stash,
            got: &mut got,
        }
        .write_fmt(args)
        .expect("printing to serial failed");
        harvest_rest(&mut uart, &mut stash, &mut got);
        got
    });
    if got > 0 {
        deliver_bytes(&stash[..got]);
    }
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

/// Transmits `bytes` on a UART the caller already holds. Pulls receive
/// bytes into `stash` while the transmitter FIFO is busy.
fn send_locked(uart: &mut Uart16550<PioBackend>, bytes: &[u8], stash: &mut [u8], got: &mut usize) {
    let mut offset = 0;
    while offset < bytes.len() {
        harvest_rest(uart, stash, got);
        let n = uart.send_bytes(&bytes[offset..]);
        if n == 0 {
            core::hint::spin_loop();
        } else {
            offset += n;
        }
    }
}

/// Fills the free tail of `stash` from the receive FIFO.
fn harvest_rest(uart: &mut Uart16550<PioBackend>, stash: &mut [u8], got: &mut usize) {
    if *got < stash.len() {
        *got += pull_rx(uart, &mut stash[*got..]);
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
