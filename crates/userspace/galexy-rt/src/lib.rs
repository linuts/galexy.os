//! galexy-rt: the ring-3 runtime.
//!
//! Everything a user program links against. Never the kernel — the only
//! contract with it is the syscall ABI (`galexy-abi`). Syscall wrappers
//! carry the register contract from `docs/DESIGN.md` (arch/syscall):
//! RAX = number in / value out, RDX = len-arg in / ok-flag out,
//! RDI/RSI = args. RCX and R11 are clobbered by the instruction itself.

#![no_std]
#![deny(clippy::all)]
#![deny(missing_docs)]

use core::arch::asm;
use core::panic::PanicInfo;

use galexy_abi::{Cap, CapRights, Syscall, SyscallResult};

/// The console capability as granted to user programs (WRITE right).
pub fn console_cap() -> Cap {
    galexy_abi::reserved::console(CapRights::WRITE)
}

/// One syscall: number + 3 args, register-form result back.
pub fn syscall(number: u64, a0: u64, a1: u64, a2: u64) -> SyscallResult {
    let value: u64;
    let ok: u64;
    // SAFETY: the kernel's syscall entry is uniform; rcx/r11 are clobbered
    // by the instruction itself per the x86_64 syscall ABI.
    unsafe {
        asm!(
            "syscall",
            inlateout("rax") number => value,
            inlateout("rdx") a2 => ok,
            in("rdi") a0,
            in("rsi") a1,
            lateout("rcx") _,
            lateout("r11") _,
            options(nostack),
        );
    }
    SyscallResult { ok: ok != 0, value }
}

/// Writes bytes to the console through the console capability.
pub fn write_console(bytes: &[u8]) -> SyscallResult {
    let cap = console_cap();
    syscall(
        Syscall::Write as u64,
        cap.bits(),
        bytes.as_ptr() as u64,
        bytes.len() as u64,
    )
}

/// Gives up the rest of the scheduling quantum.
pub fn yield_now() -> SyscallResult {
    syscall(Syscall::Yield as u64, 0, 0, 0)
}

/// Exits the calling task with `code` (never returns).
pub fn exit(code: u64) -> ! {
    let _ = syscall(Syscall::Exit as u64, code, 0, 0);
    // `exit` never returns; reaching here = a kernel bug. Park with
    // interrupts off so nothing wedges further.
    loop {
        // SAFETY: cli+hlt on the park path is the intended behavior.
        unsafe { asm!("cli", "hlt", options(nomem, nostack)) };
    }
}

/// Program entry: defines `_start`, runs the user's `main`, exits with its
/// return value.
#[macro_export]
macro_rules! entry {
    ($main:path) => {
        #[unsafe(no_mangle)]
        pub extern "C" fn _start() -> ! {
            let main: fn() -> i32 = $main;
            $crate::exit(main() as u64)
        }
    };
}

/// User panic: report through the console (best effort), die with code 1.
#[panic_handler]
fn panic(_info: &PanicInfo) -> ! {
    write_console(b"[user task panicked]\n");
    exit(1)
}
