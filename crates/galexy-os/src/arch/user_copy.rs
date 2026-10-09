//! The only `stac` / `clac` site.
//!
//! With `CR4.SMAP` set, a ring-0 load or store of a user virtual address
//! faults unless `EFLAGS.AC` is set. Syscall staging copies that touch a
//! user VA go through this module so the flag is raised for the copy and
//! cleared before the function returns, including on unwind.
//!
//! Copies that walk the physical map (`sched::copy_to_user_via` /
//! `copy_from_user_via`) address kernel mappings of the same frames and
//! do not come through here.
//!
//! `stac` / `clac` are `#UD` when `CR4.SMAP` is clear, so a CPU that did
//! not report the feature (or a boot before [`crate::arch::cpu::init_percpu`])
//! copies without them.

use core::arch::asm;

/// `true` once any CPU has set `CR4.SMAP`. The feature is uniform across
/// the package; each CPU still sets its own CR4 bit in `init_percpu`.
static SMAP_ON: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

/// Records that this CPU enabled SMAP. Called from per-CPU bring-up.
pub(crate) fn note_smap_enabled() {
    SMAP_ON.store(true, core::sync::atomic::Ordering::Release);
}

/// Whether supervisor access to user pages is fenced.
pub fn smap_enabled() -> bool {
    SMAP_ON.load(core::sync::atomic::Ordering::Acquire)
}

/// Raises `EFLAGS.AC` for the duration of a copy when SMAP is on.
struct StacGuard;

impl StacGuard {
    fn new() -> Self {
        if smap_enabled() {
            // SAFETY: CR4.SMAP is set on this CPU (the flag is published
            // only after that write). `stac` touches no memory.
            unsafe { asm!("stac", options(nostack, nomem)) };
        }
        Self
    }
}

impl Drop for StacGuard {
    fn drop(&mut self) {
        if smap_enabled() {
            // SAFETY: paired with the `stac` in `new` on this same CPU.
            // Not re-entrant: a nested copy would clear AC early.
            unsafe { asm!("clac", options(nostack, nomem)) };
        }
    }
}

/// Copies `len` bytes from user virtual address `src` into `dst`.
///
/// # Safety
///
/// `user_buffer` (or an equivalent walk) has accepted every byte of
/// `[src, src+len)` as present and user-accessible in the active tree.
/// `dst` is a kernel buffer of at least `len` bytes, exclusively owned.
/// Not called re-entrantly on the same CPU.
pub unsafe fn copy_from_user(src: u64, dst: *mut u8, len: usize) {
    if len == 0 {
        return;
    }
    let _guard = StacGuard::new();
    // SAFETY: caller contract; AC is set when SMAP is on.
    unsafe { core::ptr::copy_nonoverlapping(src as *const u8, dst, len) };
}

/// Copies `len` bytes from kernel `src` to user virtual address `dst`.
///
/// # Safety
///
/// `user_buffer` has accepted `[dst, dst+len)` as present, user-accessible,
/// and writable. `src` has at least `len` bytes. Not re-entrant on this CPU.
pub unsafe fn copy_to_user(src: *const u8, dst: u64, len: usize) {
    if len == 0 {
        return;
    }
    let _guard = StacGuard::new();
    // SAFETY: caller contract; AC is set when SMAP is on.
    unsafe { core::ptr::copy_nonoverlapping(src, dst as *mut u8, len) };
}
