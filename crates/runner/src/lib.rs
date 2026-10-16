//! Host-side helpers shared by `cargo run` and the boot tests.
//!
//! Image building stays in `build.rs`. QEMU command lines for the suite
//! stay in `tests/common`. Accelerator selection lives here so both
//! entry points apply the same rule.

mod accel;

pub use accel::{configure_accel, use_kvm, Accel, KvmUnavailable};

/// QEMU fw_cfg item the guest reads as "show kernel log lines on COM1".
///
/// Headless (`-nographic`) and `--display` both use that UART, so the
/// flag is the same in either mode. The suite passes it on every boot.
/// `cargo run` passes it only for `--debug`.
pub const KERNEL_LOG_FW_CFG: &str = "name=opt/galexy.debug,string=1";

/// Publishes [`KERNEL_LOG_FW_CFG`] so guest `serial_println!` reaches COM1.
pub fn enable_kernel_log(cmd: &mut std::process::Command) {
    cmd.arg("-fw_cfg").arg(KERNEL_LOG_FW_CFG);
}
