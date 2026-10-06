//! Machine power: ACPI S5 shutdown and reset.
//!
//! Shutdown programs the FADT's PM1 control register with the `_S5_` sleep
//! type. Reset prefers the FADT reset register and otherwise pulses the
//! keyboard controller's CPU reset line (port `0x64`, command `0xFE`).
//!
//! Both requests are machine-wide. They run with interrupts off. If the
//! platform ignores them, the call returns and the shell can say so.

use x86_64::instructions::interrupts;
use x86_64::instructions::port::Port;

use crate::arch::acpi;
use crate::serial_println;

/// SLP_EN, bit 13 of the PM1 control register.
const SLP_EN: u16 = 1 << 13;

/// Turns the machine off. Returns if the platform is still running.
pub fn shutdown() {
    interrupts::disable();
    serial_println!("[power] shutdown");
    let Some(info) = acpi::power_info() else {
        serial_println!("[power] no FADT; cannot shut down");
        return;
    };
    enable_acpi(info);
    if info.has_s5 {
        let a = (u16::from(info.slp_typa) << 10) | SLP_EN;
        outw(info.pm1a_cnt, a);
        if info.pm1b_cnt != 0 {
            let b = (u16::from(info.slp_typb) << 10) | SLP_EN;
            outw(info.pm1b_cnt, b);
        }
    }
    // Still up: the PIIX4 port QEMU accepts when `_S5_` did not take.
    outw(0x604, SLP_EN);
    spin();
    serial_println!("[power] shutdown was ignored");
}

/// Resets the machine. Returns if the platform is still running.
pub fn reboot() {
    interrupts::disable();
    serial_println!("[power] reboot");
    if let Some(info) = acpi::power_info() {
        if info.has_reset {
            outb(info.reset_port, info.reset_value);
            spin();
        }
    }
    // Keyboard-controller pulse of the CPU reset line.
    // SAFETY: port 0x64 is the i8042 command port on every PC.
    let mut status = Port::<u8>::new(0x64);
    for _ in 0..100_000 {
        if unsafe { status.read() } & 0x02 == 0 {
            break;
        }
        io_delay();
    }
    outb(0x64, 0xFE);
    spin();
    serial_println!("[power] reboot was ignored");
}

fn enable_acpi(info: &acpi::PowerInfo) {
    if info.smi_cmd != 0 && info.acpi_enable != 0 {
        outb(info.smi_cmd, info.acpi_enable);
        io_delay();
    }
}

fn outb(port: u16, value: u8) {
    // SAFETY: callers pass fixed chipset ports (PM1, reset, 0x64, 0x80).
    let mut p = Port::<u8>::new(port);
    unsafe { p.write(value) };
}

fn outw(port: u16, value: u16) {
    // SAFETY: callers pass the FADT PM1 control port or the PIIX4 fallback.
    let mut p = Port::<u16>::new(port);
    unsafe { p.write(value) };
}

fn io_delay() {
    outb(0x80, 0);
}

/// Gives the chipset a moment to honor the request before we admit failure.
fn spin() {
    for _ in 0..100_000 {
        io_delay();
    }
}
